//! Connection lifetimes and safe recovery for one serialized TV worker.

use std::{future::pending, time::Duration};

use futures_util::FutureExt;
use lgtv_companion_core::{streaming::TvPower, webos::Request};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep_until};

use crate::{
    batch::Action,
    config::{ConnectionMode, Device},
    pairing::Store,
    tv::{ExchangeError, Result, Tv},
};

use super::{failed, run_action, ssap_output, tv_power, wake};

const KEEPALIVE: Duration = Duration::from_secs(60);
const POWER_STATE: &str = "com.webos.service.tvpower/power/getPowerState";
const POWER_OFF: &str = "ssap://system/turnOff";
const BLANK: &str = "ssap://com.webos.service.tvpower/power/turnOffScreen";
const UNBLANK: &str = "ssap://com.webos.service.tvpower/power/turnOnScreen";

struct Credentials {
    key: String,
    certificate: Option<Vec<u8>>,
}

impl Credentials {
    fn load(device: &Device, store: &Store) -> Result<Self> {
        Ok(Self {
            key: store.load(&device.id)?,
            certificate: device.certificate()?,
        })
    }
}

struct Registered {
    tv: Tv,
    credentials: Credentials,
    keepalive_at: Instant,
}

pub struct Connection {
    mode: ConnectionMode,
    cached: Option<Registered>,
}

impl Connection {
    pub fn new(mode: ConnectionMode) -> Self {
        Self { mode, cached: None }
    }

    pub fn reset(&mut self) {
        self.cached = None;
    }

    pub async fn execute(
        &mut self,
        device: &Device,
        store: &Store,
        action: &Action,
        wake_seconds: u64,
        mut capture: impl FnMut(TvPower, bool) + Send,
    ) -> Result<Value> {
        // Checkout before awaiting. Cancelling an in-flight exchange drops the
        // socket and prevents its late reply from leaking into the next job.
        let cached = self.cached.take();
        let credentials = Credentials::load(device, store)?;
        let existing = cached
            .filter(|entry| same_credentials(&entry.credentials, &credentials))
            .and_then(|entry| ready(entry.tv));

        if matches!(action, Action::Wake) {
            let (result, tv) = wake(
                device,
                credentials.certificate.clone(),
                &credentials.key,
                wake_seconds,
                existing,
            )
            .await?;
            if !failed(&result) {
                self.save(tv, credentials);
            }
            return Ok(ssap_output(result));
        }

        let mut tv = match existing {
            Some(tv) => tv,
            None => register(device, &credentials.certificate, &credentials.key).await?,
        };

        let result = run_action(
            &mut tv,
            device,
            credentials.certificate.as_deref(),
            action,
            &mut capture,
        )
        .await;
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                let Some(exchange) = error.downcast_ref::<ExchangeError>() else {
                    return Err(error);
                };
                if retryable_query(action) {
                    drop(tv);
                    tv = register(device, &credentials.certificate, &credentials.key).await?;
                    run_action(
                        &mut tv,
                        device,
                        credentials.certificate.as_deref(),
                        action,
                        &mut capture,
                    )
                    .await?
                } else if let Some((expected, on_confirm)) = requested_power(action, &exchange.uri)
                {
                    drop(tv);
                    if let Ok(mut fresh) =
                        register(device, &credentials.certificate, &credentials.key).await
                        && let Ok(power) = query_power(&mut fresh).await
                        && !failed(&power)
                        && power["state"]
                            .as_str()
                            .is_some_and(|state| tv_power(state) == expected)
                    {
                        if matches!(action, Action::StreamPowerOff) {
                            // The first observation was captured before the write.
                            capture(TvPower::Unknown, true);
                        }
                        if expected != TvPower::Standby {
                            self.save(fresh, credentials);
                        }
                        return Ok(on_confirm(power));
                    }
                    return Err(error);
                } else {
                    return Err(error);
                }
            }
        };
        if !closes_socket(action, &result) {
            self.save(tv, credentials);
        }
        Ok(result)
    }

    fn save(&mut self, tv: Tv, credentials: Credentials) {
        if !self.mode.is_off() {
            self.cached = Some(Registered {
                tv,
                credentials,
                keepalive_at: Instant::now() + KEEPALIVE,
            });
        }
    }

    /// Watch a registered idle socket for close/ping or run its scheduled probe.
    /// A keepalive only uses an already connected TV and never initiates a wake.
    pub async fn idle(&mut self, device: &Device, store: &Store) {
        let Some(entry) = self.cached.as_mut() else {
            return pending().await;
        };
        if !matches!(self.mode, ConnectionMode::Keepalive) {
            if entry.tv.idle_message().await.is_err() {
                self.reset();
            }
            return;
        }

        tokio::select! {
            _ = sleep_until(entry.keepalive_at) => {}
            traffic = entry.tv.idle_message() => {
                if traffic.is_err() {
                    self.reset();
                }
                return;
            }
        }
        // A new command may cancel this idle future. If the probe started,
        // checkout guarantees the interrupted request's socket is discarded.
        let mut entry = self.cached.take().expect("registered idle socket");
        if Credentials::load(device, store)
            .is_ok_and(|current| same_credentials(&current, &entry.credentials))
            && let Ok(reply) = query_power(&mut entry.tv).await
            && !failed(&reply)
            && matches!(reply["state"].as_str(), Some("Active" | "Screen Off"))
        {
            self.save(entry.tv, entry.credentials);
        }
    }
}

fn same_credentials(a: &Credentials, b: &Credentials) -> bool {
    a.key == b.key && a.certificate == b.certificate
}

fn ready(mut tv: Tv) -> Option<Tv> {
    for _ in 0..16 {
        match tv.idle_message().now_or_never() {
            Some(Ok(())) => {}
            Some(Err(_)) => return None,
            None => break,
        }
    }
    Some(tv)
}

pub(super) async fn register(
    device: &Device,
    certificate: &Option<Vec<u8>>,
    key: &str,
) -> Result<Tv> {
    let mut tv = Tv::connect(device.address(), device.source_ip, certificate.clone()).await?;
    tv.register(Some(key)).await?;
    Ok(tv)
}

pub(super) async fn query_power(tv: &mut Tv) -> Result<Value> {
    tv.request(Request::new("", POWER_STATE, Some(json!({}))))
        .await
}

fn retryable_query(action: &Action) -> bool {
    matches!(action, Action::Ssap(request) if matches!(
        request.uri.as_str(),
        "ssap://audio/getVolume"
            | "ssap://audio/getStatus"
            | "ssap://settings/getSystemSettings"
            | "ssap://system/getSystemInfo"
            | "ssap://com.webos.applicationManager/getForegroundAppInfo"
    ) || request.uri.strip_prefix("ssap://") == Some(POWER_STATE))
}

type Confirmation = fn(Value) -> Value;

fn requested_power(action: &Action, uri: &str) -> Option<(TvPower, Confirmation)> {
    let expected = match (action, uri) {
        (Action::Ssap(_) | Action::HostPowerOff | Action::StreamPowerOff, POWER_OFF) => {
            TvPower::Standby
        }
        (Action::Ssap(_) | Action::IdleBlank | Action::StreamBlank, BLANK) => TvPower::ScreenOff,
        (Action::Ssap(_) | Action::IdleRestore, UNBLANK) => TvPower::Active,
        _ => return None,
    };
    let output: Confirmation = if matches!(action, Action::Ssap(_)) {
        |mut power| {
            power["returnValue"] = json!(true);
            power["verified_after_reconnect"] = json!(true);
            ssap_output(power)
        }
    } else {
        |_| json!({"info": "TV state confirmed after reconnect", "applied": true})
    };
    Some((expected, output))
}

fn closes_socket(action: &Action, result: &Value) -> bool {
    match action {
        Action::Ssap(request) if request.uri == POWER_OFF => !failed(result),
        Action::HostPowerOff | Action::StreamPowerOff => result["applied"] == true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::*;

    #[tokio::test]
    async fn keepalive_reuses_the_socket_and_discards_it_after_an_invalid_probe() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let device: Device = serde_json::from_value(json!({
            "id":"Device1", "ip":"127.0.0.1",
            "port":listener.local_addr().unwrap().port(),
            "transport":"ws", "connection_mode":"keepalive"
        }))
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("pairing")).unwrap();
        store.save("Device1", "mock-key").unwrap();

        let mut connection = Connection::new(device.connection_mode);
        let action = Action::Ssap(Request::new("", "audio/getVolume", None));
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(tcp).await.unwrap();
            for (uri, response) in [
                (None, json!({"type":"registered"})),
                (
                    Some("ssap://audio/getVolume"),
                    json!({"type":"response","payload":{"volume":5}}),
                ),
                (
                    Some("ssap://com.webos.service.tvpower/power/getPowerState"),
                    json!({"type":"response","payload":{"state":"Active"}}),
                ),
                (
                    Some("ssap://com.webos.service.tvpower/power/getPowerState"),
                    json!({"type":"response","payload":null}),
                ),
            ] {
                let request: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["uri"].as_str(), uri);
                let mut response = response;
                response["id"] = request["id"].clone();
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            assert!(socket.next().await.is_none_or(|result| result.is_err()));
        };
        let client = async {
            let result = connection
                .execute(&device, &store, &action, 5, |_, _| {})
                .await
                .unwrap();
            assert_eq!(result["payload"]["volume"], 5);

            for expect_cached in [true, false] {
                let entry = connection.cached.as_mut().unwrap();
                assert!(entry.keepalive_at > Instant::now());
                entry.keepalive_at = Instant::now();
                connection.idle(&device, &store).await;
                assert_eq!(connection.cached.is_some(), expect_cached);
            }
        };
        timeout(Duration::from_secs(5), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    }
}

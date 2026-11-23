mod connection;

pub use connection::Connection;

use std::{
    env, io,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    time::Duration,
};

use lgtv_companion_core::{
    streaming::TvPower,
    webos::{Request, setting_request},
    wol,
};
use serde_json::{Value, json};
use tokio::{
    net::UdpSocket,
    time::{self, MissedTickBehavior},
};

use crate::{
    batch::Action,
    config::{Device, HostPower, WakeMode, parse_mac},
    pairing::Store,
    tv::{ExchangeError, Result, Tv},
};

pub fn default_state_dir() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|path| path.join("lgtv-companion/pairing"))
}

pub fn failed(result: &Value) -> bool {
    result.get("error").is_some()
        || result["returnValue"] == false
        || result["payload"]["returnValue"] == false
}

pub async fn pair(device: &Device, store: &Store, on_prompt: impl FnOnce()) -> Result<Value> {
    let _lock = store.lock_pairing(&device.id)?;
    let mut tv = Tv::connect(device.address(), device.source_ip, device.certificate()?).await?;
    on_prompt();
    let key = tv.register(None).await?.ok_or("Missing pairing key")?;
    store.save(&device.id, &key)?;
    let wol = tv
        .luna(setting_request("", "wol", "true").expect("Windows wake-on-LAN command"))
        .await;
    let enabled = wol.is_ok_and(|result| !failed(&result));
    Ok(json!({"paired": true, "wake_on_lan_request_accepted": enabled}))
}

pub async fn execute(
    device: &Device,
    store: &Store,
    action: &Action,
    wake_seconds: u64,
) -> Result<Value> {
    Connection::new(crate::config::ConnectionMode::Off)
        .execute(device, store, action, wake_seconds, |_, _| {})
        .await
}

async fn run_action(
    tv: &mut Tv,
    device: &Device,
    certificate: Option<&[u8]>,
    action: &Action,
    capture: impl FnMut(TvPower, bool) + Send,
) -> Result<Value> {
    match action {
        Action::Ssap(request) => tv.request(request.clone()).await.map(ssap_output),
        Action::Luna(request) => tv.luna(request.clone()).await,
        Action::LunaSequence(requests) => {
            let mut result = Value::Null;
            for request in requests {
                result = tv.luna(request.clone()).await?;
                if failed(&result) {
                    break;
                }
            }
            Ok(result)
        }
        Action::Button(name) => tv
            .button(
                device.address(),
                device.source_ip,
                certificate.map(<[u8]>::to_vec),
                name,
            )
            .await
            .map(|reply| {
                if reply["returnValue"] == false {
                    ssap_output(reply)
                } else {
                    reply
                }
            }),
        Action::HostPowerOff
        | Action::IdleBlank
        | Action::IdleRestore
        | Action::StreamPowerOff
        | Action::StreamBlank => {
            let policy = device
                .host_power
                .as_ref()
                .ok_or("Host power automation is disabled")?;
            automatic_display_change(tv, policy, action, capture).await
        }
        Action::SetAutomation(_) => {
            Err("Automatic management requires the Companion daemon".into())
        }
        Action::Wake => unreachable!(),
    }
}

async fn automatic_display_change(
    tv: &mut Tv,
    policy: &HostPower,
    action: &Action,
    mut capture: impl FnMut(TvPower, bool),
) -> Result<Value> {
    let power = connection::query_power(tv).await?;
    if failed(&power) {
        return Err("Unable to read TV power state".into());
    }
    let state = power["state"]
        .as_str()
        .ok_or("TV returned no power state")?;
    let before = tv_power(state);
    if matches!(action, Action::StreamPowerOff) {
        capture(before, false);
    }
    let (endpoint, accepted) = match action {
        Action::HostPowerOff | Action::StreamPowerOff
            if matches!(state, "Active" | "Screen Off") =>
        {
            ("system/turnOff", "TV acknowledged power-off")
        }
        Action::IdleBlank | Action::StreamBlank if state == "Active" => (
            "com.webos.service.tvpower/power/turnOffScreen",
            if matches!(action, Action::IdleBlank) {
                "TV acknowledged idle blanking"
            } else {
                "TV acknowledged stream blanking"
            },
        ),
        Action::IdleRestore if state == "Screen Off" => (
            "com.webos.service.tvpower/power/turnOnScreen",
            "TV acknowledged idle restore",
        ),
        _ => return Ok(json!({"info": "TV power state does not require this action"})),
    };
    if let Some(input) = policy.hdmi_input {
        let foreground = tv
            .request(Request::new(
                "",
                "com.webos.applicationManager/getForegroundAppInfo",
                Some(json!({})),
            ))
            .await?;
        let expected = format!("com.webos.app.hdmi{input}");
        if failed(&foreground) || foreground["appId"].as_str() != Some(&expected) {
            return Ok(json!({"info": "Configured HDMI input could not be confirmed"}));
        }
    }
    let reply = tv
        .request(Request::new("", endpoint, Some(json!({}))))
        .await?;
    if failed(&reply) {
        Err("TV rejected the automatic display request".into())
    } else {
        if policy.mute_speakers
            && matches!(
                action,
                Action::IdleBlank | Action::StreamBlank | Action::IdleRestore
            )
        {
            let mute = matches!(action, Action::IdleBlank | Action::StreamBlank);
            let audio = tv
                .request(Request::new(
                    "",
                    "audio/setMute",
                    Some(json!({"mute": mute})),
                ))
                .await?;
            if failed(&audio) {
                return Err(
                    "TV changed its screen state but rejected the speaker mute request".into(),
                );
            }
        }
        if matches!(action, Action::StreamPowerOff)
            && let Ok(observed) = tv
                .request(Request::new(
                    "",
                    "com.webos.service.tvpower/power/getPowerState",
                    Some(json!({})),
                ))
                .await
            && !failed(&observed)
            && observed["state"]
                .as_str()
                .is_some_and(|state| tv_power(state) == TvPower::Standby)
        {
            capture(before, true);
        }
        Ok(json!({"info": accepted, "applied": true}))
    }
}

fn tv_power(state: &str) -> TvPower {
    match state {
        "Active" => TvPower::Active,
        "Screen Off" => TvPower::ScreenOff,
        "Standby" | "Power Off" => TvPower::Standby,
        _ => TvPower::Unknown,
    }
}

fn ssap_output(payload: Value) -> Value {
    if payload.get("error").is_some() {
        payload
    } else {
        json!({"payload": payload})
    }
}

async fn wake(
    device: &Device,
    certificate: Option<Vec<u8>>,
    key: &str,
    seconds: u64,
    mut connected: Option<Tv>,
) -> Result<(Value, Tv)> {
    let macs: Vec<_> = device
        .macs
        .iter()
        .filter_map(|mac| parse_mac(mac))
        .collect();
    let packets: Vec<_> = macs.into_iter().map(wol::magic_packet).collect();
    let mut destinations = Vec::new();
    if matches!(
        device.wake_mode,
        WakeMode::LimitedBroadcast | WakeMode::Automatic
    ) {
        destinations.push(Ipv4Addr::BROADCAST);
    }
    if matches!(device.wake_mode, WakeMode::Direct | WakeMode::Automatic) {
        destinations.push(device.ip);
    }
    if matches!(
        device.wake_mode,
        WakeMode::SubnetBroadcast | WakeMode::Automatic
    ) {
        if let Some(mask) = device.netmask {
            destinations.push(wol::subnet_broadcast(device.ip, mask));
        } else if matches!(device.wake_mode, WakeMode::SubnetBroadcast) {
            return Err("Subnet broadcast wake requires an explicit netmask".into());
        }
    }
    destinations.sort_unstable();
    destinations.dedup();

    let deadline = Duration::from_secs(seconds);
    let attempt = async {
        let mut unblank_sent = false;
        loop {
            let mut tv = match connected.take() {
                Some(tv) => tv,
                None => match connection::register(device, &certificate, key).await {
                    Ok(tv) => tv,
                    Err(error) if error.is::<io::Error>() || error.is::<time::error::Elapsed>() => {
                        time::sleep(wol::REPEAT_INTERVAL).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                },
            };
            loop {
                let power = match connection::query_power(&mut tv).await {
                    Ok(power) => power,
                    Err(error) if error.is::<ExchangeError>() => break,
                    Err(error) => return Err(error),
                };
                match power["state"].as_str() {
                    Some("Active") if power["processing"].is_null() => return Ok((power, tv)),
                    Some("Screen Off") if !unblank_sent => {
                        unblank_sent = true;
                        match tv
                            .request(Request::new(
                                "",
                                "com.webos.service.tvpower/power/turnOnScreen",
                                Some(json!({})),
                            ))
                            .await
                        {
                            Ok(result) if failed(&result) => return Ok((result, tv)),
                            Ok(_) => {}
                            Err(error) if error.is::<ExchangeError>() => break,
                            Err(error) => return Err(error),
                        }
                    }
                    None if failed(&power) => return Ok((power, tv)),
                    None => return Err("TV returned no power state".into()),
                    _ => {}
                }
                time::sleep(wol::REPEAT_INTERVAL).await;
            }
            time::sleep(wol::REPEAT_INTERVAL).await;
        }
    };

    if packets.is_empty() {
        // An already reachable TV can still be unblanked without a MAC.
        return time::timeout(deadline, attempt).await?;
    }

    let source_ip = device.source_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let socket = UdpSocket::bind(SocketAddrV4::new(source_ip, 0))
        .await
        .map_err(|error| format!("Cannot bind the Wake-on-LAN socket to {source_ip}: {error}"))?;
    socket.set_broadcast(true)?;
    let send_packets = async {
        let mut interval = time::interval(wol::REPEAT_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut sent = false;
            for mac in &packets {
                for ip in &destinations {
                    sent |= socket
                        .send_to(mac, SocketAddrV4::new(*ip, wol::PORT))
                        .await
                        .is_ok();
                }
            }
            if !sent {
                return Err("Could not send a Wake-on-LAN packet".into());
            }
        }
    };
    time::timeout(deadline, async {
        tokio::select! {
            result = attempt => result,
            result = send_packets => result,
        }
    })
    .await?
}

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

const TEST_CERT: &[u8] = include_bytes!("fixtures/mock-tv.pem");
const TEST_KEY: &[u8] = include_bytes!("fixtures/mock-tv-key.pem");
const PAIRING_KEY: &str = "test-key-for-local-mock";

fn cli(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lgtvcli"));
    cmd.arg("--config").arg(dir.join("devices.json"));
    cmd.arg("--state-dir").arg(dir.join("pairing"));
    cmd.args(args);
    cmd
}

async fn run(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

async fn receive(socket: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>) -> Value {
    let message = timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

async fn send(socket: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

async fn reply(
    socket: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>,
    request: &Value,
    payload: Value,
) {
    send(
        socket,
        json!({"id": request["id"], "type": "response", "payload": payload}),
    )
    .await;
}

#[test]
fn pairing_reports_the_resolved_path_of_a_missing_certificate() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("devices.json"),
        r#"{"devices":[{"id":"Device1","ip":"192.0.2.10","certificate":"office-tv.der"}]}"#,
    )
    .unwrap();

    let result = cli(temp.path(), &["--pair", "Device1"]).output().unwrap();
    assert_eq!(result.status.code(), Some(1));
    let output: Value = serde_json::from_slice(&result.stdout).unwrap();
    let message = output["Device1"]["error"].as_str().unwrap();
    assert!(message.contains("Cannot read the WSS certificate"));
    assert!(message.contains(&*temp.path().join("office-tv.der").to_string_lossy()));
}

#[tokio::test]
async fn pinned_wss_pairs_persists_and_reuses_key_across_invocations() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let temp = tempfile::tempdir().unwrap();
    let certificate = CertificateDer::from_pem_slice(TEST_CERT).unwrap();
    fs::write(temp.path().join("tv.der"), &certificate).unwrap();
    fs::write(temp.path().join("devices.json"), json!({
        "devices": [{"id":"Device1", "name":"Écran - 1", "ip":"127.0.0.1", "port":port, "certificate":"tv.der"}]
    }).to_string()).unwrap();

    let tls = TlsAcceptor::from(Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate],
                PrivateKeyDer::from_pem_slice(TEST_KEY).unwrap(),
            )
            .unwrap(),
    ));
    let mock = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(tls.accept(tcp).await.unwrap()).await.unwrap();
        let register = receive(&mut socket).await;
        assert_eq!(register["id"], "register_0");
        assert!(register["payload"].get("client-key").is_none());
        reply(&mut socket, &register, json!({"pairingType":"PROMPT"})).await;
        send(
            &mut socket,
            json!({"id":"register_0", "type":"registered","payload":{"client-key": PAIRING_KEY}}),
        )
        .await;

        let enable = receive(&mut socket).await;
        assert_eq!(enable["uri"], "ssap://system.notifications/createAlert");
        assert_eq!(
            enable["payload"]["onclose"]["params"]["settings"]["wolwowlOnOff"],
            "true"
        );
        reply(
            &mut socket,
            &enable,
            json!({"alertId":"mock-42", "returnValue":true}),
        )
        .await;
        let close = receive(&mut socket).await;
        assert_ne!(enable["id"], close["id"]);
        assert_eq!(
            close["payload"]["alertId"],
            "com.webos.service.apiadapter-42"
        );
        reply(&mut socket, &close, json!({"returnValue":true})).await;

        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(tls.accept(tcp).await.unwrap()).await.unwrap();
        let register = receive(&mut socket).await;
        assert_eq!(register["payload"]["client-key"], PAIRING_KEY);
        send(
            &mut socket,
            json!({"id":"register_0", "type":"registered", "payload":{}}),
        )
        .await;
        let query = receive(&mut socket).await;
        assert_eq!(query["uri"], "ssap://settings/getSystemSettings");
        assert_eq!(
            query["payload"],
            json!({"category":"picture","keys":["backlight"]})
        );
        send(
            &mut socket,
            json!({"id":"unrelated", "type":"response", "payload":{"backlight":0}}),
        )
        .await;
        reply(
            &mut socket,
            &query,
            json!({"backlight":42, "returnValue":true}),
        )
        .await;

        // A replaced pin must fail during TLS, before it can disclose a key.
        let (tcp, _) = listener.accept().await.unwrap();
        assert!(tls.accept(tcp).await.is_err());
    });

    let result = run(cli(temp.path(), &["--pair"])).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap(),
        json!({"Device1":{"paired":true,"wake_on_lan_request_accepted":true}})
    );
    assert!(!String::from_utf8_lossy(&result.stderr).contains(PAIRING_KEY));
    let key = temp.path().join("pairing/device1.key");
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let result = run(cli(
        temp.path(),
        &[
            "-ok",
            "backlight",
            "-GET_SYSTEM_SETTINGS",
            "picture",
            "[\"backlight\"]",
            "éCRAN - 1",
        ],
    ))
    .await;
    assert!(result.status.success());
    assert_eq!(result.stdout, b"42\n");

    fs::write(temp.path().join("tv.der"), b"wrong pin").unwrap();
    let result = run(cli(temp.path(), &["-request", "audio/getVolume"])).await;
    assert_eq!(result.status.code(), Some(1));
    assert!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["Device1"]["error"].is_string()
    );
    timeout(Duration::from_secs(5), mock)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn windows_import_preview_preserves_legacy_defaults_without_leaking_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("config.json");
    fs::write(&file, json!({
        "LGTV Companion": {"Version":2,"PowerOnTimeOut":150},
        "Device2": {"IP":"192.0.2.45", "Name":"[LG] webOS TV Display", "WOL":2, "MAC":["02-01-02-03-04-05","02:01:02:03:04:05"],"SessionKey":"private-key","NicLuid":21,"PersistentConnectionLevel":2}
    }).to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lgtvcli"))
        .arg("--import-windows")
        .arg(&file)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-key"));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["preview"]["wake_timeout_seconds"], 100);
    let device = &result["preview"]["devices"][0];
    assert_eq!(device["id"], "Device2");
    assert_eq!(device["name"], "Display");
    assert_eq!(device["wake_mode"], "automatic");
    assert_eq!(device["connection_mode"], "keepalive");
    assert_eq!(device["macs"], json!(["02:01:02:03:04:05"]));
    assert!(result["warnings"].is_array());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn plaintext_batch_keeps_command_order_and_reports_partial_failure() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("devices.json"),
        json!({
            "devices":[{"id":"Device1","ip":"127.0.0.1","transport":"ws","port":port}]
        })
        .to_string(),
    )
    .unwrap();
    let state = temp.path().join("pairing");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let key_path = state.join("device1.key");
    fs::write(&key_path, PAIRING_KEY).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();

    let mock = tokio::spawn(async move {
        // An explicit re-pair that the TV rejects must preserve the saved key.
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        let register = receive(&mut socket).await;
        assert!(register["payload"].get("client-key").is_none());
        send(&mut socket, json!({"id":"register_0","type":"error"})).await;

        for (uri, payload) in [
            ("ssap://audio/setVolume", Some(json!({"volume":0}))),
            (
                "ssap://system.launcher/launch",
                Some(json!({"id":"com.webos.app.hdmi2"})),
            ),
            (
                "ssap://com.webos.service.networkinput/getPointerInputSocket",
                None,
            ),
        ] {
            let mut socket = accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            let register = receive(&mut socket).await;
            assert_eq!(register["payload"]["client-key"], PAIRING_KEY);
            send(&mut socket, json!({"id":"register_0","type":"registered"})).await;
            let request = receive(&mut socket).await;
            assert_eq!(request["uri"], uri);
            assert_eq!(request.get("payload"), payload.as_ref());
            match uri {
                "ssap://audio/setVolume" => reply(&mut socket, &request, json!({"returnValue":true})).await,
                "ssap://system.launcher/launch" => send(&mut socket, json!({"id":request["id"],"type":"error","error":"Mock TV rejected HDMI switch"})).await,
                _ => {
                    reply(&mut socket, &request, json!({"returnValue":true,"socketPath":"ws://192.0.2.99:9999/resources/mock"})).await;
                    let mut pointer = accept_async(listener.accept().await.unwrap().0).await.unwrap();
                    let message = pointer.next().await.unwrap().unwrap();
                    assert_eq!(message.to_text().unwrap(), "type:button\nname:HOME\n\n");
                }
            }
        }

        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        let register = receive(&mut socket).await;
        assert_eq!(register["payload"]["client-key"], PAIRING_KEY);
        send(&mut socket, json!({"id":"register_0","type":"registered"})).await;
        for (endpoint, payload) in [
            ("getPowerState", json!({"state":"Screen Off"})),
            ("turnOnScreen", json!({"returnValue":true})),
            ("getPowerState", json!({"state":"Active"})),
        ] {
            let request = receive(&mut socket).await;
            assert_eq!(
                request["uri"],
                format!("ssap://com.webos.service.tvpower/power/{endpoint}")
            );
            reply(&mut socket, &request, payload).await;
        }
    });

    let result = run(cli(temp.path(), &["--pair"])).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&key_path).unwrap(), PAIRING_KEY);
    let result = run(cli(
        temp.path(),
        &[
            "-volume",
            "-5",
            "device1",
            "-of",
            "-sethdmi2",
            "-od",
            "-button",
            "home",
        ],
    ))
    .await;
    assert_eq!(result.status.code(), Some(1));
    let stdout = String::from_utf8(result.stdout).unwrap();
    let mut responses = serde_json::Deserializer::from_str(&stdout).into_iter::<Value>();
    assert_eq!(
        responses.next().unwrap().unwrap(),
        json!({"Device1":{"payload":{"returnValue":true}}})
    );
    assert_eq!(
        responses.next().unwrap().unwrap(),
        json!({"Device1":{"error":"Mock TV rejected HDMI switch"}})
    );
    assert_eq!(
        responses.next().unwrap().unwrap(),
        json!({"Device1":{"info":"Button sent."}})
    );
    assert!(responses.next().is_none());
    assert!(stdout.contains("\n    \"Device1\": {\n"));
    let result = run(cli(temp.path(), &["-screenon"])).await;
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["Device1"]["payload"]["state"],
        "Active"
    );
    timeout(Duration::from_secs(5), mock)
        .await
        .unwrap()
        .unwrap();
}

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use lgtv_companion_runtime::{
    ipc::{self, Reply},
    pairing::Store,
};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, UnixStream},
    time::{sleep, timeout},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

const WAIT: Duration = Duration::from_secs(5);

struct Server {
    child: Child,
    socket: PathBuf,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("run/command.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_lgtv-companiond"))
            .arg("--config")
            .arg(dir.join("devices.json"))
            .arg("--state-dir")
            .arg(dir.join("pairing"))
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { child, socket }
    }

    async fn connect(&mut self) -> UnixStream {
        timeout(WAIT, async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "Daemon exited before listening"
                );
                match UnixStream::connect(&self.socket).await {
                    Ok(stream) => return stream,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("Cannot connect to daemon: {error}"),
                }
            }
        })
        .await
        .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn send(stream: &mut UnixStream, id: u64, args: &[&str]) {
    ipc::write(
        stream,
        &ipc::Request {
            version: ipc::VERSION,
            id,
            method: ipc::Method::Execute,
            arguments: args.iter().map(|argument| (*argument).to_owned()).collect(),
        },
    )
    .await
    .unwrap();
}

async fn reply(stream: &mut UnixStream, id: u64) -> Reply {
    let response: ipc::Response = timeout(WAIT, ipc::read(stream)).await.unwrap().unwrap();
    assert_eq!((response.id, response.version), (id, ipc::VERSION));
    response.reply
}

#[tokio::test]
async fn daemon_serializes_clients_and_cancels_a_disconnected_request() {
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    configure(dir, &tv, "off");

    let mut daemon = Server::start(dir);
    let mut invalid = daemon.connect().await;
    assert_eq!(
        fs::metadata(&daemon.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    send(
        &mut invalid,
        9,
        &["-volume", "5", "-poweroff", "Missing TV"],
    )
    .await;
    assert!(
        matches!(reply(&mut invalid, 9).await, Reply::Error { message } if message.contains("Unknown device"))
    );
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err(),
        "Invalid batch reached the TV"
    );

    let mut watcher = daemon.connect().await;
    ipc::write(
        &mut watcher,
        &ipc::Request {
            version: ipc::VERSION,
            id: 10,
            method: ipc::Method::WatchEvents,
            arguments: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply(&mut watcher, 10).await, Reply::Error { message }
        if message.contains("Set external_api to true"))
    );

    // Request framing rejects an oversized message before reading the body.
    let mut oversized = daemon.connect().await;
    oversized.write_u32(u32::MAX).await.unwrap();
    assert!(
        timeout(WAIT, ipc::read::<ipc::Response>(&mut oversized))
            .await
            .unwrap()
            .is_err()
    );

    let mut first = daemon.connect().await;
    send(&mut first, 11, &["-volume", "5", "Device1"]).await;
    let mut first_tv = connection(&tv).await;
    let command = next(&mut first_tv).await;
    assert_eq!(command["payload"]["volume"], 5);

    let mut second = daemon.connect().await;
    send(&mut second, 12, &["-volume", "9", "display tv"]).await;
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err(),
        "Concurrent command bypassed the TV worker"
    );
    drop(first);

    let mut second_tv = connection(&tv).await;
    let command = next(&mut second_tv).await;
    assert_eq!(command["payload"]["volume"], 9);
    second_tv
        .send(Message::Text(
            json!({
                "id": command["id"], "type":"response", "payload":{"returnValue":true}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert!(
        matches!(reply(&mut second, 12).await, Reply::Result { devices }
        if devices["Device1"]["payload"]["returnValue"] == true)
    );
    assert!(matches!(reply(&mut second, 12).await, Reply::Complete));

    // Abrupt termination leaves a socket behind. Restart reclaims it without
    // waking or registering against the TV.
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    assert!(daemon.socket.exists());
    let mut restarted = Server::start(dir);
    drop(restarted.connect().await);
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );
    kill_process(
        Pid::from_raw(restarted.child.id() as i32).unwrap(),
        Signal::TERM,
    )
    .unwrap();
    assert!(restarted.child.wait().unwrap().success());
    assert!(!restarted.socket.exists());
}

#[tokio::test]
async fn persistent_worker_reuses_connections_and_retries_read_only_queries() {
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    configure(tmp.path(), &tv, "persistent");
    let mut daemon = Server::start(tmp.path());
    let mut client = daemon.connect().await;

    send(&mut client, 1, &["-volume", "5", "Device1"]).await;
    let mut socket = connection(&tv).await;
    let first = next(&mut socket).await;
    assert_eq!(first["uri"], "ssap://audio/setVolume");
    respond(&mut socket, &first, json!({"returnValue":true})).await;
    assert_eq!(device_result(&mut client, 1).await["returnValue"], true);

    let mut client = daemon.connect().await;
    send(&mut client, 2, &["-request", "audio/getVolume", "Device1"]).await;
    let second = next(&mut socket).await;
    assert_eq!(second["uri"], "ssap://audio/getVolume");
    assert_ne!(first["id"], second["id"]);
    // A stale reply must be ignored even if it looks valid.
    respond(&mut socket, &first, json!({"returnValue":true,"volume":99})).await;
    socket
        .send(Message::Text("malformed".into()))
        .await
        .unwrap();

    let mut socket = connection(&tv).await;
    let retried = next(&mut socket).await;
    assert_eq!(retried["uri"], second["uri"]);
    respond(
        &mut socket,
        &retried,
        json!({"returnValue":true,"volume":5}),
    )
    .await;
    assert_eq!(device_result(&mut client, 2).await["volume"], 5);

    // Close the idle socket and verify the next command reconnects.
    socket.close(None).await.unwrap();
    drop(socket);
    let mut client = daemon.connect().await;
    send(&mut client, 3, &["-volume", "8", "Device1"]).await;
    let mut socket = connection(&tv).await;
    let command = next(&mut socket).await;
    assert_eq!(command["payload"]["volume"], 8);
    respond(&mut socket, &command, json!({"returnValue":true})).await;
    assert_eq!(device_result(&mut client, 3).await["returnValue"], true);
}

#[tokio::test]
async fn persistent_worker_recovers_without_replaying_writes() {
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    configure(tmp.path(), &tv, "persistent");
    let mut daemon = Server::start(tmp.path());
    let mut client = daemon.connect().await;

    send(&mut client, 1, &["-volume", "7", "Device1"]).await;
    let mut socket = connection(&tv).await;
    assert_eq!(next(&mut socket).await["uri"], "ssap://audio/setVolume");
    drop(socket);
    assert!(
        device_result(&mut client, 1).await["error"]
            .as_str()
            .unwrap()
            .contains("outcome is unknown")
    );
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );

    let mut client = daemon.connect().await;
    send(&mut client, 2, &["-poweroff", "Device1"]).await;
    let mut socket = connection(&tv).await;
    assert_eq!(next(&mut socket).await["uri"], "ssap://system/turnOff");
    drop(socket);
    let mut socket = connection(&tv).await;
    let state = next(&mut socket).await;
    assert_eq!(
        state["uri"],
        "ssap://com.webos.service.tvpower/power/getPowerState"
    );
    respond(
        &mut socket,
        &state,
        json!({"returnValue":true,"state":"Standby"}),
    )
    .await;
    let output = device_result(&mut client, 2).await;
    assert_eq!(output["state"], "Standby");
    assert_eq!(output["verified_after_reconnect"], true);
    assert!(
        timeout(WAIT, socket.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err())
    );
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );
}

fn configure(dir: &Path, listener: &TcpListener, mode: &str) {
    fs::write(dir.join("devices.json"), json!({
        "devices":[{"id":"Device1", "name":"Display TV","ip":"127.0.0.1","source_ip":"127.0.0.2",
        "port":listener.local_addr().unwrap().port(),"transport":"ws","connection_mode":mode}]
    }).to_string()).unwrap();
    Store::open(dir.join("pairing"))
        .unwrap()
        .save("Device1", "mock-pairing-key")
        .unwrap();
}

async fn device_result(client: &mut UnixStream, id: u64) -> Value {
    let Reply::Result { devices } = reply(client, id).await else {
        panic!("Expected a TV result");
    };
    assert!(matches!(reply(client, id).await, Reply::Complete));
    let device = &devices["Device1"];
    device.get("payload").unwrap_or(device).clone()
}

async fn respond(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    request: &Value,
    payload: Value,
) {
    socket
        .send(Message::Text(
            json!({"id": request["id"], "type":"response", "payload":payload})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}

async fn connection(tv: &TcpListener) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let (tcp, peer) = timeout(WAIT, tv.accept()).await.unwrap().unwrap();
    assert_eq!(peer.ip(), std::net::Ipv4Addr::new(127, 0, 0, 2));
    let mut socket = timeout(WAIT, accept_async(tcp)).await.unwrap().unwrap();
    let registration = next(&mut socket).await;
    assert_eq!(registration["payload"]["client-key"], "mock-pairing-key");
    socket
        .send(Message::Text(
            json!({"id":"register_0", "type":"registered"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
}

async fn next(socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Value {
    let message = timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

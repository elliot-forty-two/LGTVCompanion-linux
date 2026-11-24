use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use lgtv_companion_runtime::{
    ipc::{self, DisplayPower, HostEvent, Method, Reply, Request, Response, SessionSnapshot},
    pairing::Store,
};
use serde_json::{Value as Json, json};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines},
    net::{TcpListener, TcpStream, UnixStream},
    process::{Child, ChildStderr, Command},
    sync::mpsc,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use zbus::{
    Connection,
    connection::Builder,
    zvariant::{OwnedFd, OwnedObjectPath, Value},
};

const WAIT: Duration = Duration::from_secs(8);
const NAME: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const INTERFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_PATH: &str = "/org/freedesktop/login1/session/test";

#[tokio::test]
async fn power_monitor_observes_logind_without_using_the_host_bus() {
    let (_bus, address) = start_bus().await;
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .build()
        .await
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("devices.json");
    fs::write(
        &config,
        r#"{"external_api":true,"streaming":{"mode":"blank"},"devices":[]}"#,
    )
    .unwrap();
    let socket = temp.path().join("run/command.sock");
    let (_daemon, mut logs) = start_daemon(temp.path(), &address);
    timeout(WAIT, async {
        while let Some(line) = logs.next_line().await.unwrap() {
            if line == "Monitoring host power events through logind" {
                return;
            }
        }
        panic!("Daemon exited before subscribing to logind");
    })
    .await
    .unwrap();
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o660
    );
    assert_eq!(
        fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );

    let mut events = watch_events(&socket).await;

    // A peer without logind's name must not produce an event.
    let unrelated = Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    unrelated
        .emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForSleep", &false)
        .await
        .unwrap();

    for start in [true, true, false] {
        mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForSleep", &start)
            .await
            .unwrap();
    }
    for start in [true, false] {
        mock.emit_signal(
            None::<&str>,
            PATH,
            INTERFACE,
            "PrepareForShutdownWithMetadata",
            &(start, HashMap::from([("type", Value::from("reboot"))])),
        )
        .await
        .unwrap();
        mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForShutdown", &start)
            .await
            .unwrap();
    }
    mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForShutdown", &true)
        .await
        .unwrap();

    for expected in [
        "preparing for sleep",
        "resumed from sleep",
        "preparing to reboot",
        "shutdown preparation ended",
        "preparing for shutdown (type unknown)",
    ] {
        let line = timeout(WAIT, logs.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(line, format!("Host power event: {expected}"));
    }

    for event in [
        HostEvent::Suspend,
        HostEvent::Resume,
        HostEvent::Reboot,
        HostEvent::ShutdownUnknown,
    ] {
        expect_event(&mut events, event).await;
    }

    let stream_report = || {
        report_session_method(
            &socket,
            Method::ReportStream {
                source: "test".into(),
                session: "client".into(),
                connected: true,
            },
        )
    };
    assert!(
        matches!(stream_report().await, Reply::Error { message } if message.contains("transition in progress"))
    );

    // A replacement logind instance must start with fresh transition state.
    assert!(mock.release_name(NAME).await.unwrap());
    unrelated.request_name(NAME).await.unwrap();
    timeout(WAIT, async {
        while !matches!(stream_report().await, Reply::Complete) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    unrelated
        .emit_signal(
            None::<&str>,
            PATH,
            INTERFACE,
            "PrepareForShutdownWithMetadata",
            &(true, HashMap::from([("type", Value::from("power-off"))])),
        )
        .await
        .unwrap();
    let line = timeout(WAIT, logs.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(line, "Host power event: preparing for power-off");
    expect_event(&mut events, HostEvent::Shutdown).await;
    assert!(
        timeout(Duration::from_millis(70), events.read_u8())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn host_sleep_cancels_an_in_flight_command_on_a_persistent_socket() {
    let (_bus, address) = start_bus().await;
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .build()
        .await
        .unwrap();
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("devices.json"),
        json!({
            "devices":[{"id":"Device1","ip":"127.0.0.1","transport":"ws",
            "port":tv.local_addr().unwrap().port(),"connection_mode":"persistent"}]
        })
        .to_string(),
    )
    .unwrap();
    Store::open(temp.path().join("pairing"))
        .unwrap()
        .save("Device1", "mock-key")
        .unwrap();
    let (_daemon, mut logs) = start_daemon(temp.path(), &address);
    timeout(WAIT, async {
        loop {
            if logs.next_line().await.unwrap().unwrap()
                == "Monitoring host power events through logind"
            {
                break;
            }
        }
    })
    .await
    .unwrap();

    let path = temp.path().join("run/command.sock");
    let volume = |value: &'static str| {
        let path = &path;
        async move {
            let mut client = UnixStream::connect(path).await.unwrap();
            ipc::write(
                &mut client,
                &Request {
                    version: ipc::VERSION,
                    id: 1,
                    method: Method::Execute,
                    arguments: ["-volume", value, "Device1"].map(str::to_owned).to_vec(),
                },
            )
            .await
            .unwrap();
            client
        }
    };
    let mut client = volume("4").await;
    let mut socket = accept_tv(&tv).await;
    assert_eq!(next_frame(&mut socket).await["payload"]["volume"], 4);

    sleep_signal(&mock, true).await;
    let result: Response = timeout(WAIT, ipc::read(&mut client))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result.reply, Reply::Result { devices }
        if devices["Device1"]["error"] == "TV command canceled by host power transition"));
    assert!(
        timeout(WAIT, socket.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err())
    );

    sleep_signal(&mock, false).await;
    for expected in ["preparing for sleep", "resumed from sleep"] {
        assert_eq!(
            timeout(WAIT, logs.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            format!("Host power event: {expected}")
        );
    }

    let mut client = volume("8").await;
    let mut socket = accept_tv(&tv).await;
    answer(
        &mut socket,
        "ssap://audio/setVolume",
        json!({"returnValue":true}),
    )
    .await;
    let result: Response = timeout(WAIT, ipc::read(&mut client))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result.reply, Reply::Result { devices }
        if devices["Device1"]["payload"]["returnValue"] == true));
}

async fn watch_events(path: &Path) -> UnixStream {
    let mut stream = UnixStream::connect(path).await.unwrap();
    ipc::write(
        &mut stream,
        &Request {
            version: ipc::VERSION,
            id: 39,
            method: Method::WatchEvents,
            arguments: Vec::new(),
        },
    )
    .await
    .unwrap();
    let response: Response = timeout(WAIT, ipc::read(&mut stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((response.version, response.id), (ipc::VERSION, 39));
    assert!(matches!(response.reply, Reply::Subscribed));
    stream
}

async fn expect_event(watcher: &mut UnixStream, expected: HostEvent) {
    let response: Response = timeout(WAIT, ipc::read(watcher)).await.unwrap().unwrap();
    assert_eq!((response.version, response.id), (ipc::VERSION, 39));
    let Reply::Event { event } = response.reply else {
        panic!("Expected host event {expected:?}");
    };
    assert_eq!(event, expected);
}

async fn start_bus() -> (Child, String) {
    let mut bus = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("dbus-daemon is required for the power-monitor integration test");
    let mut bus_output = BufReader::new(bus.stdout.take().unwrap()).lines();
    let address = timeout(WAIT, bus_output.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    (bus, address)
}

fn start_daemon(dir: &Path, address: &str) -> (Child, Lines<BufReader<ChildStderr>>) {
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lgtv-companiond"))
        .env("DBUS_SYSTEM_BUS_ADDRESS", address)
        .arg("--config")
        .arg(dir.join("devices.json"))
        .arg("--state-dir")
        .arg(dir.join("pairing"))
        .arg("--socket")
        .arg(dir.join("run/command.sock"))
        .args(["--group-socket", "--monitor-power"])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let logs = BufReader::new(daemon.stderr.take().unwrap()).lines();
    (daemon, logs)
}

struct MockLogind {
    inhibitors: mpsc::UnboundedSender<UnixStream>,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl MockLogind {
    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        OwnedObjectPath::try_from(SESSION_PATH).unwrap()
    }

    #[zbus(property)]
    fn inhibit_delay_max_u_sec(&self) -> u64 {
        1_000_000
    }

    fn inhibit(&self, what: &str, who: &str, _why: &str, mode: &str) -> OwnedFd {
        assert_eq!(
            (what, who, mode),
            ("sleep:shutdown", "LGTV Companion", "delay")
        );
        let (lock, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        self.inhibitors
            .send(UnixStream::from_std(peer).unwrap())
            .unwrap();
        OwnedFd::from(std::os::fd::OwnedFd::from(lock))
    }
}

#[tokio::test]
async fn lifecycle_powers_off_only_eligible_tvs_and_releases_the_inhibitor() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .build()
        .await
        .unwrap();

    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let unmanaged = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("devices.json"), json!({
        "devices": [
            {"id":"Device1", "ip":"127.0.0.1", "port":tv.local_addr().unwrap().port(), "transport":"ws", "host_power":{"hdmi_input":1}},
            {"id":"Device2", "ip":"127.0.0.1", "port":unmanaged.local_addr().unwrap().port(), "transport":"ws"}
        ]
    }).to_string()).unwrap();
    let state = dir.join("pairing");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    for id in ["device1", "device2"] {
        let key = state.join(format!("{id}.key"));
        fs::write(&key, "mock-key").unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let (_daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    shutdown(&mock, "reboot", true).await;
    released(lock).await;
    shutdown(&mock, "reboot", false).await;
    let lock = armed(&mut locks, &mut logs).await;
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );

    mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForShutdown", &true)
        .await
        .unwrap();
    released(lock).await;
    mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForShutdown", &false)
        .await
        .unwrap();
    let lock = armed(&mut locks, &mut logs).await;
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );

    sleep_signal(&mock, true).await;
    let mut socket = accept_tv(&tv).await;
    answer(
        &mut socket,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Screen Off"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;
    released(lock).await;
    sleep_signal(&mock, false).await;
    let lock = armed(&mut locks, &mut logs).await;
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );

    shutdown(&mock, "power-off", true).await;
    let mut socket = accept_tv(&tv).await;
    answer(
        &mut socket,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi2"}),
    )
    .await;
    released(lock).await;
    assert!(!matches!(
        timeout(WAIT, socket.next()).await.unwrap(),
        Some(Ok(Message::Text(_)))
    ));
    shutdown(&mock, "power-off", false).await;
    let lock = armed(&mut locks, &mut logs).await;

    shutdown(&mock, "power-off", true).await;
    let mut socket = accept_tv(&tv).await;
    answer(
        &mut socket,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;
    released(lock).await;
    assert!(
        timeout(Duration::from_millis(60), tv.accept())
            .await
            .is_err()
    );
    shutdown(&mock, "power-off", false).await;
    let lock = armed(&mut locks, &mut logs).await;

    sleep_signal(&mock, true).await;
    let mut socket = accept_tv(&tv).await;
    let request = next_frame(&mut socket).await;
    assert_eq!(
        request["uri"],
        "ssap://com.webos.service.tvpower/power/getPowerState"
    );
    // Withhold the response. The daemon must cancel the connection and release
    // before the mock logind's one-second delay maximum.
    timeout(Duration::from_secs(1), released(lock))
        .await
        .unwrap();
    assert!(!matches!(
        timeout(WAIT, socket.next()).await.unwrap(),
        Some(Ok(Message::Text(_)))
    ));
    assert!(
        timeout(Duration::from_millis(60), unmanaged.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn host_resume_wakes_the_tv_without_a_session_agent() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .build()
        .await
        .unwrap();

    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("devices.json"),
        json!({
            "devices": [{"id": "Device1", "ip": "127.0.0.1",
                "port": tv.local_addr().unwrap().port(), "transport": "ws",
                "host_power": {"wake_on_resume": true}}]
        })
        .to_string(),
    )
    .unwrap();
    Store::open(dir.join("pairing"))
        .unwrap()
        .save("Device1", "mock-key")
        .unwrap();

    let (_daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    sleep_signal(&mock, true).await;
    let mut off = accept_tv(&tv).await;
    answer(
        &mut off,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state": "Standby"}),
    )
    .await;
    released(lock).await;

    sleep_signal(&mock, false).await;
    sleep_signal(&mock, false).await;
    // TCP can come up before webOS finishes bringing up the WebSocket service.
    let (tcp, _) = timeout(WAIT, tv.accept()).await.unwrap().unwrap();
    timeout(WAIT, async {
        let mut tcp = BufReader::new(tcp);
        let mut headers = String::new();
        while tcp.read_line(&mut headers).await.unwrap() > 0 {
            if headers.ends_with("\r\n\r\n") {
                assert!(headers.starts_with("GET / HTTP/1.1"));
                return;
            }
        }
        panic!("WebSocket request ended before the headers were complete");
    })
    .await
    .unwrap();
    let mut wake = accept_tv(&tv).await;
    answer(
        &mut wake,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state": "Active"}),
    )
    .await;
    let _lock = armed(&mut locks, &mut logs).await;
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn boot_wake_is_attempted_once_per_tv_across_daemon_restarts() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let _mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .build()
        .await
        .unwrap();

    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let other_tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let configure = |wake_other| {
        let config = json!({"devices": [
            {"id": "Device1", "ip": "127.0.0.1", "port": tv.local_addr().unwrap().port(),
                "transport": "ws", "host_power": {"wake_on_boot": true}},
            {"id": "Device2", "ip": "127.0.0.1", "port": other_tv.local_addr().unwrap().port(),
                "transport": "ws", "host_power": {"wake_on_boot": wake_other}}
        ]});
        fs::write(dir.join("devices.json"), config.to_string()).unwrap();
    };
    configure(false);
    let state = dir.join("pairing");
    let store = Store::open(state.clone()).unwrap();
    store.save("Device1", "mock-key").unwrap();
    store.save("Device2", "mock-key").unwrap();
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    fs::write(state.join(".last-boot-wake"), boot_id).unwrap();

    let (mut daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    let mut wake = accept_tv(&tv).await;
    answer(
        &mut wake,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state": "Active"}),
    )
    .await;
    daemon.kill().await.unwrap();
    released(lock).await;

    configure(true);
    let (mut daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    let mut wake = accept_tv(&other_tv).await;
    answer(
        &mut wake,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state": "Active"}),
    )
    .await;
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );
    daemon.kill().await.unwrap();
    released(lock).await;

    let (_daemon, mut logs) = start_daemon(dir, &address);
    let _lock = armed(&mut locks, &mut logs).await;
    assert!(
        timeout(Duration::from_millis(70), async {
            tokio::select! {
                _ = tv.accept() => {},
                _ = other_tv.accept() => {},
            }
        })
        .await
        .is_err()
    );
}

async fn armed(
    locks: &mut mpsc::UnboundedReceiver<UnixStream>,
    logs: &mut Lines<BufReader<ChildStderr>>,
) -> UnixStream {
    let lock = timeout(WAIT, locks.recv()).await.unwrap().unwrap();
    timeout(WAIT, async {
        loop {
            let line = logs.next_line().await.unwrap().expect("daemon exited");
            if line == "logind delay inhibitor ready" {
                break;
            }
        }
    })
    .await
    .unwrap();
    lock
}

async fn released(mut lock: UnixStream) {
    let mut byte = [0];
    assert_eq!(
        timeout(WAIT, lock.read(&mut byte)).await.unwrap().unwrap(),
        0
    );
}

async fn shutdown(mock: &Connection, kind: &str, start: bool) {
    mock.emit_signal(
        None::<&str>,
        PATH,
        INTERFACE,
        "PrepareForShutdownWithMetadata",
        &(start, HashMap::from([("type", Value::from(kind))])),
    )
    .await
    .unwrap();
    mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForShutdown", &start)
        .await
        .unwrap();
}

async fn sleep_signal(mock: &Connection, start: bool) {
    mock.emit_signal(None::<&str>, PATH, INTERFACE, "PrepareForSleep", &start)
        .await
        .unwrap();
}

async fn accept_tv(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (tcp, _) = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    let mut socket = timeout(WAIT, accept_async(tcp)).await.unwrap().unwrap();
    let register = next_frame(&mut socket).await;
    assert_eq!(register["payload"]["client-key"], "mock-key");
    socket
        .send(Message::Text(
            json!({"id":register["id"], "type":"registered"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
}

async fn next_frame(socket: &mut WebSocketStream<TcpStream>) -> Json {
    let frame = timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

async fn answer(socket: &mut WebSocketStream<TcpStream>, uri: &str, payload: Json) {
    let request = next_frame(socket).await;
    assert_eq!(request["uri"], uri);
    socket
        .send(Message::Text(
            json!({"id":request["id"], "type":"response", "payload":payload})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}

struct MockSession {
    active: Arc<AtomicBool>,
    kind: &'static str,
}

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl MockSession {
    #[zbus(property)]
    fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    fn remote(&self) -> bool {
        false
    }

    #[zbus(property, name = "Type")]
    fn session_type(&self) -> &str {
        self.kind
    }

    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (
            rustix::process::geteuid().as_raw(),
            OwnedObjectPath::try_from("/org/freedesktop/login1/user/test").unwrap(),
        )
    }

    #[zbus(property)]
    fn seat(&self) -> (&str, OwnedObjectPath) {
        (
            "seat0",
            OwnedObjectPath::try_from("/org/freedesktop/login1/seat/seat0").unwrap(),
        )
    }
}

#[tokio::test]
async fn authorized_session_transitions_are_shared_with_watchers_without_tvs() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, _) = mpsc::unbounded_channel();
    let active = Arc::new(AtomicBool::new(true));
    let _mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .serve_at(
            SESSION_PATH,
            MockSession {
                active: Arc::clone(&active),
                kind: "wayland",
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("devices.json"),
        r#"{"external_api":true,"devices":[]}"#,
    )
    .unwrap();
    let (_daemon, mut logs) = start_daemon(temp.path(), &address);
    assert!(
        timeout(WAIT, logs.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .contains("listening on")
    );
    let path = temp.path().join("run/command.sock");
    let mut first = watch_events(&path).await;
    let mut second = watch_events(&path).await;
    let report = |power, idle| {
        let path = &path;
        async move {
            report_session_method(
                path,
                Method::ReportSession {
                    snapshot: SessionSnapshot {
                        power,
                        active_outputs: None,
                        output_power: None,
                        idle,
                    },
                    session_id: None,
                },
            )
            .await
        }
    };
    assert!(matches!(
        report(DisplayPower::On, Some(false)).await,
        Reply::SessionSettings {
            idle_timeout_ms: Some(600_000),
            ..
        }
    ));
    report(DisplayPower::On, Some(false)).await;
    active.store(false, Ordering::Relaxed);
    assert!(matches!(
        report(DisplayPower::Off, Some(true)).await,
        Reply::Error { .. }
    ));
    active.store(true, Ordering::Relaxed);
    report(DisplayPower::On, Some(true)).await;
    expect_event(&mut first, HostEvent::UserIdle).await;
    expect_event(&mut second, HostEvent::UserIdle).await;

    drop(second);
    report(DisplayPower::Off, Some(true)).await;
    expect_event(&mut first, HostEvent::DisplaysOff).await;
    let mut reconnected = watch_events(&path).await;
    report(DisplayPower::On, Some(false)).await;
    for watcher in [&mut first, &mut reconnected] {
        expect_event(watcher, HostEvent::DisplaysOn).await;
        expect_event(watcher, HostEvent::UserBusy).await;
    }
    report(DisplayPower::Unknown, None).await;
    report(DisplayPower::Off, Some(true)).await;
    assert!(
        timeout(Duration::from_millis(70), first.read_u8())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn display_reports_drive_the_tv_only_after_an_authorized_transition() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let active = Arc::new(AtomicBool::new(true));
    let _mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .serve_at(
            SESSION_PATH,
            MockSession {
                active: Arc::clone(&active),
                kind: "x11",
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("devices.json"),
        json!({
            "devices":[{"id":"Device1","ip":"127.0.0.1","port":tv.local_addr().unwrap().port(),
                "transport":"ws","host_power":{"hdmi_input":1,"follow_display_power":true}}]
        })
        .to_string(),
    )
    .unwrap();
    let state = dir.join("pairing");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let key = state.join("device1.key");
    fs::write(&key, "mock-key").unwrap();
    fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    let (_daemon, mut logs) = start_daemon(dir, &address);
    let _lock = armed(&mut locks, &mut logs).await;

    let socket = dir.join("run/command.sock");
    assert!(matches!(
        report_display(&socket, DisplayPower::On).await,
        Reply::Complete
    ));
    active.store(false, Ordering::Relaxed);
    assert!(matches!(
        report_display(&socket, DisplayPower::Off).await,
        Reply::Error { .. }
    ));
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );

    active.store(true, Ordering::Relaxed);
    assert!(matches!(
        report_display(&socket, DisplayPower::Off).await,
        Reply::Complete
    ));
    let mut off = accept_tv(&tv).await;
    answer(
        &mut off,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut off,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut off,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;

    assert!(matches!(
        report_display(&socket, DisplayPower::On).await,
        Reply::Complete
    ));
    let mut on = accept_tv(&tv).await;
    answer(
        &mut on,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    assert!(matches!(
        report_display(&socket, DisplayPower::On).await,
        Reply::Complete
    ));
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn automation_override_is_temporary_and_manual_controls_keep_working() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .build()
        .await
        .unwrap();
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("devices.json"),
        json!({
            "devices":[{"id":"Device1","name":"Test TV","ip":"127.0.0.1","transport":"ws",
            "port":tv.local_addr().unwrap().port(),"host_power":{"enabled":false}}]
        })
        .to_string(),
    )
    .unwrap();
    Store::open(dir.join("pairing"))
        .unwrap()
        .save("Device1", "mock-key")
        .unwrap();
    let path = dir.join("run/command.sock");
    let (mut daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;

    assert_eq!(
        execute(&path, &["-AUTOENABLE", "test tv"]).await["automation_enabled"],
        true
    );
    assert_eq!(
        execute(&path, &["-autodisable", "Device1"]).await["automation_enabled"],
        false
    );
    sleep_signal(&mock, true).await;
    released(lock).await;
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );
    sleep_signal(&mock, false).await;
    let lock = armed(&mut locks, &mut logs).await;

    let manual = async {
        let mut socket = accept_tv(&tv).await;
        answer(
            &mut socket,
            "ssap://system/turnOff",
            json!({"returnValue":true}),
        )
        .await;
    };
    let (result, ()) = tokio::join!(execute(&path, &["-poweroff", "Device1"]), manual);
    assert_eq!(result["payload"]["returnValue"], true);

    assert_eq!(
        execute(&path, &["-autoenable"]).await["automation_enabled"],
        true
    );
    sleep_signal(&mock, true).await;
    let mut socket = accept_tv(&tv).await;
    answer(
        &mut socket,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut socket,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;
    released(lock).await;
    daemon.kill().await.unwrap();

    let (_restarted, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    sleep_signal(&mock, true).await;
    released(lock).await;
    assert!(
        timeout(Duration::from_millis(70), tv.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn automatic_wake_switches_hdmi_after_the_delay_unless_superseded() {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .serve_at(
            SESSION_PATH,
            MockSession {
                active: Arc::new(AtomicBool::new(true)),
                kind: "x11",
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("devices.json"),
        json!({
            "devices":[{"id":"Device1","ip":"127.0.0.1","transport":"ws",
            "port":tv.local_addr().unwrap().port(),"host_power":{
                "follow_display_power":true,"wake_hdmi":{"input":2,"delay_seconds":1}
            }}]
        })
        .to_string(),
    )
    .unwrap();
    Store::open(dir.join("pairing"))
        .unwrap()
        .save("Device1", "mock-key")
        .unwrap();
    let (_daemon, mut logs) = start_daemon(dir, &address);
    let lock = armed(&mut locks, &mut logs).await;
    let path = dir.join("run/command.sock");

    // Forced wake checks power and leaves the source alone.
    let forced = async {
        let mut socket = accept_tv(&tv).await;
        answer(
            &mut socket,
            "ssap://com.webos.service.tvpower/power/getPowerState",
            json!({"state":"Active"}),
        )
        .await;
    };
    let (result, ()) = tokio::join!(execute(&path, &["-poweron"]), forced);
    assert_eq!(result["payload"]["state"], "Active");

    enum Event {
        Unchanged,
        DisplayLost,
        AutomationDisabled,
        Reboot,
    }
    for event in [
        Event::Unchanged,
        Event::DisplayLost,
        Event::AutomationDisabled,
        Event::Reboot,
    ] {
        // Unknown is observational and allows us to establish an off baseline.
        for power in [DisplayPower::Unknown, DisplayPower::Off, DisplayPower::On] {
            assert!(matches!(
                report_display(&path, power).await,
                Reply::Complete
            ));
        }
        let mut wake = accept_tv(&tv).await;
        answer(
            &mut wake,
            "ssap://com.webos.service.tvpower/power/getPowerState",
            json!({"state":"Active"}),
        )
        .await;
        // A manual command arriving during the delay uses the worker immediately.
        let manual = async {
            let mut socket = accept_tv(&tv).await;
            answer(
                &mut socket,
                "ssap://audio/setVolume",
                json!({"returnValue":true}),
            )
            .await;
        };
        let (result, ()) = timeout(Duration::from_millis(750), async {
            tokio::join!(execute(&path, &["-volume", "10"]), manual)
        })
        .await
        .unwrap();
        assert_eq!(result["payload"]["returnValue"], true);

        match event {
            Event::DisplayLost => {
                assert!(matches!(
                    report_display(&path, DisplayPower::Unknown).await,
                    Reply::Complete
                ));
            }
            Event::AutomationDisabled => {
                assert_eq!(
                    execute(&path, &["-autodisable"]).await["automation_enabled"],
                    false
                );
                assert_eq!(
                    execute(&path, &["-autoenable"]).await["automation_enabled"],
                    true
                );
            }
            Event::Reboot => shutdown(&mock, "reboot", true).await,
            Event::Unchanged => {
                // Duplicate samples should leave the pending switch in place.
                assert!(matches!(
                    report_display(&path, DisplayPower::On).await,
                    Reply::Complete
                ));
                assert!(
                    timeout(Duration::from_millis(100), tv.accept())
                        .await
                        .is_err()
                );
                let mut hdmi = accept_tv(&tv).await;
                let launch = next_frame(&mut hdmi).await;
                assert_eq!(launch["uri"], "ssap://system.launcher/launch");
                assert_eq!(launch["payload"]["id"], "com.webos.app.hdmi2");
                hdmi.send(Message::Text(
                    json!({"id":launch["id"],"type":"response",
                "payload":{"returnValue":true}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                continue;
            }
        }
        assert!(
            timeout(Duration::from_millis(1200), tv.accept())
                .await
                .is_err()
        );
    }
    released(lock).await;
}

async fn execute(path: &Path, args: &[&str]) -> Json {
    let mut client = UnixStream::connect(path).await.unwrap();
    ipc::write(
        &mut client,
        &Request {
            version: ipc::VERSION,
            id: 1,
            method: Method::Execute,
            arguments: args.iter().map(|s| (*s).to_owned()).collect(),
        },
    )
    .await
    .unwrap();
    let result: Response = timeout(WAIT, ipc::read(&mut client))
        .await
        .unwrap()
        .unwrap();
    let Reply::Result { devices } = result.reply else {
        panic!("Expected a TV result");
    };
    let complete: Response = timeout(WAIT, ipc::read(&mut client))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(complete.reply, Reply::Complete));
    devices["Device1"].clone()
}

async fn report_display(path: &Path, power: DisplayPower) -> Reply {
    report_session_method(
        path,
        Method::ReportDisplay {
            power,
            session_id: None,
        },
    )
    .await
}

async fn report_session_method(path: &Path, method: Method) -> Reply {
    let mut socket = UnixStream::connect(path).await.unwrap();
    ipc::write(
        &mut socket,
        &Request {
            version: ipc::VERSION,
            id: 1,
            method,
            arguments: Vec::new(),
        },
    )
    .await
    .unwrap();
    let result: Response = timeout(WAIT, ipc::read(&mut socket))
        .await
        .unwrap()
        .unwrap();
    result.reply
}

#[tokio::test]
async fn wayland_session_blanks_restores_and_targets_the_mapped_output() {
    use DisplayPower::{Off, On, Unknown};

    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let _mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .serve_at(
            SESSION_PATH,
            MockSession {
                active: Arc::new(AtomicBool::new(true)),
                kind: "wayland",
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let first_tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let second_tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("devices.json"), json!({
        "idle_blank_minutes":1,
        "devices":[
            {"id":"Device1","ip":"127.0.0.1","port":first_tv.local_addr().unwrap().port(),"transport":"ws",
                "host_power":{"output":"DP-1","hdmi_input":1,"follow_display_power":true,"blank_when_idle":true,"mute_speakers":true}},
            {"id":"Device2","ip":"127.0.0.1","port":second_tv.local_addr().unwrap().port(),"transport":"ws",
                "host_power":{"output":"HDMI-A-1","follow_display_power":true}}
        ]
    }).to_string()).unwrap();
    let state = dir.join("pairing");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["device1", "device2"] {
        let key = state.join(format!("{name}.key"));
        fs::write(&key, "mock-key").unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (_daemon, mut logs) = start_daemon(dir, &address);
    let _lock = armed(&mut locks, &mut logs).await;
    let socket = dir.join("run/command.sock");
    let report = |first, idle| {
        report_session_method(
            &socket,
            Method::ReportSession {
                snapshot: SessionSnapshot {
                    power: On,
                    active_outputs: Some(vec!["DP-1".into(), "HDMI-A-1".into()]),
                    output_power: Some([("DP-1".into(), first), ("HDMI-A-1".into(), On)].into()),
                    idle: Some(idle),
                },
                session_id: None,
            },
        )
    };
    assert!(matches!(
        report(On, false).await,
        Reply::SessionSettings {
            idle_timeout_ms: Some(60_000),
            ..
        }
    ));
    assert!(
        timeout(Duration::from_millis(70), first_tv.accept())
            .await
            .is_err()
    );
    report(On, true).await;
    let mut tv = accept_tv(&first_tv).await;
    answer(
        &mut tv,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut tv,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    // Activity can arrive while the blanking request is still in flight.
    let blank = next_frame(&mut tv).await;
    assert_eq!(
        blank["uri"],
        "ssap://com.webos.service.tvpower/power/turnOffScreen"
    );
    report(On, false).await;
    tv.send(Message::Text(
        json!({"id":blank["id"],"type":"response","payload":{"returnValue":true}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    answer(
        &mut tv,
        "ssap://audio/setMute",
        json!({"returnValue":false}),
    )
    .await;

    let mut tv = accept_tv(&first_tv).await;
    answer(
        &mut tv,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Screen Off"}),
    )
    .await;
    answer(
        &mut tv,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut tv,
        "ssap://com.webos.service.tvpower/power/turnOnScreen",
        json!({"returnValue":true}),
    )
    .await;
    answer(&mut tv, "ssap://audio/setMute", json!({"returnValue":true})).await;
    timeout(WAIT, async {
        loop {
            let line = logs.next_line().await.unwrap().expect("daemon exited");
            if line == "Device1: TV acknowledged idle restore" {
                break;
            }
        }
    })
    .await
    .unwrap();

    report(Unknown, false).await;
    assert!(
        timeout(Duration::from_millis(70), first_tv.accept())
            .await
            .is_err()
    );
    report(On, false).await;
    report(Off, false).await;
    let mut tv = accept_tv(&first_tv).await;
    answer(
        &mut tv,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Active"}),
    )
    .await;
    answer(
        &mut tv,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut tv,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;
    assert!(
        timeout(Duration::from_millis(70), second_tv.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn streaming_keep_off_uses_first_and_last_source_then_requests_display_sync() {
    streaming_power_off("keep_off").await;
}

#[tokio::test]
async fn streaming_restore_uses_captured_power_before_the_stream() {
    streaming_power_off("restore").await;
}

async fn streaming_power_off(end_mode: &str) {
    let (_bus, address) = start_bus().await;
    let (inhibitors, mut locks) = mpsc::unbounded_channel();
    let _mock = Builder::address(address.as_str())
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, MockLogind { inhibitors })
        .unwrap()
        .serve_at(
            SESSION_PATH,
            MockSession {
                active: Arc::new(AtomicBool::new(true)),
                kind: "wayland",
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let tv = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("devices.json"), json!({
        "streaming":{"mode":"power_off","end_mode":end_mode},
        "devices":[{"id":"Device1", "ip":"127.0.0.1","port":tv.local_addr().unwrap().port(),"transport":"ws",
            "host_power":{"hdmi_input":1,"follow_display_power":true}}]
    }).to_string()).unwrap();
    let state = dir.join("pairing");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let key = state.join("device1.key");
    fs::write(&key, "mock-key").unwrap();
    fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    let (_daemon, mut logs) = start_daemon(dir, &address);
    let _lock = armed(&mut locks, &mut logs).await;
    let socket = dir.join("run/command.sock");

    let snapshot = || Method::ReportSession {
        snapshot: SessionSnapshot {
            power: DisplayPower::On,
            active_outputs: None,
            output_power: None,
            idle: Some(false),
        },
        session_id: None,
    };
    let hook = |source: &str, connected| Method::ReportStream {
        source: source.into(),
        session: "client-1".into(),
        connected,
    };
    let _ = report_session_method(&socket, snapshot()).await;
    assert!(matches!(
        report_session_method(&socket, hook("sunshine", true)).await,
        Reply::Complete
    ));
    assert!(matches!(
        report_session_method(&socket, hook("steam", true)).await,
        Reply::Complete
    ));
    // The native agent refreshes its display report every two seconds.
    assert!(timeout(Duration::from_secs(3), tv.accept()).await.is_err());
    let _ = report_session_method(&socket, snapshot()).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let _ = report_session_method(&socket, snapshot()).await;

    let mut stream = accept_tv(&tv).await;
    answer(
        &mut stream,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Screen Off"}),
    )
    .await;
    answer(
        &mut stream,
        "ssap://com.webos.applicationManager/getForegroundAppInfo",
        json!({"appId":"com.webos.app.hdmi1"}),
    )
    .await;
    answer(
        &mut stream,
        "ssap://system/turnOff",
        json!({"returnValue":true}),
    )
    .await;
    answer(
        &mut stream,
        "ssap://com.webos.service.tvpower/power/getPowerState",
        json!({"state":"Standby"}),
    )
    .await;
    wait_for_stream_log(&mut logs, "Device1: TV acknowledged power-off").await;

    assert!(matches!(
        report_session_method(&socket, hook("sunshine", false)).await,
        Reply::Complete
    ));
    assert!(
        timeout(Duration::from_millis(1150), tv.accept())
            .await
            .is_err()
    );
    assert!(matches!(
        report_session_method(&socket, snapshot()).await,
        Reply::SessionSettings {
            turn_displays_off: false,
            ..
        }
    ));
    assert!(matches!(
        report_session_method(&socket, hook("steam", false)).await,
        Reply::Complete
    ));
    wait_for_stream_log(&mut logs, "Remote streaming: disconnected").await;

    let result = report_session_method(&socket, snapshot()).await;
    match end_mode {
        "keep_off" => {
            assert!(matches!(
                result,
                Reply::SessionSettings {
                    turn_displays_off: true,
                    ..
                }
            ));
            assert!(matches!(
                report_session_method(&socket, snapshot()).await,
                Reply::SessionSettings {
                    turn_displays_off: false,
                    ..
                }
            ));
            assert!(
                timeout(Duration::from_millis(80), tv.accept())
                    .await
                    .is_err()
            );
        }
        "restore" => {
            assert!(matches!(
                result,
                Reply::SessionSettings {
                    turn_displays_off: false,
                    ..
                }
            ));
            let mut stream = accept_tv(&tv).await;
            answer(
                &mut stream,
                "ssap://com.webos.service.tvpower/power/getPowerState",
                json!({"state":"Active"}),
            )
            .await;
        }
        _ => unreachable!(),
    }
}

async fn wait_for_stream_log(logs: &mut Lines<BufReader<ChildStderr>>, expected: &str) {
    timeout(WAIT, async {
        loop {
            let line = logs.next_line().await.unwrap().expect("daemon exited");
            if line == expected {
                break;
            }
        }
    })
    .await
    .unwrap();
}

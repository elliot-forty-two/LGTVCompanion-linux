use std::process::Command;

use lgtv_companion_runtime::ipc::{self, Reply};
use serde_json::json;
use tokio::net::UnixListener;

#[tokio::test]
async fn daemon_mode_preserves_cli_formatting_and_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request: ipc::Request = ipc::read(&mut stream).await.unwrap();
        assert_eq!(request.version, ipc::VERSION);
        assert_eq!(
            request.arguments,
            [
                "-ok",
                "volume",
                "-request",
                "audio/getVolume",
                "-od",
                "-volume",
                "15",
                "-autodisable"
            ]
        );
        for reply in [
            Reply::Result {
                devices: json!({"Device1":{"payload":{"volume":10}}})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            Reply::Result {
                devices: json!({"Device1":{"error":"TV rejected request"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            Reply::Result {
                devices: json!({"Device1":{"automation_enabled":false}})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            Reply::Complete,
        ] {
            ipc::write(
                &mut stream,
                &ipc::Response {
                    version: ipc::VERSION,
                    id: request.id,
                    reply,
                },
            )
            .await
            .unwrap();
        }
    });

    let result = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_lgtvcli"))
            .arg("--socket")
            .arg(path)
            .args([
                "-ok",
                "volume",
                "-request",
                "audio/getVolume",
                "-od",
                "-volume",
                "15",
                "-autodisable",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        result.stdout,
        b"10\n{\"Device1\":{\"error\":\"TV rejected request\"}}\n{\"Device1\":{\"automation_enabled\":false}}\n"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn streaming_hooks_forward_the_source_and_session_to_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        for expected in [true, false] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: ipc::Request = ipc::read(&mut stream).await.unwrap();
            assert!(matches!(request.method, ipc::Method::ReportStream {
                source, session, connected
            } if source == "steam" && session == "client-42" && connected == expected));
            ipc::write(
                &mut stream,
                &ipc::Response {
                    version: ipc::VERSION,
                    id: request.id,
                    reply: Reply::Complete,
                },
            )
            .await
            .unwrap();
        }
    });
    let result = tokio::task::spawn_blocking(move || {
        for hook in ["-streaming_connect", "-streaming_disconnect"] {
            let result = Command::new(env!("CARGO_BIN_EXE_lgtvcli"))
                .arg("--socket")
                .arg(&path)
                .args([hook, "steam", "client-42"])
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, b"{\"accepted\":true}\n");
        }
    })
    .await;
    result.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn event_watcher_prints_json_or_legacy_tokens_and_reports_disconnect() {
    use lgtv_companion_runtime::ipc::{HostEvent, Method};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: ipc::Request = ipc::read(&mut stream).await.unwrap();
            assert!(matches!(request.method, Method::WatchEvents));
            assert!(request.arguments.is_empty());
            for reply in [
                Reply::Subscribed,
                Reply::Event {
                    event: HostEvent::DisplaysOn,
                },
                Reply::Event {
                    event: HostEvent::ShutdownUnknown,
                },
            ] {
                ipc::write(
                    &mut stream,
                    &ipc::Response {
                        version: ipc::VERSION,
                        id: request.id,
                        reply,
                    },
                )
                .await
                .unwrap();
            }
        }
    });
    tokio::task::spawn_blocking(move || {
        for (flags, expected) in [
            (
                &[][..],
                "{\"event\":\"displays_on\"}\n{\"event\":\"shutdown_unknown\"}\n",
            ),
            (&["--legacy"][..], "SYSTEM_DISPLAYS_ON\nSYSTEM_SHUTDOWN\n"),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_lgtvcli"))
                .arg("--socket")
                .arg(&path)
                .arg("--watch-events")
                .args(flags)
                .output()
                .unwrap();
            assert_eq!(output.stdout, expected.as_bytes());
            assert_eq!(output.status.code(), Some(2));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("Reconnect to receive new events")
            );
        }
    })
    .await
    .unwrap();
    server.await.unwrap();
}

mod events;
mod power;
mod session;
mod socket;
mod stream_log;
mod streams;

use std::{
    env,
    future::{Future, pending},
    io,
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use lgtv_companion_runtime::{
    batch::{self, Action, Step},
    config::Config,
    control::{Connection, default_state_dir, failed},
    ipc::{self, Reply, Request, Response},
    pairing::Store,
    tv::Result,
};
use serde_json::{Map, Value, json};
use socket::CommandSocket;
use tokio::{
    io::AsyncReadExt,
    net::{UnixStream, unix::OwnedWriteHalf},
    signal::unix::{SignalKind, signal},
    sync::{Semaphore, mpsc, oneshot, watch},
    time::{Instant, sleep_until, timeout, timeout_at},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const JOB_TIMEOUT: Duration = Duration::from_secs(125);
const QUEUE_SIZE: usize = 8;
const MAX_CLIENTS: usize = 32;
const MAX_BATCH_COMMANDS: usize = 128;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lgtv-companiond: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let mut config_path = PathBuf::from(ipc::DEFAULT_CONFIG);
    let mut socket_path = PathBuf::from(ipc::DEFAULT_SOCKET);
    let mut state_path = None;
    let mut group_socket = false;
    let mut monitor_power = false;
    let mut args = env::args_os().skip(1);
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "Usage: lgtv-companiond [--config FILE] [--socket PATH] [--state-dir DIR] [--group-socket] [--monitor-power]"
                );
                return Ok(());
            }
            Some("--config") => config_path = args.next().ok_or("--config requires a file")?.into(),
            Some("--socket") => socket_path = args.next().ok_or("--socket requires a path")?.into(),
            Some("--group-socket") => group_socket = true,
            Some("--monitor-power") => monitor_power = true,
            Some("--state-dir") => {
                state_path = Some(
                    args.next()
                        .ok_or("--state-dir requires a directory")?
                        .into(),
                )
            }
            _ => return Err("Unknown option; see --help".into()),
        }
    }
    let config = Arc::new(
        Config::load(&config_path)
            .map_err(|error| format!("Cannot load {}: {error}", config_path.display()))?,
    );
    let store = Arc::new(Store::open(
        state_path
            .or_else(default_state_dir)
            .ok_or("Specify a state directory with --state-dir")?,
    )?);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let socket = CommandSocket::bind(socket_path.clone(), group_socket).await?;
    let state = Arc::new(Daemon::new(config, store));
    streams::start_log_providers(&state);
    let clients = Arc::new(Semaphore::new(MAX_CLIENTS));
    let owner = rustix::process::geteuid().as_raw();
    eprintln!("lgtv-companiond listening on {}", socket_path.display());
    if monitor_power {
        tokio::spawn(power::monitor(Arc::clone(&state)));
    }

    loop {
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            connection = socket.listener.accept() => {
                let (connection, _) = connection?;
                // Group mode is authorized by the pathname socket and directory
                // permissions, which also account for clients' supplementary groups.
                if !group_socket && !connection.peer_cred().is_ok_and(|peer| peer.uid() == owner) {
                    continue;
                }
                let Ok(permit) = Arc::clone(&clients).try_acquire_owned() else {
                    continue;
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _permit = permit;
                    // Commands and TV replies may include user-provided payloads.
                    // Keep client protocol errors out of the service log.
                    let _ = handle_client(connection, state).await;
                });
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Automation {
    enabled: bool,
    revision: u64,
    display_revision: u64,
}

#[derive(Clone, Copy)]
struct AutomationGuard {
    revision: u64,
    display_revision: Option<u64>,
}

impl AutomationGuard {
    fn is_current(self, state: Automation) -> bool {
        state.enabled
            && self.revision == state.revision
            && self
                .display_revision
                .is_none_or(|revision| revision == state.display_revision)
    }
}

async fn automation_changed(
    state: &mut watch::Receiver<Automation>,
    guard: Option<AutomationGuard>,
) {
    match guard {
        Some(guard) => {
            let _ = state.wait_for(|state| !guard.is_current(*state)).await;
        }
        None => pending().await,
    }
}

struct Job {
    automation: Option<AutomationGuard>,
    epoch: u64,
    action: Arc<Action>,
    response: oneshot::Sender<Value>,
    deadline: Instant,
    capture: Option<streams::Capture>,
}

struct Daemon {
    events: events::Feed,
    connection_epoch: watch::Sender<u64>,
    automation: Vec<watch::Sender<Automation>>,
    config: Arc<Config>,
    workers: Vec<mpsc::Sender<Job>>,
    display: Mutex<session::DisplayAutomation>,
    streams: Mutex<streams::Controller>,
}

impl Daemon {
    fn new(config: Arc<Config>, store: Arc<Store>) -> Self {
        let (connection_epoch, _) = watch::channel(0_u64);
        let automation: Vec<_> = config
            .devices
            .iter()
            .map(|device| {
                watch::Sender::new(Automation {
                    enabled: device
                        .host_power
                        .as_ref()
                        .is_some_and(|policy| policy.enabled),
                    ..Automation::default()
                })
            })
            .collect();
        let workers: Vec<_> = automation.iter().enumerate().map(|(index, automation)| {
            let (sender, mut receiver) = mpsc::channel::<Job>(QUEUE_SIZE);
            let config = Arc::clone(&config);
            let store = Arc::clone(&store);
            let mut epoch = connection_epoch.subscribe();
            let mut automation_state = automation.subscribe();
            tokio::spawn(async move {
                let device = &config.devices[index];
                let mut idle_blanked = false;
                let mut connection = Connection::new(device.connection_mode);
                loop {
                    let Job { epoch: queued_epoch, automation, action, mut response, deadline, capture } = tokio::select! {
                        biased;
                        change = epoch.changed() => {
                            if change.is_err() {
                                break;
                            }
                            connection.reset();
                            continue;
                        }
                        job = receiver.recv() => match job {
                            Some(job) => job,
                            None => break,
                        },
                        () = connection.idle(device, &store) => continue,
                    };
                    if queued_epoch != *epoch.borrow() {
                        let _ = response.send(json!({"error": "TV command canceled by host power transition"}));
                        continue;
                    }
                    if automation.is_some_and(|guard| !guard.is_current(*automation_state.borrow())) {
                        let _ = response.send(json!({"info": "TV automation superseded"}));
                        continue;
                    }
                    if matches!(&*action, Action::IdleRestore) && !idle_blanked {
                        let _ = response.send(json!({}));
                        continue;
                    }
                    tokio::select! {
                        biased;
                        change = epoch.changed() => {
                            connection.reset();
                            if change.is_err() {
                                break;
                            }
                            let _ = response.send(json!({"error": "TV command canceled by host power transition"}));
                        }
                        () = automation_changed(&mut automation_state, automation) => {
                            let _ = response.send(json!({"info": "TV automation superseded"}));
                        }
                        _ = response.closed() => {}
                        _ = sleep_until(deadline) => {
                            let _ = response.send(json!({"error": "TV command timed out"}));
                        }
                        result = connection.execute(device, &store, &action, config.wake_timeout_seconds, |before, confirmed| {
                            if let Some(capture) = &capture {
                                capture.report(before, confirmed);
                            }
                        }) => {
                            let output = match result {
                                Ok(value) => {
                                    update_blank_ownership(&mut idle_blanked, &action, &value);
                                    value
                                }
                                Err(error) => json!({"error": error.to_string()}),
                            };
                            let _ = response.send(output);
                        }
                    }
                }
            });
            sender
        }).collect();
        Self {
            events: events::Feed::new(config.external_api),
            connection_epoch,
            automation,
            config,
            display: Mutex::new(session::DisplayAutomation::new(workers.len())),
            workers,
            streams: Mutex::default(),
        }
    }

    fn automation_enabled(&self, target: usize) -> bool {
        self.automation[target].borrow().enabled
    }

    fn set_automation(&self, target: usize, enabled: bool) -> Value {
        if enabled && self.config.devices[target].host_power.is_none() {
            return json!({"error": "Configure host_power before enabling TV automation"});
        }
        self.automation[target].send_if_modified(|state| {
            if state.enabled == enabled {
                return false;
            }
            state.enabled = enabled;
            state.revision = state.revision.wrapping_add(1);
            true
        });
        json!({"automation_enabled": enabled})
    }

    fn display_changed(&self, target: usize) {
        self.automation[target].send_modify(|state| {
            state.display_revision = state.display_revision.wrapping_add(1);
        });
    }

    async fn execute(&self, target: usize, action: Arc<Action>) -> Value {
        if let Action::SetAutomation(enabled) = *action {
            return self.set_automation(target, enabled);
        }
        let deadline = Instant::now() + JOB_TIMEOUT;
        let result = self.enqueue(target, action, deadline, None, None);
        match result {
            Ok(result) => wait_for_result(result, deadline).await,
            Err(value) => value,
        }
    }

    fn start_automation(
        self: &Arc<Self>,
        target: usize,
        action: Action,
        capture: Option<streams::Capture>,
        deadline: Instant,
        allow_hdmi_switch: bool,
    ) -> std::result::Result<impl Future<Output = Value> + use<>, Value> {
        let state = *self.automation[target].borrow();
        if !state.enabled {
            return Err(json!({"info": "TV automation disabled"}));
        }

        let guard = AutomationGuard {
            revision: state.revision,
            display_revision: None,
        };
        let hdmi = self.config.devices[target]
            .host_power
            .as_ref()
            .filter(|_| allow_hdmi_switch && matches!(action, Action::Wake))
            .and_then(|policy| policy.wake_hdmi);
        let mut epoch = self.connection_epoch.subscribe();
        let result = self.enqueue(target, Arc::new(action), deadline, capture, Some(guard))?;
        let daemon = Arc::clone(self);

        Ok(async move {
            let output = wait_for_result(result, deadline).await;
            if let Some(hdmi) = hdmi
                && !failed(&output)
                && output["payload"]["state"] == "Active"
            {
                let guard = AutomationGuard {
                    display_revision: Some(state.display_revision),
                    ..guard
                };
                let mut automation = daemon.automation[target].subscribe();
                tokio::select! {
                    biased;
                    _ = epoch.changed() => {}
                    () = automation_changed(&mut automation, Some(guard)) => {}
                    () = tokio::time::sleep(Duration::from_secs(hdmi.delay_seconds.into())) => {
                        let deadline = Instant::now() + JOB_TIMEOUT;
                        let command = Arc::new(batch::hdmi_action(hdmi.input));
                        let result = daemon.enqueue(target, command, deadline, None, Some(guard));
                        let output = match result {
                            Ok(result) => wait_for_result(result, deadline).await,
                            Err(value) => value,
                        };
                        if failed(&output) {
                            eprintln!("{}: automatic HDMI switch failed", daemon.config.devices[target].id);
                        }
                    }
                }
            }
            output
        })
    }

    fn enqueue(
        &self,
        target: usize,
        action: Arc<Action>,
        deadline: Instant,
        capture: Option<streams::Capture>,
        automation: Option<AutomationGuard>,
    ) -> std::result::Result<oneshot::Receiver<Value>, Value> {
        let (response, result) = oneshot::channel();
        match self.workers[target].try_send(Job {
            epoch: *self.connection_epoch.borrow(),
            automation,
            action,
            response,
            deadline,
            capture,
        }) {
            Ok(()) => Ok(result),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(json!({"error": "TV command queue is full"}))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(json!({"error": "TV worker stopped"})),
        }
    }

    fn reset_tv_connections(&self) {
        self.connection_epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn host_transition(&self, paused: bool, after_resume: bool) {
        self.streams
            .lock()
            .expect("stream controller")
            .host_transition(paused);
        let mut display = self.display.lock().expect("display state");
        display.set_streaming(false);
        display.host_transition(paused, after_resume);
    }

    fn has_power_automation(&self) -> bool {
        self.config
            .devices
            .iter()
            .any(|device| device.host_power.is_some())
    }

    fn power_targets(&self, unknown_shutdown: bool) -> impl Iterator<Item = usize> + '_ {
        self.config
            .devices
            .iter()
            .enumerate()
            .filter(|(index, _)| self.automation_enabled(*index))
            .filter_map(move |(index, device)| {
                device
                    .host_power
                    .as_ref()
                    .filter(|policy| !unknown_shutdown || policy.power_off_on_unknown_shutdown)
                    .map(|_| index)
            })
    }

    async fn power_off(self: &Arc<Self>, unknown_shutdown: bool, deadline: Instant) {
        futures_util::future::join_all(self.power_targets(unknown_shutdown).map(
            |index| async move {
                let result =
                    match self.start_automation(index, Action::HostPowerOff, None, deadline, false)
                    {
                        Ok(work) => work.await,
                        Err(value) => value,
                    };
                let id = &self.config.devices[index].id;
                if failed(&result) {
                    eprintln!("{id}: automatic power-off failed");
                } else if let Some(info) = result["info"].as_str() {
                    eprintln!("{id}: {info}");
                }
            },
        ))
        .await;
    }

    fn commands(&self, args: &[String]) -> Result<Vec<(Arc<Action>, Vec<usize>)>> {
        let mut commands = Vec::new();
        for step in batch::parse(args, true)? {
            if let Step::Command { action, selectors } = step {
                commands.push((Arc::from(action), self.config.targets(&selectors)?));
            }
        }
        if commands.is_empty() || commands.len() > MAX_BATCH_COMMANDS {
            return Err(format!("Supply between 1 and {MAX_BATCH_COMMANDS} TV commands").into());
        }
        Ok(commands)
    }
}

async fn handle_client(stream: UnixStream, state: Arc<Daemon>) -> Result<()> {
    let peer = stream.peer_cred()?;
    let (mut reader, mut writer) = stream.into_split();
    let request: Request = timeout(IO_TIMEOUT, ipc::read(&mut reader)).await??;
    let result = async {
        if request.version != ipc::VERSION {
            return send(
                &mut writer,
                request.id,
                Reply::Error {
                    message: "Unsupported command protocol version".into(),
                },
            )
            .await;
        }
        let commands = match request.method {
            ipc::Method::ReportStream {
                source,
                session,
                connected,
            } => {
                let valid = |value: &str| {
                    !value.is_empty()
                        && value.len() <= 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
                };
                let reply = if valid(&source) && valid(&session) {
                    let source = format!("hook/{}/{source}", peer.uid());
                    match state
                        .streams
                        .lock()
                        .expect("stream controller")
                        .report(&state, &source, &session, connected)
                    {
                        Ok(()) => Reply::Complete,
                        Err(message) => Reply::Error {
                            message: message.into(),
                        },
                    }
                } else {
                    Reply::Error {
                        message: "Stream source and session must use 1–64 letters, digits, or -_.:"
                            .into(),
                    }
                };
                return send(&mut writer, request.id, reply).await;
            }
            ipc::Method::Execute => state.commands(&request.arguments),
            ipc::Method::WatchEvents => {
                if !request.arguments.is_empty() {
                    return send(
                        &mut writer,
                        request.id,
                        Reply::Error {
                            message: "WatchEvents takes no arguments".into(),
                        },
                    )
                    .await;
                }
                return state.events.watch(&mut writer, request.id).await;
            }
            ipc::Method::ReportSession {
                snapshot,
                session_id,
            } => {
                let reply = accept_session_report(&state, peer, session_id, &snapshot).await;
                return send(&mut writer, request.id, reply).await;
            }
            ipc::Method::ReportDisplay { power, session_id } => {
                let snapshot = ipc::SessionSnapshot {
                    power,
                    active_outputs: None,
                    output_power: None,
                    idle: None,
                };
                let reply = match session::authorize(peer, session_id.as_deref()).await {
                    Ok(path) => {
                        state
                            .display
                            .lock()
                            .expect("display state")
                            .report(&state, path, &snapshot);
                        Reply::Complete
                    }
                    Err(error) => Reply::Error {
                        message: error.to_string(),
                    },
                };
                return send(&mut writer, request.id, reply).await;
            }
        };

        let commands = match commands {
            Ok(commands) => commands,
            Err(error) => {
                return send(
                    &mut writer,
                    request.id,
                    Reply::Error {
                        message: error.to_string(),
                    },
                )
                .await;
            }
        };
        for (action, targets) in commands {
            let mut devices = Map::new();
            for target in targets {
                let result = state.execute(target, Arc::clone(&action)).await;
                devices.insert(state.config.devices[target].id.clone(), result);
            }
            send(&mut writer, request.id, Reply::Result { devices }).await?;
        }
        send(&mut writer, request.id, Reply::Complete).await
    };

    // EOF or extra input cancels the request and drops any pending worker reply.
    tokio::select! {
        _ = reader.read_u8() => Ok(()),
        result = result => result.map_err(Into::into),
    }
}

async fn send(writer: &mut OwnedWriteHalf, id: u64, reply: Reply) -> io::Result<()> {
    timeout(
        IO_TIMEOUT,
        ipc::write(
            writer,
            &Response {
                version: ipc::VERSION,
                id,
                reply,
            },
        ),
    )
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?
}

async fn accept_session_report(
    state: &Arc<Daemon>,
    peer: tokio::net::unix::UCred,
    session_id: Option<String>,
    snapshot: &ipc::SessionSnapshot,
) -> Reply {
    match session::authorize(peer, session_id.as_deref()).await {
        Ok(path) => {
            let mut display = state.display.lock().expect("display state");
            display.report(state, path, snapshot);
            Reply::SessionSettings {
                idle_timeout_ms: state
                    .config
                    .idle_blank_minutes
                    .or_else(|| state.config.external_api.then_some(10))
                    .map(|minutes| minutes * 60_000),
                turn_displays_off: display.take_display_off(snapshot),
            }
        }
        Err(error) => Reply::Error {
            message: error.to_string(),
        },
    }
}

fn update_blank_ownership(owned: &mut bool, action: &Action, value: &Value) {
    if failed(value) {
        return;
    }
    match action {
        Action::IdleBlank if value["applied"] == true => *owned = true,
        Action::Wake | Action::IdleRestore => *owned = false,
        Action::HostPowerOff | Action::StreamPowerOff if value["applied"] == true => *owned = false,
        Action::Ssap(request)
            if matches!(
                request.uri.as_str(),
                "ssap://system/turnOff"
                    | "ssap://com.webos.service.tvpower/power/turnOffScreen"
                    | "ssap://com.webos.service.tvpower/power/turnOnScreen"
            ) =>
        {
            *owned = false
        }
        _ => {}
    }
}

async fn wait_for_result(result: oneshot::Receiver<Value>, deadline: Instant) -> Value {
    match timeout_at(deadline, result).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => json!({"error": "TV worker stopped"}),
        Err(_) => json!({"error": "Timed out waiting for the TV worker"}),
    }
}

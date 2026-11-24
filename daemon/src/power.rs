//! Host lifecycle observations from the logind manager on the system bus.

use std::{
    collections::HashMap,
    fs,
    future::{Future, pending},
    io::{Read, Seek, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use tokio::time::{MissedTickBehavior, interval, sleep, sleep_until, timeout};
use zbus::{
    Connection, Message, Proxy,
    proxy::{Builder, CacheProperties},
    zvariant::{OwnedFd, OwnedValue},
};

use crate::{Daemon, JOB_TIMEOUT};
use lgtv_companion_runtime::{
    batch::Action, config::HostPower, control::failed, ipc::HostEvent, tv::Result,
};

const SERVICE: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const INTERFACE: &str = "org.freedesktop.login1.Manager";
const RECONNECT_DELAY: Duration = Duration::from_secs(10);
const DBUS_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_WORK_TIME: Duration = Duration::from_secs(4);
const RELEASE_MARGIN: Duration = Duration::from_millis(250);
type PowerWork = Pin<Box<dyn Future<Output = ()> + Send>>;

pub async fn monitor(daemon: Arc<Daemon>) {
    loop {
        if let Err(error) = subscribe(&daemon).await {
            daemon.host_transition(false, false);
            eprintln!("logind power monitor: {error}. Reconnecting in 10 seconds");
        }
        sleep(RECONNECT_DELAY).await;
    }
}

async fn subscribe(daemon: &Arc<Daemon>) -> Result<()> {
    let proxy: Proxy<'_> = timeout(Duration::from_secs(5), async {
        let connection = Connection::system().await?;
        Builder::new(&connection)
            .destination(SERVICE)?
            .path(PATH)?
            .interface(INTERFACE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    })
    .await??;
    // A single stream preserves the order of metadata and legacy signals.
    let (mut owners, mut signals) = timeout(Duration::from_secs(5), async {
        Ok::<_, zbus::Error>((
            proxy.receive_owner_changed().await?,
            proxy.receive_all_signals().await?,
        ))
    })
    .await??;

    eprintln!("Monitoring host power events through logind");
    let mut state = PowerState::default();
    let mut owner = None::<String>;
    let mut inhibitor: Option<DelayInhibitor> = None;
    let mut work: Option<PowerWork> = None;
    let automation = daemon.has_power_automation();
    let mut retry = interval(RECONNECT_DELAY);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let message = tokio::select! {
            biased;
            changed = owners.next() => {
                let Some(owner_change) = changed else { break };
                let next_owner = owner_change.map(|name| name.to_string());
                if next_owner != owner {
                    owner = next_owner;
                    state = PowerState::default();
                    inhibitor = None;
                    work = None;
                    retry.reset_immediately();
                }
                continue;
            }
            message = signals.next() => match message {
                Some(message) => message,
                None => break,
            },
            _ = async {
                match work.as_mut() {
                    Some(job) => job.await,
                    None => pending().await,
                }
            } => {
                work = None;
                continue;
            }
            _ = retry.tick(), if automation && inhibitor.is_none() && !state.in_progress() => {
                match timeout(DBUS_TIMEOUT, DelayInhibitor::acquire(&proxy)).await {
                    Ok(Ok(lock)) => {
                        owner = Some(lock.owner.clone());
                        inhibitor = Some(lock);
                        eprintln!("logind delay inhibitor ready");
                    }
                    Ok(Err(error)) => eprintln!("logind delay inhibitor unavailable: {error}"),
                    Err(_) => eprintln!("logind delay inhibitor acquisition timed out"),
                }
                continue;
            }
        };
        if let Some(sender) = message.header().sender()
            && owner.as_deref() != Some(sender.as_str())
        {
            owner = Some(sender.to_string());
            state = PowerState::default();
            inhibitor = None;
            work = None;
            retry.reset_immediately();
        }
        match state.observe(&message) {
            Ok(Some(event)) => {
                eprintln!("Host power event: {}", event.description());
                daemon.reset_tv_connections();
                daemon.host_transition(!event.is_end(), event == Event::Resume);
                if let Some(event) = event.host_event() {
                    daemon.events.publish(event);
                }
                if event.is_end() {
                    work = (event == Event::Resume && !state.in_progress()).then(|| {
                        let daemon = Arc::clone(daemon);
                        Box::pin(async move { wake(&daemon, WakeTrigger::Resume).await })
                            as PowerWork
                    });
                    retry.reset_immediately();
                } else {
                    let unknown = event == Event::UnknownShutdown;
                    let lock = inhibitor.take();
                    work = None;
                    if event.permits_power_off() && daemon.power_targets(unknown).next().is_some() {
                        if let Some(lock) = lock {
                            let daemon = Arc::clone(daemon);
                            let deadline = tokio::time::Instant::now() + lock.budget;
                            work = Some(Box::pin(async move {
                                let _fd = lock.fd;
                                tokio::select! {
                                    biased;
                                    _ = sleep_until(deadline) => eprintln!(
                                        "Host power deadline reached; releasing the logind delay inhibitor"
                                    ),
                                    _ = daemon.power_off(unknown, deadline) => {}
                                }
                            }));
                        } else {
                            eprintln!(
                                "Skipped automatic TV power-off because no logind delay inhibitor was acquired"
                            );
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("logind power monitor: invalid lifecycle signal: {error}"),
        }
    }
    Err("logind signal stream closed".into())
}

#[derive(Clone, Copy)]
enum WakeTrigger {
    Boot,
    Resume,
}

impl WakeTrigger {
    fn enabled(self, policy: &HostPower) -> bool {
        match self {
            Self::Boot => policy.wake_on_boot,
            Self::Resume => policy.wake_on_resume,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Boot => "host boot",
            Self::Resume => "host resume",
        }
    }

    fn targets(self, daemon: &Daemon) -> impl Iterator<Item = usize> {
        daemon
            .config
            .devices
            .iter()
            .enumerate()
            .filter(move |(index, device)| {
                device
                    .host_power
                    .as_ref()
                    .is_some_and(|policy| self.enabled(policy))
                    && daemon.automation_enabled(*index)
            })
            .map(|(index, _)| index)
    }
}

pub async fn wake_at_boot(daemon: Arc<Daemon>, state_path: PathBuf) {
    let trigger = WakeTrigger::Boot;
    if trigger.targets(&daemon).next().is_none() {
        return;
    }
    match claim_boot_wake(&state_path) {
        Ok(true) => wake(&daemon, trigger).await,
        Ok(false) => {}
        Err(error) => eprintln!("Could not record automatic boot wake: {error}"),
    }
}

fn claim_boot_wake(state_path: &Path) -> Result<bool> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let mut marker = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state_path.join(".last-boot-wake"))?;
    marker.lock()?;
    let mut last_boot = String::new();
    marker.read_to_string(&mut last_boot)?;
    if last_boot == boot_id {
        return Ok(false);
    }
    marker.rewind()?;
    marker.write_all(boot_id.as_bytes())?;
    marker.set_len(boot_id.len() as u64)?;
    marker.sync_all()?;
    Ok(true)
}

async fn wake(daemon: &Arc<Daemon>, trigger: WakeTrigger) {
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    futures_util::future::join_all(trigger.targets(daemon).map(|index| async move {
        let id = &daemon.config.devices[index].id;
        let reason = trigger.description();
        eprintln!("{id}: waking TV after {reason}");
        let result = match daemon.start_automation(index, Action::Wake, None, deadline, true) {
            Ok(work) => work.await,
            Err(value) => value,
        };
        if failed(&result) {
            let detail = result["error"]
                .as_str()
                .unwrap_or("TV rejected the wake request");
            let detail: String = detail
                .chars()
                .take(240)
                .flat_map(char::escape_debug)
                .collect();
            eprintln!("{id}: automatic wake after {reason} failed: {detail}");
        } else if let Some(info) = result["info"].as_str() {
            eprintln!("{id}: {info}");
        } else if result["payload"]["state"] == "Active" {
            eprintln!("{id}: TV is active");
        }
    }))
    .await;
}

struct DelayInhibitor {
    fd: OwnedFd,
    budget: Duration,
    owner: String,
}

impl DelayInhibitor {
    async fn acquire(proxy: &Proxy<'_>) -> Result<Self> {
        let limit: u64 = proxy.get_property("InhibitDelayMaxUSec").await?;
        let budget = Duration::from_micros(limit)
            .saturating_sub(RELEASE_MARGIN)
            .min(MAX_WORK_TIME);
        if budget.is_zero() {
            return Err("logind has no usable delay budget".into());
        }
        let reply = proxy
            .call_method(
                "Inhibit",
                &(
                    "sleep:shutdown",
                    "LGTV Companion",
                    "Turn off managed TVs during host power transitions",
                    "delay",
                ),
            )
            .await?;
        let fd = reply.body().deserialize()?;
        let owner = reply
            .header()
            .sender()
            .ok_or("logind inhibitor reply has no sender")?
            .to_string();
        Ok(Self { fd, budget, owner })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    PrepareForSleep,
    Resume,
    PowerOff,
    Halt,
    Reboot,
    Kexec,
    SoftReboot,
    UnknownShutdown,
    ShutdownEnded,
}

impl Event {
    fn permits_power_off(self) -> bool {
        matches!(
            self,
            Self::PrepareForSleep | Self::PowerOff | Self::Halt | Self::UnknownShutdown
        )
    }

    fn is_end(self) -> bool {
        matches!(self, Self::Resume | Self::ShutdownEnded)
    }
    fn shutdown(action: Option<&str>) -> Self {
        match action {
            Some("power-off") => Self::PowerOff,
            Some("halt") => Self::Halt,
            Some("reboot") => Self::Reboot,
            Some("kexec") => Self::Kexec,
            Some("soft-reboot") => Self::SoftReboot,
            _ => Self::UnknownShutdown,
        }
    }

    fn host_event(self) -> Option<HostEvent> {
        Some(match self {
            Self::PrepareForSleep => HostEvent::Suspend,
            Self::Resume => HostEvent::Resume,
            Self::PowerOff | Self::Halt => HostEvent::Shutdown,
            Self::Reboot | Self::Kexec | Self::SoftReboot => HostEvent::Reboot,
            Self::UnknownShutdown => HostEvent::ShutdownUnknown,
            Self::ShutdownEnded => return None,
        })
    }

    fn description(self) -> &'static str {
        match self {
            Self::PrepareForSleep => "preparing for sleep",
            Self::Resume => "resumed from sleep",
            Self::PowerOff => "preparing for power-off",
            Self::Halt => "preparing to halt",
            Self::Reboot => "preparing to reboot",
            Self::Kexec => "preparing for kexec reboot",
            Self::SoftReboot => "preparing for soft reboot",
            Self::UnknownShutdown => "preparing for shutdown (type unknown)",
            Self::ShutdownEnded => "shutdown preparation ended",
        }
    }
}

#[derive(Default)]
struct PowerState {
    sleeping: Option<bool>,
    shutting_down: Option<bool>,
}

impl PowerState {
    fn in_progress(&self) -> bool {
        self.sleeping == Some(true) || self.shutting_down == Some(true)
    }

    fn observe(&mut self, message: &Message) -> zbus::Result<Option<Event>> {
        let header = message.header();
        let Some(member) = header.member() else {
            return Ok(None);
        };
        let body = message.body();
        match member.as_str() {
            "PrepareForSleep" => {
                let start: bool = body.deserialize()?;
                Ok(transition(
                    &mut self.sleeping,
                    start,
                    Event::PrepareForSleep,
                    Event::Resume,
                ))
            }
            "PrepareForShutdown" => {
                let start: bool = body.deserialize()?;
                Ok(transition(
                    &mut self.shutting_down,
                    start,
                    Event::UnknownShutdown,
                    Event::ShutdownEnded,
                ))
            }
            "PrepareForShutdownWithMetadata" => {
                let (start, metadata): (bool, HashMap<String, OwnedValue>) = body.deserialize()?;
                let action = metadata
                    .get("type")
                    .and_then(|value| <&str>::try_from(value).ok());
                Ok(transition(
                    &mut self.shutting_down,
                    start,
                    Event::shutdown(action),
                    Event::ShutdownEnded,
                ))
            }
            _ => Ok(None),
        }
    }
}

fn transition(
    previous: &mut Option<bool>,
    start: bool,
    preparing: Event,
    completed: Event,
) -> Option<Event> {
    if previous.replace(start) == Some(start) {
        None
    } else if start {
        Some(preparing)
    } else {
        Some(completed)
    }
}

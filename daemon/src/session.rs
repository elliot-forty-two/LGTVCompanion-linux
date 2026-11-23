use std::{sync::Arc, time::Duration};

use lgtv_companion_core::display::{DisplayPolicy, DisplayPower, Effect};
use lgtv_companion_runtime::{batch::Action, control::failed, ipc::SessionSnapshot, tv::Result};
use tokio::{
    net::unix::UCred,
    task::JoinHandle,
    time::{Instant, timeout},
};
use zbus::{
    Connection, Proxy,
    proxy::{Builder, CacheProperties},
    zvariant::OwnedObjectPath,
};

use crate::{Daemon, JOB_TIMEOUT};

const SERVICE: &str = "org.freedesktop.login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

pub async fn authorize(peer: UCred, hint: Option<&str>) -> Result<String> {
    let pid = peer
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or("Display reporter has no local process ID")?;
    timeout(Duration::from_secs(2), async {
        let connection = Connection::system().await?;
        let manager: Proxy<'_> = Builder::new(&connection)
            .destination(SERVICE)?
            .path("/org/freedesktop/login1")?
            .interface(MANAGER)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let path: OwnedObjectPath = match manager.call("GetSessionByPID", &(pid,)).await {
            Ok(path) => path,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.login1.NoSessionForPID" =>
            {
                // Desktop autostart can launch the agent in a systemd user
                // scope. In that case verify the supplied session against
                // logind and the peer UID before accepting it.
                let id = hint
                    .filter(|id| !id.is_empty() && id.len() <= 128)
                    .ok_or("Display reporter has no logind session")?;
                manager.call("GetSession", &(id,)).await?
            }
            Err(error) => return Err(error.into()),
        };
        let session: Proxy<'_> = Builder::new(&connection)
            .destination(SERVICE)?
            .path(path.as_str())?
            .interface(SESSION)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let (active, remote, kind, seat, user): (
            bool,
            bool,
            String,
            (String, OwnedObjectPath),
            (u32, OwnedObjectPath),
        ) = tokio::try_join!(
            session.get_property("Active"),
            session.get_property("Remote"),
            session.get_property("Type"),
            session.get_property("Seat"),
            session.get_property("User"),
        )?;
        if !active
            || remote
            || !matches!(kind.as_str(), "x11" | "wayland")
            || seat.0 != "seat0"
            || user.0 != peer.uid()
        {
            return Err(
                "Display reports require the active local graphical session on seat0".into(),
            );
        }
        Ok(path.to_string())
    })
    .await?
}

pub struct DisplayAutomation {
    devices: Vec<DeviceSession>,
    session: Option<String>,
    paused: bool,
    streaming: bool,
    latest: Option<(Instant, SessionSnapshot)>,
    display_off: Option<Instant>,
    awaiting_display_off: Option<Instant>,
}

#[derive(Default)]
struct DeviceSession {
    policy: DisplayPolicy,
    was_idle: Option<bool>,
    work: Vec<JoinHandle<()>>,
}

impl DisplayAutomation {
    pub fn new(device_count: usize) -> Self {
        Self {
            devices: (0..device_count)
                .map(|_| DeviceSession::default())
                .collect(),
            session: None,
            paused: false,
            streaming: false,
            latest: None,
            display_off: None,
            awaiting_display_off: None,
        }
    }

    pub fn host_transition(&mut self, paused: bool, after_resume: bool) {
        self.paused = paused;
        self.latest = None;
        self.display_off = None;
        self.awaiting_display_off = None;
        for state in &mut self.devices {
            state.cancel();
            state.was_idle = None;
            if paused {
                state.policy.pause();
            } else {
                state.policy.reset(after_resume);
            }
        }
    }

    pub fn set_streaming(&mut self, active: bool) {
        self.streaming = active;
        self.display_off = None;
        self.awaiting_display_off = None;
        for state in &mut self.devices {
            state.cancel();
            state.was_idle = None;
            state.policy.reset(false);
        }
    }

    pub fn snapshot(&self) -> Option<&SessionSnapshot> {
        self.latest
            .as_ref()
            .filter(|(seen, _)| seen.elapsed() <= Duration::from_secs(8))
            .map(|(_, snapshot)| snapshot)
    }

    pub fn request_display_off(&mut self) {
        self.display_off = Some(Instant::now() + Duration::from_secs(5));
    }

    pub fn report(&mut self, daemon: &Arc<Daemon>, session: String, snapshot: &SessionSnapshot) {
        if self.paused {
            return;
        }
        let switched = self.session.as_ref() != Some(&session);
        if switched {
            self.display_off = None;
            self.awaiting_display_off = None;
        }
        if !switched && let Some(previous) = self.snapshot() {
            daemon.events.publish_session_changes(previous, snapshot);
        }
        for (index, device) in daemon.config.devices.iter().enumerate() {
            if let Some(config) = device.host_power.as_ref() {
                let output = config.output.as_deref();
                if switched
                    || self.latest.as_ref().is_some_and(|(_, previous)| {
                        previous.power_for(output) != snapshot.power_for(output)
                    })
                {
                    daemon.display_changed(index);
                }
            }
        }
        self.session = Some(session.clone());
        self.latest = Some((Instant::now(), snapshot.clone()));
        if self.streaming {
            return;
        }
        if let Some(deadline) = self.awaiting_display_off {
            if deadline > Instant::now() && snapshot.power != DisplayPower::Off {
                return;
            }
            self.awaiting_display_off = None;
            for state in &mut self.devices {
                state.cancel();
                state.policy.reset(false);
                state.was_idle = None;
            }
        }
        for (index, (state, device)) in self
            .devices
            .iter_mut()
            .zip(&daemon.config.devices)
            .enumerate()
        {
            let Some(config) = device.host_power.as_ref() else {
                continue;
            };
            let power = snapshot.power_for(config.output.as_deref());
            let display_action = state.policy.observe(session.clone(), power);
            if switched || power != DisplayPower::On {
                state.was_idle = None;
            }
            if display_action.is_some() {
                state.cancel();
                if config.follow_display_power && daemon.automation_enabled(index) {
                    let action = match display_action {
                        Some(Effect::Wake) => Some(Action::Wake),
                        Some(Effect::PowerOff) => Some(Action::HostPowerOff),
                        _ => None,
                    };
                    if let Some(action) = action {
                        state.submit(daemon, index, action);
                        // A display wake itself unblanks the TV.
                        state.was_idle = snapshot.idle.filter(|_| power == DisplayPower::On);
                        continue;
                    }
                }
            }
            if config.blank_when_idle
                && daemon.config.idle_blank_minutes.is_some()
                && power == DisplayPower::On
                && let Some(idle) = snapshot.idle
            {
                let previous = state.was_idle.replace(idle);
                if !daemon.automation_enabled(index) {
                    continue;
                }
                match (previous, idle) {
                    (Some(false), true) => state.submit(daemon, index, Action::IdleBlank),
                    (None | Some(true), false) => state.submit(daemon, index, Action::IdleRestore),
                    _ => {}
                }
            }
        }
    }

    pub fn take_display_off(&mut self, snapshot: &SessionSnapshot) -> bool {
        let requested = self.display_off.take().is_some_and(|deadline| {
            deadline > Instant::now() && snapshot.power == DisplayPower::On
        });
        if requested {
            self.awaiting_display_off = Some(Instant::now() + Duration::from_secs(5));
        }
        requested
    }
}

impl DeviceSession {
    fn cancel(&mut self) {
        for work in self.work.drain(..) {
            work.abort();
        }
    }

    fn submit(&mut self, daemon: &Arc<Daemon>, index: usize, action: Action) {
        self.work.retain(|job| !job.is_finished());
        let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
        let id = daemon.config.devices[index].id.clone();
        let allow_hdmi_switch = matches!(action, Action::Wake);
        let Ok(work) = daemon.start_automation(index, action, None, deadline, allow_hdmi_switch)
        else {
            eprintln!("{id}: could not queue the session display action");
            return;
        };
        self.work.push(tokio::spawn(async move {
            let result = work.await;
            if failed(&result) {
                eprintln!("{id}: session display action failed");
            } else if let Some(info) = result["info"].as_str() {
                eprintln!("{id}: {info}");
            } else if result["payload"]["state"] == "Active" {
                eprintln!("{id}: TV is active");
            }
        }));
    }
}

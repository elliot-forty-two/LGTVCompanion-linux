use std::sync::{Arc, Mutex, Weak};

use lgtv_companion_core::streaming::{
    Action as Effect, Generation, HostDisplay, StreamSessions, Streaming, Transition, TvPower,
};
use lgtv_companion_runtime::{batch::Action, control::failed, ipc::DisplayPower};
use tokio::{
    task::JoinHandle,
    time::{Instant, sleep_until},
};

use crate::{Daemon, JOB_TIMEOUT};

const MAX_SESSIONS: usize = 128;

pub struct Capture {
    policy: Arc<Mutex<Streaming>>,
    generation: Generation,
    device: String,
}

impl Capture {
    pub fn report(&self, before: TvPower, confirmed_off: bool) {
        let mut policy = self.policy.lock().expect("stream power state");
        policy.record_pre_stream_power(self.generation, &self.device, before);
        if confirmed_off {
            policy.record_powered_off(self.generation, &self.device);
        }
    }
}

pub struct Controller {
    sessions: StreamSessions,
    policy: Arc<Mutex<Streaming>>,
    origin: Instant,
    timer: Option<JoinHandle<()>>,
    work: Vec<JoinHandle<()>>,
    paused: bool,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            sessions: StreamSessions::default(),
            policy: Arc::new(Mutex::new(Streaming::default())),
            origin: Instant::now(),
            timer: None,
            work: Vec::new(),
            paused: false,
        }
    }
}

impl Controller {
    pub fn report(
        &mut self,
        daemon: &Arc<Daemon>,
        source: &str,
        session: &str,
        connected: bool,
    ) -> Result<(), &'static str> {
        if daemon.config.streaming.is_none() {
            return Err("Streaming automation is disabled in the daemon configuration");
        }
        if self.paused {
            return Err("Host power transition in progress");
        }
        if connected
            && self.sessions.len() >= MAX_SESSIONS
            && !self.sessions.contains(source, session)
        {
            return Err("Too many tracked streams");
        }
        let before = self.sessions.deadline();
        self.sessions
            .report(source, session, connected, self.origin.elapsed());
        let after = self.sessions.deadline();
        if after != before {
            if let Some(timer) = self.timer.take() {
                timer.abort();
            }
            if let Some(deadline) = after {
                let daemon = Arc::downgrade(daemon);
                let when = self.origin + deadline;
                self.timer = Some(tokio::spawn(async move {
                    sleep_until(when).await;
                    if let Some(daemon) = daemon.upgrade() {
                        daemon
                            .streams
                            .lock()
                            .expect("stream controller")
                            .advance(&daemon);
                    }
                }));
            }
        }
        Ok(())
    }

    pub fn host_transition(&mut self, paused: bool) {
        self.paused = paused;
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
        self.sessions = StreamSessions::default();
        self.policy.lock().expect("stream power state").cancel();
        self.cancel_work();
    }

    fn advance(&mut self, daemon: &Arc<Daemon>) {
        let Some(transition) = self.sessions.advance(self.origin.elapsed()) else {
            return;
        };
        let Some(config) = daemon.config.streaming.as_ref() else {
            return;
        };
        let mut display = daemon.display.lock().expect("display state");
        let snapshot = display.snapshot();
        let host_power = match snapshot.map(|snapshot| snapshot.power) {
            Some(DisplayPower::On) => HostDisplay::On,
            Some(DisplayPower::Off) => HostDisplay::Off,
            _ => HostDisplay::Unknown,
        };
        let devices: Vec<_> = daemon
            .config
            .devices
            .iter()
            .enumerate()
            .filter(|(index, _)| daemon.automation_enabled(*index))
            .map(|(_, device)| device)
            .filter(|device| {
                device.host_power.as_ref().is_some_and(|policy| {
                    snapshot.is_some_and(|snapshot| {
                        snapshot.power_for(policy.output.as_deref()) == DisplayPower::On
                    })
                })
            })
            .map(|device| device.id.clone())
            .collect();

        self.cancel_work();
        let effects = {
            let mut policy = self.policy.lock().expect("stream power state");
            match transition {
                Transition::Connected => policy.connect(config.policy(), host_power, &devices),
                Transition::Disconnected => policy.disconnect(host_power, &devices),
            }
        };
        display.set_streaming(transition == Transition::Connected);
        eprintln!(
            "Remote streaming: {}",
            if transition == Transition::Connected {
                "connected"
            } else {
                "disconnected"
            }
        );
        for effect in effects {
            match effect {
                Effect::HostDisplaysOff => display.request_display_off(),
                Effect::Wake {
                    device,
                    allow_hdmi_switch,
                } => {
                    self.submit(daemon, device, Action::Wake, None, allow_hdmi_switch);
                }
                Effect::Blank { device, .. } => {
                    self.submit(daemon, device, Action::StreamBlank, None, false)
                }
                Effect::PowerOffWithCapture { generation, device } => {
                    let capture = Capture {
                        policy: Arc::clone(&self.policy),
                        generation,
                        device: device.clone(),
                    };
                    self.submit(daemon, device, Action::StreamPowerOff, Some(capture), false);
                }
            }
        }
    }

    fn cancel_work(&mut self) {
        for job in self.work.drain(..) {
            job.abort();
        }
    }

    fn submit(
        &mut self,
        daemon: &Arc<Daemon>,
        id: String,
        action: Action,
        capture: Option<Capture>,
        allow_hdmi_switch: bool,
    ) {
        let index = daemon
            .config
            .devices
            .iter()
            .position(|device| device.id == id)
            .expect("configured TV");
        let deadline = Instant::now() + JOB_TIMEOUT;
        let Ok(work) = daemon.start_automation(index, action, capture, deadline, allow_hdmi_switch)
        else {
            eprintln!("{id}: could not queue streaming TV action");
            return;
        };
        self.work.push(tokio::spawn(async move {
            let result = work.await;
            if failed(&result) {
                eprintln!("{id}: streaming TV action failed");
            } else if let Some(info) = result["info"].as_str() {
                eprintln!("{id}: {info}");
            }
        }));
    }
}

pub fn start_log_providers(daemon: &Arc<Daemon>) {
    let Some(config) = &daemon.config.streaming else {
        return;
    };
    for (name, file) in [
        ("sunshine", &config.sunshine_log),
        ("apollo", &config.apollo_log),
    ] {
        if let Some(file) = file {
            let daemon: Weak<Daemon> = Arc::downgrade(daemon);
            let file = file.clone();
            tokio::spawn(async move {
                let mut log = crate::stream_log::Log::new(file);
                let mut previous = false;
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    tick.tick().await;
                    let Ok((updated, active)) = tokio::task::spawn_blocking(move || {
                        let active = log.poll();
                        (log, active)
                    })
                    .await
                    else {
                        return;
                    };
                    log = updated;
                    let Some(daemon) = daemon.upgrade() else {
                        return;
                    };
                    if active != previous {
                        let result = daemon.streams.lock().expect("stream controller").report(
                            &daemon,
                            &format!("log/{name}"),
                            "active_clients",
                            active,
                        );
                        if result.is_ok() {
                            previous = active;
                        }
                    }
                }
            });
        }
    }
}

//! Decisions for the first connect and last disconnect of a stream.
//!
//! The session adapter aggregates sources and delivers debounced transitions.
//! The worker applies HDMI safety and reports the outcome of its power-off attempt.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub const CONNECT_DELAY: Duration = Duration::from_secs(10);
pub const DISCONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndMode {
    #[default]
    PowerOn,
    KeepOff,
    Restore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    PowerOff(EndMode),
    Blank,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDisplay {
    On,
    Off,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TvPower {
    Active,
    ScreenOff,
    Standby,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Generation(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    PowerOffWithCapture {
        generation: Generation,
        device: String,
    },
    Blank {
        generation: Generation,
        device: String,
    },
    Wake {
        device: String,
        allow_hdmi_switch: bool,
    },
    HostDisplaysOff,
}

#[derive(Default)]
pub struct Streaming {
    generation: u64,
    active: Option<Stream>,
}

struct Stream {
    generation: Generation,
    mode: Mode,
    attempts: BTreeMap<String, PowerOffAttempt>,
}

#[derive(Default)]
struct PowerOffAttempt {
    before: Option<TvPower>,
    powered_off: bool,
}

impl Streaming {
    /// Returns actions in configured device order. Pass the effective managed
    /// device set, including temporary automation overrides.
    pub fn connect(
        &mut self,
        mode: Mode,
        host_display: HostDisplay,
        managed_devices: &[String],
    ) -> Vec<Action> {
        if self.active.is_some() {
            return Vec::new();
        }

        self.generation += 1;
        let generation = Generation(self.generation);
        let mut stream = Stream {
            generation,
            mode,
            attempts: BTreeMap::new(),
        };
        let mut actions = Vec::new();
        if host_display == HostDisplay::On {
            for device in managed_devices {
                actions.push(match mode {
                    Mode::PowerOff(_) => {
                        stream
                            .attempts
                            .insert(device.clone(), PowerOffAttempt::default());
                        Action::PowerOffWithCapture {
                            generation,
                            device: device.clone(),
                        }
                    }
                    Mode::Blank => Action::Blank {
                        generation,
                        device: device.clone(),
                    },
                });
            }
        }
        self.active = Some(stream);
        actions
    }

    /// Record the first observation so a later retry cannot overwrite it.
    pub fn record_pre_stream_power(
        &mut self,
        generation: Generation,
        device: &str,
        power: TvPower,
    ) {
        if let Some(attempt) = self.attempt(generation, device) {
            attempt.before.get_or_insert(power);
        }
    }

    /// Report that the worker verified the stream's action powered off this TV.
    pub fn record_powered_off(&mut self, generation: Generation, device: &str) {
        if let Some(attempt) = self.attempt(generation, device) {
            attempt.powered_off = true;
        }
    }

    fn attempt(&mut self, generation: Generation, device: &str) -> Option<&mut PowerOffAttempt> {
        self.active
            .as_mut()
            .filter(|stream| stream.generation == generation)?
            .attempts
            .get_mut(device)
    }

    pub fn disconnect(
        &mut self,
        host_display: HostDisplay,
        managed_devices: &[String],
    ) -> Vec<Action> {
        let Some(stream) = self.active.take() else {
            return Vec::new();
        };
        if host_display != HostDisplay::On {
            return Vec::new();
        }

        let mut actions = Vec::new();
        for device in managed_devices {
            let wake = match stream.mode {
                Mode::Blank | Mode::PowerOff(EndMode::PowerOn) => true,
                Mode::PowerOff(EndMode::KeepOff) => false,
                Mode::PowerOff(EndMode::Restore) => {
                    stream.attempts.get(device).is_some_and(|attempt| {
                        matches!(attempt.before, Some(TvPower::Active | TvPower::ScreenOff))
                    })
                }
            };
            if wake {
                actions.push(Action::Wake {
                    device: device.clone(),
                    allow_hdmi_switch: matches!(stream.mode, Mode::PowerOff(_)),
                });
            }
        }

        if matches!(
            stream.mode,
            Mode::PowerOff(EndMode::KeepOff | EndMode::Restore)
        ) && actions.is_empty()
            && stream.attempts.values().any(|attempt| attempt.powered_off)
        {
            actions.push(Action::HostDisplaysOff);
        }
        actions
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Discard the stream on a host lifecycle transition. Workers also cancel
    /// any outstanding actions associated with that generation.
    pub fn cancel(&mut self) {
        self.active = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    Connected,
    Disconnected,
}

/// Debounces the aggregate of uniquely identified source/session pairs.
/// Timestamps come from the caller's monotonic clock.
#[derive(Default)]
pub struct StreamSessions {
    active: BTreeSet<(String, String)>,
    published_connected: bool,
    deadline: Option<Duration>,
}

impl StreamSessions {
    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn contains(&self, source: &str, session: &str) -> bool {
        self.active
            .contains(&(source.to_owned(), session.to_owned()))
    }

    pub fn report(&mut self, source: &str, session: &str, connected: bool, now: Duration) {
        let was_connected = !self.active.is_empty();
        let key = (source.to_owned(), session.to_owned());
        if connected {
            self.active.insert(key);
        } else {
            self.active.remove(&key);
        }
        let is_connected = !self.active.is_empty();
        if was_connected != is_connected {
            self.deadline = if self.published_connected == is_connected {
                None
            } else {
                Some(now.saturating_add(if is_connected {
                    CONNECT_DELAY
                } else {
                    DISCONNECT_DELAY
                }))
            };
        }
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    pub fn advance(&mut self, now: Duration) -> Option<Transition> {
        if now < self.deadline? {
            return None;
        }
        self.deadline = None;
        self.published_connected = !self.active.is_empty();
        Some(if self.published_connected {
            Transition::Connected
        } else {
            Transition::Disconnected
        })
    }
}

//! Length-prefixed JSON for the local command socket.

use std::{collections::BTreeMap, io};

pub use lgtv_companion_core::display::DisplayPower;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u16 = 1;
pub const DEFAULT_SOCKET: &str = "/run/lgtv-companion/command.sock";
pub const DEFAULT_CONFIG: &str = "/etc/lgtv-companion/devices.json";
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u16,
    pub id: u64,
    pub method: Method,
    pub arguments: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Execute,
    WatchEvents,
    ReportStream {
        source: String,
        session: String,
        connected: bool,
    },
    ReportSession {
        snapshot: SessionSnapshot,
        session_id: Option<String>,
    },
    ReportDisplay {
        power: DisplayPower,
        #[serde(default)]
        session_id: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSnapshot {
    pub power: DisplayPower,
    pub active_outputs: Option<Vec<String>>,
    pub output_power: Option<BTreeMap<String, DisplayPower>>,
    pub idle: Option<bool>,
}

impl SessionSnapshot {
    pub fn power_for(&self, output: Option<&str>) -> DisplayPower {
        let Some(output) = output else {
            return self.power;
        };
        match &self.active_outputs {
            None => DisplayPower::Unknown,
            Some(outputs) if !outputs.iter().any(|name| name == output) => DisplayPower::Off,
            Some(_) => self.output_power.as_ref().map_or(self.power, |powers| {
                powers.get(output).copied().unwrap_or(DisplayPower::Unknown)
            }),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub id: u64,
    pub reply: Reply,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Result {
        devices: Map<String, Value>,
    },
    Complete,
    Subscribed,
    Event {
        event: HostEvent,
    },
    SessionSettings {
        idle_timeout_ms: Option<u32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        turn_displays_off: bool,
    },
    Error {
        message: String,
    },
}

/// Transitions observed by the daemon, independent of TV automation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEvent {
    DisplaysOn,
    DisplaysOff,
    UserIdle,
    UserBusy,
    Suspend,
    Resume,
    Reboot,
    Shutdown,
    ShutdownUnknown,
}

impl HostEvent {
    pub fn legacy_token(self) -> &'static str {
        match self {
            Self::DisplaysOn => "SYSTEM_DISPLAYS_ON",
            Self::DisplaysOff => "SYSTEM_DISPLAYS_OFF",
            Self::UserIdle => "SYSTEM_USER_IDLE",
            Self::UserBusy => "SYSTEM_USER_BUSY",
            Self::Suspend => "SYSTEM_SUSPEND",
            Self::Resume => "SYSTEM_RESUME",
            Self::Reboot => "SYSTEM_REBOOT",
            Self::Shutdown | Self::ShutdownUnknown => "SYSTEM_SHUTDOWN",
        }
    }
}

pub async fn read<T: DeserializeOwned>(mut reader: impl AsyncRead + Unpin) -> io::Result<T> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid IPC frame size",
        ));
    }
    let mut json = vec![0; size];
    reader.read_exact(&mut json).await?;
    serde_json::from_slice(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub async fn write<T: Serialize>(
    mut writer: impl AsyncWrite + Unpin,
    message: &T,
) -> io::Result<()> {
    let json = serde_json::to_vec(message)?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds size limit",
        ));
    }
    writer.write_u32(json.len() as u32).await?;
    writer.write_all(&json).await?;
    writer.flush().await
}

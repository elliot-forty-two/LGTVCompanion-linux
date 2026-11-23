use std::{
    collections::BTreeSet,
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
};

use lgtv_companion_core::streaming::{EndMode, Mode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tv::Result;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_api: bool,
    #[serde(default = "default_wake_timeout")]
    pub wake_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_blank_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<StreamingConfig>,
    pub devices: Vec<Device>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingConfig {
    pub mode: StreamingMode,
    #[serde(default)]
    pub end_mode: EndMode,
    #[serde(default)]
    pub sunshine_log: Option<PathBuf>,
    #[serde(default)]
    pub apollo_log: Option<PathBuf>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingMode {
    PowerOff,
    Blank,
}

impl StreamingConfig {
    pub fn policy(&self) -> Mode {
        match self.mode {
            StreamingMode::Blank => Mode::Blank,
            StreamingMode::PowerOff => Mode::PowerOff(self.end_mode),
        }
    }
}

fn default_wake_timeout() -> u64 {
    40
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub ip: Ipv4Addr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<Ipv4Addr>,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "ConnectionMode::is_off")]
    pub connection_mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    #[serde(default)]
    pub wake_mode: WakeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netmask: Option<Ipv4Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_power: Option<HostPower>,
}

/// Per-device lifecycle, session and streaming behavior.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostPower {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blank_when_idle: bool,
    #[serde(default)]
    pub mute_speakers: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub follow_display_power: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdmi_input: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_hdmi: Option<WakeHdmi>,
    #[serde(default)]
    pub power_off_on_unknown_shutdown: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WakeHdmi {
    pub input: u8,
    #[serde(default = "default_hdmi_delay")]
    pub delay_seconds: u8,
}

fn default_hdmi_delay() -> u8 {
    1
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Wss,
    Ws,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Off,
    Persistent,
    Keepalive,
}

impl ConnectionMode {
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    #[default]
    Automatic,
    LimitedBroadcast,
    Direct,
    SubnetBroadcast,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = Self::load_for_preview(path)?;
        let parent = path.parent().unwrap_or(Path::new("."));
        if let Some(streaming) = config.streaming.as_mut() {
            for log in [&mut streaming.sunshine_log, &mut streaming.apollo_log]
                .into_iter()
                .flatten()
            {
                if log.is_relative() {
                    *log = parent.join(&*log);
                }
            }
        }
        for device in &mut config.devices {
            if let Some(certificate) = &mut device.certificate
                && certificate.is_relative()
            {
                *certificate = parent.join(&*certificate);
            }
        }
        Ok(config)
    }

    /// Validate a preview while preserving paths relative to the original config.
    pub fn load_for_preview(path: &Path) -> Result<Self> {
        let file = fs::File::open(path)?;
        let config: Self = serde_json::from_reader(file)?;
        if !(5..=100).contains(&config.wake_timeout_seconds) {
            return Err("wake_timeout_seconds must be between 5 and 100".into());
        }
        if config
            .idle_blank_minutes
            .is_some_and(|minutes| !(1..=240).contains(&minutes))
        {
            return Err("idle_blank_minutes must be between 1 and 240".into());
        }
        let mut ids = BTreeSet::new();
        for device in &config.devices {
            if device
                .host_power
                .as_ref()
                .and_then(|policy| policy.output.as_ref())
                .is_some_and(|output| {
                    output.is_empty() || output.len() > 128 || output.chars().any(char::is_control)
                })
            {
                return Err(format!("{} has an invalid host_power.output name", device.id).into());
            }
            if !valid_id(&device.id) || !ids.insert(device.id.to_ascii_lowercase()) {
                return Err("Device IDs must be unique and contain only ASCII letters, numbers, underscores, or hyphens".into());
            }
            if device.port == Some(0) || device.macs.iter().any(|mac| parse_mac(mac).is_none()) {
                return Err(format!("{} has a malformed port or MAC address", device.id).into());
            }
            if device
                .host_power
                .as_ref()
                .and_then(|power| power.hdmi_input)
                .is_some_and(|input| !(1..=4).contains(&input))
            {
                return Err(format!(
                    "{} host_power.hdmi_input must be between 1 and 4",
                    device.id
                )
                .into());
            }
            if let Some(hdmi) = device.host_power.as_ref().and_then(|power| power.wake_hdmi)
                && (!(1..=4).contains(&hdmi.input) || hdmi.delay_seconds > 30)
            {
                return Err(format!(
                    "{} host_power.wake_hdmi requires input 1–4 and delay_seconds 0–30",
                    device.id
                )
                .into());
            }
            if device
                .source_ip
                .is_some_and(|ip| ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast())
            {
                return Err(format!(
                    "{} source_ip must be a local unicast IPv4 address",
                    device.id
                )
                .into());
            }
            if matches!(device.transport, Transport::Ws) && device.certificate.is_some() {
                return Err(
                    format!("{} specifies a certificate for plaintext WS", device.id).into(),
                );
            }
        }
        Ok(config)
    }

    pub fn targets(&self, selectors: &[String]) -> Result<Vec<usize>> {
        if self.devices.is_empty() {
            return Err("No devices configured".into());
        }
        if selectors.is_empty() {
            return Ok((0..self.devices.len()).collect());
        }
        let mut selected = BTreeSet::new();
        for selector in selectors {
            let selector_lower = selector.to_lowercase();
            // Explicit IDs remain usable even when a friendly name collides.
            if let Some(index) = self
                .devices
                .iter()
                .position(|device| device.id.eq_ignore_ascii_case(selector))
            {
                selected.insert(index);
                continue;
            }
            let mut matches = self
                .devices
                .iter()
                .enumerate()
                .filter(|(_, device)| device.name.to_lowercase() == selector_lower);
            let Some((index, _)) = matches.next() else {
                return Err(format!("Unknown device selector {selector:?}").into());
            };
            if matches.next().is_some() {
                return Err(format!("Ambiguous device name {selector:?}. Use a device ID").into());
            }
            selected.insert(index);
        }
        Ok(selected.into_iter().collect())
    }
}

impl Device {
    pub fn address(&self) -> SocketAddrV4 {
        let default_port = match self.transport {
            Transport::Wss => 3001,
            Transport::Ws => 3000,
        };
        SocketAddrV4::new(self.ip, self.port.unwrap_or(default_port))
    }

    pub fn certificate(&self) -> Result<Option<Vec<u8>>> {
        match self.transport {
            Transport::Ws => Ok(None),
            Transport::Wss => {
                let path = self
                    .certificate
                    .as_ref()
                    .ok_or("WSS requires the certificate field to point to a verified TV leaf certificate in DER format")?;
                let certificate = fs::read(path).map_err(|error| {
                    format!("Cannot read the WSS certificate at {}: {error}. Set certificate to the TV's verified DER certificate", path.display())
                })?;
                Ok(Some(certificate))
            }
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let digits: Vec<u8> = value
        .bytes()
        .filter(|byte| !matches!(byte, b':' | b'-' | b'.' | b' '))
        .collect();
    if digits.len() != 12 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut mac = [0; 6];
    for (octet, pair) in mac.iter_mut().zip(digits.as_chunks::<2>().0) {
        *octet = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(mac)
}

/// Produce a preview of settings the direct CLI understands. Windows secrets
/// never enter the output or the direct CLI's key store.
pub fn windows_preview(path: &Path) -> Result<Value> {
    let input: Value = serde_json::from_reader(fs::File::open(path)?)?;
    let windows = input["LGTV Companion"]
        .as_object()
        .ok_or("Missing Windows preferences node")?;
    let version = windows
        .get("Version")
        .and_then(Value::as_u64)
        .ok_or("Missing Windows configuration version")?;
    let mut warnings = Vec::new();
    let mut config = Config {
        external_api: false,
        streaming: None,
        idle_blank_minutes: None,
        wake_timeout_seconds: windows
            .get("PowerOnTimeOut")
            .and_then(Value::as_i64)
            .unwrap_or(40)
            .clamp(5, 100) as u64,
        devices: Vec::new(),
    };
    let mut ids = BTreeSet::new();
    for (id, record) in input.as_object().unwrap() {
        if id == "LGTV Companion" {
            continue;
        }
        let ip = record["IP"].as_str().and_then(|ip| ip.parse().ok());
        let Some(ip) = ip.filter(|_| valid_id(id) && ids.insert(id.to_ascii_lowercase())) else {
            warnings.push("Skipped a record with an invalid device ID or IPv4 address".into());
            continue;
        };
        let mut seen_macs = BTreeSet::new();
        let macs = record["MAC"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|mac| {
                let parsed = mac.as_str().and_then(parse_mac);
                if parsed.is_none() {
                    warnings.push(format!("{id}: skipped an invalid MAC address"));
                }
                parsed
                    .filter(|mac| seen_macs.insert(*mac))
                    .map(|mac| mac.map(|byte| format!("{byte:02x}")).join(":"))
            })
            .collect();
        let wake_mode = match (version, record["WOL"].as_i64()) {
            (3.., Some(..=1)) => WakeMode::LimitedBroadcast,
            (3.., Some(2)) => WakeMode::Direct,
            (3.., Some(3)) => WakeMode::SubnetBroadcast,
            _ => WakeMode::Automatic,
        };
        let transport = if record["NewSockConnect"] == false {
            Transport::Ws
        } else {
            Transport::Wss
        };
        if matches!(transport, Transport::Wss) {
            warnings.push(format!(
                "{id}: configure a verified DER certificate before connecting"
            ));
        }
        if record["NicLuid"].as_u64().is_some_and(|luid| luid != 0) {
            warnings.push(format!(
                "{id}: Windows network interface selection needs remapping"
            ));
        }
        if record["SessionKey"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
        {
            warnings.push(format!("{id}: pairing key omitted from preview"));
        }
        let name = record["Name"].as_str().unwrap_or_default();
        let name = if name.len() > 18 {
            name.strip_prefix("[LG] webOS TV ").unwrap_or(name)
        } else {
            name
        };
        config.devices.push(Device {
            connection_mode: match record["PersistentConnectionLevel"].as_u64() {
                Some(1) => ConnectionMode::Persistent,
                Some(2) => ConnectionMode::Keepalive,
                _ => ConnectionMode::Off,
            },
            host_power: None,
            source_ip: None,
            id: id.clone(),
            name: name.to_owned(),
            ip,
            transport,
            port: None,
            certificate: None,
            macs,
            wake_mode,
            netmask: Some(
                record["Subnet"]
                    .as_str()
                    .and_then(|mask| mask.parse().ok())
                    .unwrap_or(Ipv4Addr::new(255, 255, 255, 0)),
            ),
        });
    }
    Ok(json!({
        "source_version": version,
        "preview": config,
        "warnings": warnings,
        "scope": "Direct TV settings only. Automation, topology and desktop policies need separate migration."
    }))
}

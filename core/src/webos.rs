//! SSAP frame construction without transport or connection state.

use serde::Serialize;
use serde_json::{Value, json};

use crate::commands::SETTINGS;

const SETTINGS_ENDPOINT: &str = "luna://com.webos.settingsservice/setSystemSettings";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Request {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl Request {
    /// The caller assigns an ID unique among its pending requests.
    pub fn new(id: impl Into<String>, endpoint: &str, payload: Option<Value>) -> Self {
        Self {
            id: id.into(),
            kind: "request",
            uri: format!("ssap://{endpoint}"),
            payload,
        }
    }
}

/// The worker must handle the alert lifecycle after sending this request.
pub fn luna_request(id: impl Into<String>, endpoint: &str, params: Value) -> Request {
    let callback = json!({"uri": endpoint, "params": params});
    Request::new(
        id,
        "system.notifications/createAlert",
        Some(json!({
            "message": " ",
            "buttons": [{"label": "", "onClick": endpoint, "params": params}],
            "onclose": callback,
            "onfail": callback,
        })),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingError {
    UnknownCommand,
    InvalidArgument,
}

/// Build a settings request from the shared Windows command table.
/// Numeric values clamp to the catalog's range. Invalid integers return an error.
pub fn setting_request(
    id: impl Into<String>,
    command: &str,
    argument: &str,
) -> Result<Request, SettingError> {
    let spec = SETTINGS
        .get(&command.to_ascii_lowercase())
        .ok_or(SettingError::UnknownCommand)?;
    let value = if let (Some(min), Some(max)) = (spec.min, spec.max) {
        let number = argument
            .parse::<i64>()
            .map_err(|_| SettingError::InvalidArgument)?
            .clamp(min, max);
        if spec.val_format.as_deref() == Some("int") {
            json!(number)
        } else {
            json!(number.to_string())
        }
    } else {
        let canonical = spec
            .argument
            .as_deref()
            .unwrap_or_default()
            .split_ascii_whitespace()
            .find(|candidate| candidate.eq_ignore_ascii_case(argument))
            .ok_or(SettingError::InvalidArgument)?;
        json!(canonical)
    };
    let settings = if let Some((key, input)) = spec.setting.split_once("_hdmi") {
        json!({key: {format!("hdmi{input}"): value}})
    } else {
        json!({&spec.setting: value})
    };

    Ok(luna_request(
        id,
        SETTINGS_ENDPOINT,
        json!({"category": spec.category, "settings": settings}),
    ))
}

pub const REGISTRATION_ID: &str = "register_0";

/// Registration uses the prompt manifest from Windows 5.7.0.
pub fn registration_frame(key: Option<&str>) -> Value {
    let manifest: Value = serde_json::from_str(include_str!("registration-manifest.json"))
        .expect("valid bundled registration manifest");
    let mut payload = json!({"forcePairing": false, "pairingType": "PROMPT", "manifest": manifest});
    if let Some(key) = key {
        payload["client-key"] = json!(key);
    }
    json!({"type": "register", "id": REGISTRATION_ID, "payload": payload})
}

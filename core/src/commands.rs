//! Windows command inventory and generated settings contracts.

use std::{collections::BTreeMap, sync::LazyLock};

use serde::{Deserialize, Serialize};

const SETTINGS_SOURCE: &str = include_str!("../../../Common/lg_api_commands.h");
const BUTTONS_SOURCE: &str = include_str!("../../../Common/lg_api_buttons.h");

/// Windows execution paths. ServiceInternal commands were session reports, not public CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsMode {
    Console,
    Service,
    Both,
    ServiceInternal,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Builtin {
    pub names: &'static [&'static str],
    pub mode: WindowsMode,
    /// Required value arguments, in order, before any device selectors.
    pub arguments: &'static [&'static str],
}

macro_rules! builtin {
    ($mode:ident, [$($name:literal),+], [$($argument:literal),*]) => {
        Builtin {
            names: &[$($name),+],
            mode: WindowsMode::$mode,
            arguments: &[$($argument),*],
        }
    };
}

/// Branches shared by both executables use Both. Aliases share one row when
/// their argument shapes agree. The registry describes the reference, not Linux support.
pub const WINDOWS_BUILTINS: &[Builtin] = &[
    builtin!(Console, ["help", "?", "h"], []),
    builtin!(Console, ["output"], ["default | friendly | key [key-name]"]),
    builtin!(Console, ["od", "of"], []),
    builtin!(Console, ["ok"], ["key-name"]),
    builtin!(Both, ["poweron", "screenon"], []),
    builtin!(Both, ["poweroff"], []),
    builtin!(Both, ["screenoff"], []),
    builtin!(Service, ["autoenable", "autodisable"], []),
    builtin!(Service, ["streaming_connect", "streaming_disconnect"], []),
    builtin!(Service, ["clearlog"], []),
    builtin!(Service, ["idle", "unidle"], []),
    builtin!(
        ServiceInternal,
        ["daemon"],
        ["event", "event-specific arguments"]
    ),
    builtin!(Both, ["sethdmi"], ["integer 1–4"]),
    builtin!(Both, ["sethdmi1", "sethdmi2", "sethdmi3", "sethdmi4"], []),
    builtin!(Both, ["set_input_type"], ["input", "icon", "label"]),
    builtin!(Both, ["volume"], ["integer 0–100"]),
    builtin!(Both, ["mute", "unmute"], []),
    builtin!(Both, ["button"], ["button from catalog"]),
    builtin!(Console, ["button_nocheck"], ["button string"]),
    builtin!(Both, ["freesyncinfo"], []),
    builtin!(Both, ["servicemenu"], []),
    builtin!(
        Both,
        ["servicemenu_legacy_enable", "servicemenu_legacy_disable"],
        []
    ),
    builtin!(
        Both,
        ["servicemenu_tpc_enable", "servicemenu_tpc_disable"],
        []
    ),
    builtin!(
        Both,
        ["servicemenu_gsr_enable", "servicemenu_gsr_disable"],
        []
    ),
    builtin!(
        Both,
        ["settings_picture", "settings_other", "settings_options"],
        ["JSON"]
    ),
    builtin!(Both, ["request"], ["SSAP endpoint"]),
    builtin!(Both, ["request_with_param"], ["SSAP endpoint", "JSON"]),
    builtin!(Both, ["start_app", "close_app"], ["app ID"]),
    builtin!(Both, ["start_app_with_param"], ["app ID", "JSON"]),
    builtin!(Both, ["set_curve_preset"], ["flat | 0–3"]),
    builtin!(
        Both,
        ["adjust_curve_preset"],
        ["preset 1–3", "integer 0–100"]
    ),
    builtin!(Both, ["set_curvature"], ["flat | integer 0–100"]),
    builtin!(Console, ["get_system_settings"], ["category", "JSON keys"]),
];

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Setting {
    pub category: String,
    pub setting: String,
    pub argument: Option<String>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub val_format: Option<String>,
    pub log_message: String,
}

pub static SETTINGS: LazyLock<BTreeMap<String, Setting>> =
    LazyLock::new(|| decode_catalog(SETTINGS_SOURCE));

pub static BUTTONS: LazyLock<Vec<String>> = LazyLock::new(|| {
    #[derive(Deserialize)]
    struct Catalog {
        #[serde(rename = "Buttons")]
        buttons: Vec<String>,
    }
    decode_catalog::<Catalog>(BUTTONS_SOURCE).buttons
});

fn decode_catalog<T: serde::de::DeserializeOwned>(source: &str) -> T {
    let source = source.trim();
    let json = source
        .strip_prefix("R\"(")
        .and_then(|source| source.strip_suffix(")\""))
        .expect("Windows catalog uses a C++ raw string literal");
    serde_json::from_str(json).expect("Windows catalog contains valid JSON")
}

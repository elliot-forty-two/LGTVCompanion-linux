use std::io::{self, Write};

use lgtv_companion_core::{
    commands::{BUTTONS, SETTINGS, WINDOWS_BUILTINS, WindowsMode},
    webos::{Request, SettingError, luna_request, setting_request},
};
use serde::Serialize;
use serde_json::{Value, json, ser::PrettyFormatter};

use crate::tv::Result;

pub enum Step {
    Format(Output),
    Command {
        action: Box<Action>,
        selectors: Vec<String>,
    },
}

pub enum Action {
    Ssap(Request),
    Luna(Request),
    LunaSequence(Vec<Request>),
    Button(String),
    Wake,
    SetAutomation(bool),
    HostPowerOff,
    StreamPowerOff,
    StreamBlank,
    IdleBlank,
    IdleRestore,
}

pub enum Output {
    Compact,
    Friendly,
    Key(String),
}

pub fn parse(args: &[String], daemon: bool) -> Result<Vec<Step>> {
    let mut input = args.iter().map(String::as_str).peekable();
    let mut steps = Vec::new();
    while let Some(token) = input.next() {
        let name = token
            .strip_prefix('-')
            .ok_or_else(|| format!("Expected a command, got {token:?}"))?
            .to_ascii_lowercase();
        let builtin = WINDOWS_BUILTINS
            .iter()
            .find(|entry| entry.names.contains(&name.as_str()));
        if !(daemon && matches!(name.as_str(), "autoenable" | "autodisable"))
            && builtin.is_some_and(|entry| {
                matches!(
                    entry.mode,
                    WindowsMode::Service | WindowsMode::ServiceInternal
                )
            })
        {
            return Err(format!("-{name} requires the Companion daemon or session agent").into());
        }
        let count = match builtin {
            Some(entry) => entry.arguments.len(),
            None if SETTINGS.contains_key(&name) => 1,
            None => return Err(format!("Unknown command -{name}").into()),
        };
        let mut values: Vec<_> = input.by_ref().take(count).collect();
        if values.len() < count {
            return Err(format!("Missing arguments for -{name}").into());
        }
        if name == "output" && values[0].eq_ignore_ascii_case("key") {
            values.push(input.next().ok_or("Missing key name for -output key")?);
        }
        if let Some(format) = output(&name, &values)? {
            steps.push(Step::Format(format));
            continue;
        }

        let action = action(&name, &values)?;
        let mut selectors = Vec::new();
        while let Some(next) = input.peek() {
            if *next == "--" {
                input.next();
                selectors.push(
                    input
                        .next()
                        .ok_or("Expected a selector after --")?
                        .to_owned(),
                );
            } else if next.starts_with('-') {
                break;
            } else {
                selectors.push(input.next().unwrap().to_owned());
            }
        }
        steps.push(Step::Command {
            action: Box::new(action),
            selectors,
        });
    }
    Ok(steps)
}

fn output(name: &str, args: &[&str]) -> Result<Option<Output>> {
    let format = match name {
        "od" => Output::Compact,
        "of" => Output::Friendly,
        "ok" => Output::Key(args[0].into()),
        "output" => match args[0].to_ascii_lowercase().as_str() {
            "default" => Output::Compact,
            "friendly" => Output::Friendly,
            "key" => Output::Key(args[1].into()),
            _ => return Err("Expected default, friendly, or key for -output".into()),
        },
        _ => return Ok(None),
    };
    Ok(Some(format))
}

pub fn hdmi_action(input: u8) -> Action {
    Action::Ssap(Request::new(
        "",
        "system.launcher/launch",
        Some(json!({"id": format!("com.webos.app.hdmi{input}")})),
    ))
}

fn action(name: &str, args: &[&str]) -> Result<Action> {
    let request = |uri, payload| Action::Ssap(Request::new("", uri, payload));
    let launcher = |id: &str| request("system.launcher/launch", Some(json!({"id": id})));
    let set_hdmi = |port: &str| {
        if let [input @ b'1'..=b'4'] = port.as_bytes() {
            Ok(hdmi_action(input - b'0'))
        } else {
            Err("HDMI input must be between 1 and 4".into())
        }
    };
    Ok(match name {
        "poweron" | "screenon" => Action::Wake,
        "autoenable" | "autodisable" => Action::SetAutomation(name == "autoenable"),
        "poweroff" => request("system/turnOff", None),
        "screenoff" => request("com.webos.service.tvpower/power/turnOffScreen", None),
        "sethdmi" => return set_hdmi(args[0]),
        "sethdmi1" | "sethdmi2" | "sethdmi3" | "sethdmi4" => return set_hdmi(&name[7..]),
        "volume" => request(
            "audio/setVolume",
            Some(json!({"volume": args[0].parse::<i64>()?.clamp(0, 100)})),
        ),
        "mute" => request("audio/setMute", Some(json!({"mute": "true"}))),
        "unmute" => request("audio/setMute", None),
        "request" | "request_with_param" => {
            let payload = args
                .get(1)
                .map(|value| serde_json::from_str(value))
                .transpose()?;
            request(args[0], payload)
        }
        "get_system_settings" => {
            let keys: Vec<String> = serde_json::from_str(args[1])?;
            request(
                "settings/getSystemSettings",
                Some(json!({"category": args[0], "keys": keys})),
            )
        }
        "start_app" => launcher(args[0]),
        "close_app" => request("system.launcher/close", Some(json!({"id": args[0]}))),
        "start_app_with_param" => {
            let params: Value = serde_json::from_str(args[1])?;
            request(
                "system.launcher/launch",
                Some(json!({"id": args[0], "params": params})),
            )
        }
        "freesyncinfo" => request(
            "system.launcher/launch",
            Some(
                json!({"id": "com.webos.app.tvhotkey", "params": {"activateType": "freesync-info"}}),
            ),
        ),
        "button" => {
            let button = BUTTONS
                .iter()
                .find(|button| button.eq_ignore_ascii_case(args[0]))
                .ok_or("Unknown remote button")?;
            Action::Button(button.clone())
        }
        "button_nocheck" => {
            if args[0].is_empty() || args[0].chars().any(char::is_control) {
                return Err("Button name must be a non-empty line".into());
            }
            Action::Button(args[0].to_owned())
        }
        "servicemenu" => request(
            "system.launcher/launch",
            Some(json!({"id": "com.webos.app.factorywin", "params": {"irKey": "inStart"}})),
        ),
        "servicemenu_legacy_enable" | "servicemenu_legacy_disable" => {
            change_settings("other", json!({"svcMenuFlag": name.ends_with("_disable")}))
        }
        "servicemenu_tpc_enable" | "servicemenu_tpc_disable" => Action::Luna(luna_request(
            "",
            "luna://com.webos.service.oledepl/setTemporalPeakControl",
            json!({"enable": name.ends_with("_enable")}),
        )),
        "servicemenu_gsr_enable" | "servicemenu_gsr_disable" => Action::Luna(luna_request(
            "",
            "luna://com.webos.service.oledepl/setGlobalStressReduction",
            json!({"enable": name.ends_with("_enable")}),
        )),
        "set_curve_preset" => Action::Luna(curve_preset(args[0])?),
        "adjust_curve_preset" => Action::Luna(adjust_curve(
            args[0],
            args[1].parse::<i64>()?.clamp(0, 100),
        )?),
        "set_curvature" => {
            let percent = if args[0].eq_ignore_ascii_case("flat") {
                0
            } else {
                args[0].parse::<i64>()?.clamp(0, 100)
            };
            if percent == 0 {
                Action::Luna(curve_preset("0")?)
            } else {
                Action::LunaSequence(vec![
                    adjust_curve("2", percent)?,
                    adjust_curve("3", percent)?,
                    curve_preset("3")?,
                    curve_preset("2")?,
                ])
            }
        }
        "settings_picture" | "settings_other" | "settings_options" => {
            let category = match name {
                "settings_picture" => "picture",
                "settings_other" => "other",
                _ => "option",
            };
            let settings: serde_json::Map<String, Value> = serde_json::from_str(args[0])?;
            change_settings(category, Value::Object(settings))
        }
        "set_input_type" => Action::Luna(luna_request(
            "",
            "luna://com.webos.service.eim/setDeviceInfo",
            json!({"id": args[0], "icon": format!("{}.png", args[1]), "label": args[2]}),
        )),
        _ => match setting_request("", name, args.first().copied().unwrap_or_default()) {
            Ok(request) => Action::Luna(request),
            Err(SettingError::InvalidArgument) => {
                return Err(format!("Invalid argument for -{name}").into());
            }
            Err(SettingError::UnknownCommand) => {
                return Err(format!("-{name} is pending implementation").into());
            }
        },
    })
}

fn change_settings(category: &str, settings: Value) -> Action {
    Action::Luna(luna_request(
        "",
        "luna://com.webos.settingsservice/setSystemSettings",
        json!({"category": category, "settings": settings}),
    ))
}

fn curve_preset(preset: &str) -> Result<Request> {
    let kind = if preset == "0" || preset.eq_ignore_ascii_case("flat") {
        "flat".to_owned()
    } else {
        curvature_type(preset)?
    };
    Ok(luna_request(
        "",
        "luna://com.webos.service.rollingscreen/changeCurve",
        json!({"type": kind, "reason": "com.pal.app.settings"}),
    ))
}

fn adjust_curve(preset: &str, percent: i64) -> Result<Request> {
    Ok(luna_request(
        "",
        "luna://com.webos.service.rollingscreen/updateCurvature",
        json!({"type": curvature_type(preset)?, "value": format!("{percent}%")}),
    ))
}

fn curvature_type(preset: &str) -> Result<String> {
    if matches!(preset, "1" | "2" | "3") {
        Ok(format!("curvature{preset}"))
    } else {
        Err("Curvature preset must be between 1 and 3".into())
    }
}

impl Output {
    pub fn write(&self, mut dest: impl Write, value: &Value) -> io::Result<()> {
        match self {
            Self::Compact => serde_json::to_writer(&mut dest, value)?,
            Self::Friendly => value.serialize(&mut serde_json::Serializer::with_formatter(
                &mut dest,
                PrettyFormatter::with_indent(b"    "),
            ))?,
            Self::Key(key) => {
                let mut first = true;
                write_matches(&mut dest, key, value, &mut first)?;
            }
        }
        writeln!(dest)
    }
}

fn write_matches(
    dest: &mut impl Write,
    key: &str,
    value: &Value,
    first: &mut bool,
) -> io::Result<()> {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                if name == key {
                    if !*first {
                        writeln!(dest)?;
                    }
                    *first = false;
                    serde_json::to_writer(&mut *dest, value)?;
                }
                write_matches(dest, key, value, first)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                write_matches(dest, key, value, first)?;
            }
        }
        _ => {}
    }
    Ok(())
}

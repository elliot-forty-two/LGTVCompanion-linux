//! Fixtures transcribed from Windows 5.7.0 at a37d054.

use std::{collections::BTreeSet, net::Ipv4Addr, time::Duration};

use lgtv_companion_core::{
    commands::{BUTTONS, SETTINGS, WINDOWS_BUILTINS, WindowsMode},
    webos::{Request, SettingError, setting_request},
    wol,
};
use serde_json::{Value, json};

// Extracted from Console.cpp, companion.cpp, and Docs/Commandline.md.
// Both dispatch lists include the names handled by the sethdmi prefix branch.
const CONSOLE_COMMANDS: &str = include_str!("fixtures/windows-console-commands.txt");
const SERVICE_COMMANDS: &str = include_str!("fixtures/windows-service-commands.txt");
const DOCUMENTED_COMMANDS: &str = include_str!("fixtures/windows-documented-commands.txt");

#[test]
fn covers_builtin_dispatch_in_both_windows_executables() {
    for (commands, mode) in [
        (CONSOLE_COMMANDS, WindowsMode::Console),
        (SERVICE_COMMANDS, WindowsMode::Service),
    ] {
        let expected: BTreeSet<_> = commands.lines().collect();
        let registry: BTreeSet<_> = WINDOWS_BUILTINS
            .iter()
            .filter(|builtin| {
                builtin.mode == mode
                    || builtin.mode == WindowsMode::Both
                    || (mode == WindowsMode::Service
                        && builtin.mode == WindowsMode::ServiceInternal)
            })
            .flat_map(|builtin| builtin.names.iter().copied())
            .collect();
        assert_eq!(registry, expected);
    }
}

#[test]
fn covers_documented_command_names() {
    let builtins: Vec<_> = WINDOWS_BUILTINS
        .iter()
        .flat_map(|entry| entry.names)
        .collect();
    assert_eq!(
        builtins.len(),
        builtins.iter().copied().collect::<BTreeSet<_>>().len()
    );
    for name in DOCUMENTED_COMMANDS.lines() {
        if let Some(prefix) = name.strip_suffix('X') {
            for input in 1..=4 {
                assert!(SETTINGS.contains_key(&format!("{prefix}{input}")));
            }
        } else {
            assert!(
                builtins.contains(&&name) || SETTINGS.contains_key(name),
                "{name}"
            );
        }
    }
    assert!(BUTTONS.iter().any(|button| button == "IN_START"));
    assert!(BUTTONS.iter().any(|button| button == "BENDABLE"));
}

#[test]
fn every_generated_setting_builds_a_request() {
    for (command, spec) in SETTINGS.iter() {
        let example = match (&spec.argument, spec.min, spec.max) {
            (Some(args), None, None) => args.split_ascii_whitespace().next().unwrap().to_owned(),
            (None, Some(min), Some(max)) if min <= max => min.to_string(),
            _ => panic!("Invalid argument specification for {command}"),
        };
        let frame = setting_request("test", command, &example).unwrap();
        assert_eq!(frame.uri, "ssap://system.notifications/createAlert");
    }
}

#[test]
fn webos_settings_frame_matches_windows_luna_bridge() {
    let frame = setting_request("luna_request", "GameMode_HDMI1", "ON").unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/windows-gamemode-hdmi1.json")).unwrap();
    assert_eq!(serde_json::to_value(frame).unwrap(), expected);
}

#[test]
fn settings_keep_windows_types_and_catalog_spelling() {
    for (command, value, key, expected) in [
        ("colortemperature", "-99", "colorTemperature", json!("-50")),
        (
            "ambientlight_staticmodecolor1",
            "12",
            "staticModeColor1",
            json!(12),
        ),
        (
            "picturemode",
            "HDRFILMMAKER",
            "pictureMode",
            json!("hdrFilmMaker"),
        ),
    ] {
        let payload = setting_request("1", command, value)
            .unwrap()
            .payload
            .unwrap();
        assert_eq!(payload["onclose"]["params"]["settings"][key], expected);
    }
    assert_eq!(
        setting_request("2", "backlight", "invalid"),
        Err(SettingError::InvalidArgument)
    );
    assert_eq!(
        setting_request("3", "unsupported", "1"),
        Err(SettingError::UnknownCommand)
    );
}

#[test]
fn ssap_frame_preserves_payload_case_and_omits_absent_payload() {
    assert_eq!(
        serde_json::to_value(Request::new("1", "audio/getVolume", None)).unwrap(),
        json!({"id": "1", "type": "request", "uri": "ssap://audio/getVolume"})
    );
    assert_eq!(
        serde_json::to_value(Request::new(
            "2",
            "system.launcher/launch",
            Some(json!({"id": "Sample.App", "params": {"label": "Écran - 1"}}))
        ))
        .unwrap(),
        json!({
            "id": "2",
            "type": "request",
            "uri": "ssap://system.launcher/launch",
            "payload": {"id": "Sample.App", "params": {"label": "Écran - 1"}}
        })
    );
}

#[test]
fn windows_wake_packet_and_destination() {
    let packet = wol::magic_packet([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let expected: Vec<u8> = include_str!("fixtures/windows-wol-packet.hex")
        .split_ascii_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect();
    assert_eq!(packet.as_slice(), expected);
    assert_eq!(wol::PORT, 9);
    assert_eq!(wol::REPEAT_INTERVAL, Duration::from_secs(1));
    assert_eq!(
        wol::subnet_broadcast(
            Ipv4Addr::new(192, 0, 2, 41),
            Ipv4Addr::new(255, 255, 255, 224)
        ),
        Ipv4Addr::new(192, 0, 2, 63)
    );
}

#[test]
fn registration_frames_match_the_windows_pairing_manifest() {
    use lgtv_companion_core::webos::registration_frame;
    // Decoded from the two LG_HANDSHAKE_*_V3 macros in Common/lg_api.h.
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/windows-registration-frames.json")).unwrap();
    assert_eq!(registration_frame(None), expected["unpaired"]);
    assert_eq!(registration_frame(Some("#CLIENTKEY#")), expected["paired"]);
}

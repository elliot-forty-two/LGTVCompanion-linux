use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
};

use serde::Serialize;

use crate::config::{Config, ConnectionMode, Device, Transport, WakeMode, parse_mac};

use super::Discovery;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Refresh,
    Append,
    Replace,
}

#[derive(Debug, Serialize)]
pub struct IpChange {
    pub id: String,
    pub previous_ip: Ipv4Addr,
    pub discovered_ip: Ipv4Addr,
}

#[derive(Serialize)]
pub struct Preview {
    pub mode: Mode,
    pub changes: Vec<IpChange>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub warnings: Vec<String>,
    pub preview: Config,
}

/// Plan changes without touching the configuration file, certificate pins or keys.
pub fn stage(mut config: Config, found: &Discovery, mode: Mode) -> Preview {
    let mut warnings = Vec::new();
    let mut observations = BTreeMap::<_, Vec<_>>::new();
    let mut configured = BTreeMap::<_, BTreeSet<_>>::new();
    let mut matched_ips = BTreeMap::<usize, BTreeSet<Ipv4Addr>>::new();
    let mut preserve = BTreeSet::new();
    let mut new_candidates = Vec::new();

    for (index, device) in config.devices.iter().enumerate() {
        for mac in device.macs.iter().filter_map(|mac| parse_mac(mac)) {
            configured.entry(mac).or_default().insert(index);
        }
    }
    for tv in &found.televisions {
        if let Some(mac) = tv.mac.as_deref().and_then(parse_mac) {
            observations.entry(mac).or_default().push(tv);
        } else {
            warnings.push(format!(
                "{} on {} has no confirmed MAC; complete its setup manually",
                tv.ip, tv.interface.name
            ));
            preserve.extend(
                config
                    .devices
                    .iter()
                    .enumerate()
                    .filter(|(_, device)| device.ip == tv.ip)
                    .map(|(index, _)| index),
            );
        }
    }
    for (mac, observations) in observations {
        let endpoints: BTreeSet<_> = observations.iter().map(|tv| tv.ip).collect();
        let matches = configured.get(&mac);
        if endpoints.len() != 1 || matches.is_some_and(|indices| indices.len() != 1) {
            warnings.push(format!(
                "MAC {} has conflicting discovery addresses or configured TV IDs; resolve it manually",
                observations[0].mac.as_deref().unwrap_or_default()
            ));
            preserve.extend(matches.into_iter().flatten().copied());
            continue;
        }

        if let Some(index) = matches.and_then(BTreeSet::first) {
            matched_ips.entry(*index).or_default().extend(endpoints);
        } else if mode != Mode::Refresh {
            new_candidates.push((mac, observations[0]));
        }
    }

    let mut changes = Vec::new();
    let mut usable = 0;
    for (index, addresses) in matched_ips {
        let device = &config.devices[index];
        let Some(&ip) = addresses.first() else {
            continue;
        };
        preserve.insert(index);
        if addresses.len() != 1 {
            warnings.push(format!(
                "{} was observed at multiple IP addresses; kept its configured address",
                device.id
            ));
        } else if ip != device.ip
            && overlaps_address(
                config
                    .devices
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index)
                    .map(|(_, device)| device),
                ip,
                device.source_ip,
            )
        {
            warnings.push(format!(
                "{} cannot be refreshed to {ip} because that address is already configured",
                device.id
            ));
        } else {
            usable += 1;
            if ip != device.ip {
                changes.push(IpChange {
                    id: device.id.clone(),
                    previous_ip: device.ip,
                    discovered_ip: ip,
                });
                config.devices[index].ip = ip;
            }
        }
    }

    let mut ids: BTreeSet<_> = config
        .devices
        .iter()
        .map(|device| device.id.to_ascii_lowercase())
        .collect();
    let mut added = Vec::new();
    let mut new_devices = Vec::new();
    for (mac, candidate) in new_candidates {
        let id = format!("TV-{}", mac.map(|byte| format!("{byte:02x}")).join(""));
        if !ids.insert(id.to_ascii_lowercase()) {
            warnings.push(format!(
                "Cannot add {} because {id} is already configured; resolve the ID manually",
                candidate.ip
            ));
            continue;
        }
        if overlaps_address(
            config
                .devices
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    mode != Mode::Replace || !found.complete || preserve.contains(index)
                })
                .map(|(_, device)| device)
                .chain(&new_devices),
            candidate.ip,
            Some(candidate.interface.source_ip),
        ) {
            warnings.push(format!(
                "Cannot add {id} at {} because that address is already configured",
                candidate.ip
            ));
            continue;
        }
        new_devices.push(Device {
            id: id.clone(),
            name: candidate.name.clone(),
            ip: candidate.ip,
            source_ip: Some(candidate.interface.source_ip),
            transport: Transport::Wss,
            connection_mode: ConnectionMode::Off,
            port: None,
            certificate: None,
            macs: vec![mac.map(|byte| format!("{byte:02x}")).join(":")],
            wake_mode: WakeMode::Automatic,
            netmask: Some(candidate.interface.netmask),
            host_power: None,
        });
        added.push(id);
    }

    usable += added.len();
    let mut removed = Vec::new();
    if mode == Mode::Replace {
        if !found.complete {
            warnings.push("Scan was incomplete; kept the existing TV list".into());
        } else if usable == 0 {
            warnings.push("No usable TVs were found; kept the entire configuration".into());
        } else {
            let mut index = 0;
            config.devices.retain(|device| {
                let keep = preserve.contains(&index);
                index += 1;
                if !keep {
                    removed.push(device.id.clone());
                }
                keep
            });
        }
    }
    config.devices.extend(new_devices);
    Preview {
        mode,
        changes,
        added,
        removed,
        warnings,
        preview: config,
    }
}

fn overlaps_address<'a>(
    devices: impl IntoIterator<Item = &'a Device>,
    ip: Ipv4Addr,
    source: Option<Ipv4Addr>,
) -> bool {
    devices.into_iter().any(|other| {
        other.ip == ip
            && (other.source_ip.is_none() || source.is_none() || other.source_ip == source)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{Interface, Television};
    use serde_json::{Value, json};

    fn config() -> Config {
        serde_json::from_value(json!({
            "streaming": {"mode":"blank","sunshine_log":"sunshine.log"},
            "devices":[
                {"id":"Device1","name":"Known TV","ip":"192.0.2.10","macs":["02-11-22-33-44-55"],
                    "certificate":"living-room.der","host_power":{"hdmi_input":2,"follow_display_power":true}},
                {"id":"Device2","ip":"192.0.2.11","macs":["02:11:22:33:44:66"]}
            ]
        })).unwrap()
    }

    fn found(entries: &[(&str, Option<&str>)]) -> Discovery {
        Discovery {
            interfaces: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            televisions: entries
                .iter()
                .map(|(ip, mac)| Television {
                    ip: ip.parse().unwrap(),
                    interface: Interface {
                        name: "eth0".into(),
                        source_ip: "192.0.2.5".parse().unwrap(),
                        netmask: "255.255.255.0".parse().unwrap(),
                    },
                    mac: mac.map(str::to_owned),
                    name: "Discovered TV".into(),
                    model: None,
                    udn: None,
                })
                .collect(),
        }
    }

    #[test]
    fn refresh_changes_only_the_ip_and_preserves_relative_paths() {
        let discovered = found(&[("192.0.2.42", Some("02:11:22:33:44:55"))]);
        let mut original = serde_json::to_value(config()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("devices.json");
        let saved = original.to_string();
        std::fs::write(&path, &saved).unwrap();
        original["devices"][0]["ip"] = json!("192.0.2.42");
        let preview = stage(
            Config::load_for_preview(&path).unwrap(),
            &discovered,
            Mode::Refresh,
        );
        assert_eq!(preview.changes[0].id, "Device1");
        assert_eq!(serde_json::to_value(preview.preview).unwrap(), original);
        assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
    }

    #[test]
    fn append_deduplicates_macs_and_does_not_stage_incomplete_tvs() {
        let discovered = found(&[
            ("192.0.2.30", Some("02:11:22:33:44:77")),
            ("192.0.2.30", Some("02-11-22-33-44-77")),
            ("192.0.2.44", None),
        ]);
        let preview = stage(config(), &discovered, Mode::Append);
        assert_eq!(preview.added, ["TV-021122334477"]);
        assert_eq!(preview.preview.devices.len(), 3);
        assert!(preview.warnings[0].contains("no confirmed MAC"));
        let staged = &preview.preview.devices[2];
        assert!(staged.certificate.is_none() && staged.host_power.is_none());
        assert!(matches!(staged.transport, Transport::Wss));
    }

    #[test]
    fn replace_preserves_the_list_after_empty_incomplete_or_ambiguous_scans() {
        let inputs = [
            found(&[]),
            found(&[("192.0.2.30", None)]),
            found(&[
                ("192.0.2.30", Some("02:11:22:33:44:55")),
                ("192.0.2.31", Some("02:11:22:33:44:55")),
            ]),
        ];
        for input in inputs {
            let original = serde_json::to_value(config()).unwrap();
            let preview = stage(config(), &input, Mode::Replace);
            assert!(preview.removed.is_empty());
            assert_eq!(serde_json::to_value(preview.preview).unwrap(), original);
        }
        let mut incomplete = found(&[("192.0.2.64", Some("02:11:22:33:44:55"))]);
        incomplete.complete = false;
        assert!(
            stage(config(), &incomplete, Mode::Replace)
                .removed
                .is_empty()
        );
    }

    #[test]
    fn replacement_at_the_same_ip_gets_a_distinct_id() {
        let discovered = found(&[("192.0.2.10", Some("02:11:22:33:44:99"))]);
        let preview = stage(config(), &discovered, Mode::Replace);
        assert_eq!(preview.removed, ["Device1", "Device2"]);
        let output: Value = serde_json::to_value(preview.preview).unwrap();
        assert_eq!(output["devices"][0]["id"], "TV-021122334499");
        assert!(output["devices"][0].get("certificate").is_none());
    }
}

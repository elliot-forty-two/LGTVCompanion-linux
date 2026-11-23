//! Read-only, on-link IPv4 discovery of LG webOS television descriptions.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use reqwest::{Client, Url, redirect::Policy};
use roxmltree::Document;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::UdpSocket,
    process::Command,
    time::{Instant, timeout, timeout_at},
};

use crate::{config::parse_mac, tv::Result};

pub mod preview;

const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
const WEBOS: &str = "urn:lge-com:service:webos-second-screen:1";
const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const SEARCH_TIME: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DATAGRAM: usize = 4096;
const MAX_CANDIDATES: usize = 64;
const MAX_XML: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Interface {
    pub name: String,
    pub source_ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

impl Interface {
    fn contains(&self, peer: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        (u32::from(self.source_ip) & mask) == (u32::from(peer) & mask)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Television {
    pub ip: Ipv4Addr,
    pub interface: Interface,
    pub mac: Option<String>,
    pub name: String,
    pub model: Option<String>,
    pub udn: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Discovery {
    pub interfaces: Vec<Interface>,
    pub televisions: Vec<Television>,
    pub warnings: Vec<String>,
    pub complete: bool,
}

pub async fn scan(selected: &[String]) -> Result<Discovery> {
    let addresses = ip_json(&["-j", "-4", "address", "show", "up"]).await?;
    let interfaces = eligible_interfaces(addresses, selected);
    if interfaces.is_empty() {
        return Err("No eligible multicast IPv4 interface was found".into());
    }
    for name in selected {
        if !interfaces.iter().any(|interface| interface.name == *name) {
            return Err(
                format!("Interface {name:?} has no eligible multicast IPv4 address").into(),
            );
        }
    }

    let results = stream::iter(interfaces.iter().cloned())
        .map(|interface| async move {
            let result = scan_interface(&interface, GROUP, SEARCH_TIME).await;
            (interface, result)
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    let mut warnings = Vec::new();
    let mut televisions = Vec::new();
    let mut completed = false;
    for (interface, result) in results {
        match result {
            Ok((found, limited)) => {
                completed = true;
                if limited {
                    warnings.push(format!("{}: SSDP candidate limit reached", interface.name));
                }
                televisions.extend(found);
            }
            Err(error) => warnings.push(format!(
                "{} ({}): {error}",
                interface.name, interface.source_ip
            )),
        }
    }
    warnings.sort();
    if !completed {
        return Err(format!(
            "SSDP failed on every selected interface ({})",
            warnings.join(", ")
        )
        .into());
    }

    if !televisions.is_empty() {
        match ip_json::<Vec<Neighbour>>(&["-j", "-4", "neigh", "show"]).await {
            Ok(neighbours) => assign_macs(&mut televisions, neighbours),
            Err(error) => warnings.push(format!("Cannot read the IPv4 neighbour table: {error}")),
        }
    }
    deduplicate(&mut televisions);
    Ok(Discovery {
        complete: warnings.is_empty(),
        interfaces,
        televisions,
        warnings,
    })
}

async fn scan_interface(
    interface: &Interface,
    target: SocketAddrV4,
    window: Duration,
) -> Result<(Vec<Television>, bool)> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddrV4::new(interface.source_ip, 0).into())?;
    socket.set_multicast_if_v4(&interface.source_ip)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket.into())?;

    for service in [WEBOS, RENDERER] {
        let probe = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {target}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {service}\r\n\r\n"
        );
        timeout(IO_TIMEOUT, socket.send_to(probe.as_bytes(), target)).await??;
    }

    let mut candidates = BTreeMap::new();
    let mut buffer = [0; MAX_DATAGRAM + 1];
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let Ok(received) = timeout_at(deadline, socket.recv_from(&mut buffer)).await else {
            break;
        };
        let (size, sender) = received?;
        let SocketAddr::V4(sender) = sender else {
            continue;
        };
        let ip = *sender.ip();
        if size <= MAX_DATAGRAM
            && ip != interface.source_ip
            && usable_ip(ip)
            && interface.contains(ip)
            && let Some(url) = response_location(&buffer[..size], ip)
        {
            if candidates.contains_key(&url)
                || candidates.values().filter(|peer| **peer == ip).count() >= 4
            {
                continue;
            }
            if candidates.len() == MAX_CANDIDATES {
                break;
            }
            candidates.insert(url, ip);
        }
    }
    let limited = candidates.len() == MAX_CANDIDATES;
    if candidates.is_empty() {
        return Ok((Vec::new(), limited));
    }

    let http = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .local_address(IpAddr::V4(interface.source_ip))
        .timeout(IO_TIMEOUT)
        .build()?;
    let mut found = stream::iter(candidates)
        .map(|(url, ip)| {
            let http = &http;
            async move {
                let description = fetch_description(http, url).await?;
                Some(Television {
                    ip,
                    interface: interface.clone(),
                    mac: None,
                    name: description.name,
                    model: description.model,
                    udn: description.udn,
                })
            }
        })
        .buffer_unordered(8)
        .filter_map(std::future::ready)
        .collect::<Vec<_>>()
        .await;
    // A device can advertise multiple services with distinct XML endpoints.
    found.sort_by(|a, b| {
        (a.ip, a.model.is_none(), &a.name).cmp(&(b.ip, b.model.is_none(), &b.name))
    });
    found.dedup_by_key(|tv| tv.ip);
    Ok((found, limited))
}

fn usable_ip(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
}

fn response_location(packet: &[u8], sender: Ipv4Addr) -> Option<Url> {
    let text = std::str::from_utf8(packet).ok()?;
    let mut lines = text.lines();
    let mut status = lines.next()?.split_ascii_whitespace();
    if !matches!(status.next(), Some("HTTP/1.1" | "HTTP/1.0")) || status.next() != Some("200") {
        return None;
    }
    let mut location = None;
    for line in lines.take_while(|line| !line.trim().is_empty()) {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("location") {
            if location.is_some() {
                return None;
            }
            let url = Url::parse(value.trim()).ok()?;
            if url.scheme() != "http"
                || url
                    .host_str()
                    .and_then(|value| value.parse::<Ipv4Addr>().ok())
                    != Some(sender)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || url.port() == Some(0)
            {
                return None;
            }
            location = Some(url);
        }
    }
    location
}

struct Description {
    name: String,
    model: Option<String>,
    udn: Option<String>,
}

async fn fetch_description(client: &Client, url: Url) -> Option<Description> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > MAX_XML as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_XML {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_description(std::str::from_utf8(&bytes).ok()?)
}

fn parse_description(xml: &str) -> Option<Description> {
    let document = Document::parse(xml).ok()?;
    let root = document.root_element();
    if !root.has_tag_name("root") {
        return None;
    }
    let device = root.children().find(|node| node.has_tag_name("device"))?;
    let field = |name| {
        device
            .children()
            .find(|node| node.has_tag_name(name))
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| {
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
            })
    };
    let maker = field("manufacturer")?.to_ascii_lowercase();
    if !matches!(
        maker.trim_end_matches('.'),
        "lg" | "lge" | "lg electronics" | "lg electronics inc" | "lg electronics, inc"
    ) {
        return None;
    }
    let named_webos = ["modelDescription", "friendlyName", "modelName"]
        .iter()
        .any(|field_name| {
            field(*field_name).is_some_and(|value| value.to_ascii_lowercase().contains("webos"))
        });
    let second_screen = device
        .children()
        .filter(|node| node.has_tag_name("serviceList"))
        .flat_map(|node| node.children())
        .filter(|node| node.has_tag_name("service"))
        .flat_map(|node| node.children())
        .any(|node| node.has_tag_name("serviceType") && node.text().map(str::trim) == Some(WEBOS));
    if !(named_webos || second_screen) {
        return None;
    }
    Some(Description {
        name: field("friendlyName").unwrap_or("LG webOS TV").to_owned(),
        model: field("modelNumber")
            .or_else(|| field("modelName"))
            .map(str::to_owned),
        udn: field("UDN").map(str::to_owned),
    })
}

#[derive(Deserialize)]
struct AddressList {
    ifname: String,
    flags: Vec<String>,
    #[serde(default)]
    addr_info: Vec<Address>,
}

#[derive(Deserialize)]
struct Address {
    family: String,
    local: IpAddr,
    prefixlen: u8,
}

fn eligible_interfaces(addresses: Vec<AddressList>, selected: &[String]) -> Vec<Interface> {
    let mut result = Vec::new();
    for address_list in addresses {
        let has_flag = |flag: &str| address_list.flags.iter().any(|value| value == flag);
        if !has_flag("UP")
            || !has_flag("MULTICAST")
            || has_flag("LOOPBACK")
            || has_flag("POINTOPOINT")
            || (!selected.is_empty() && !selected.contains(&address_list.ifname))
        {
            continue;
        }
        for address in address_list.addr_info {
            let IpAddr::V4(ip) = address.local else {
                continue;
            };
            if address.family == "inet"
                && !ip.is_loopback()
                && usable_ip(ip)
                && (1..=32).contains(&address.prefixlen)
            {
                let mask = u32::MAX << (32 - address.prefixlen);
                result.push(Interface {
                    name: address_list.ifname.clone(),
                    source_ip: ip,
                    netmask: Ipv4Addr::from(mask),
                });
            }
        }
    }
    result.sort_by(|a, b| (&a.name, a.source_ip).cmp(&(&b.name, b.source_ip)));
    result.dedup_by(|a, b| a.name == b.name && a.source_ip == b.source_ip);
    result
}

#[derive(Deserialize)]
struct Neighbour {
    dst: Ipv4Addr,
    dev: String,
    lladdr: Option<String>,
    #[serde(default)]
    state: Vec<String>,
}

fn assign_macs(televisions: &mut [Television], neighbours: Vec<Neighbour>) {
    let mut addresses = BTreeMap::<_, BTreeSet<_>>::new();
    for neighbour in neighbours {
        if neighbour
            .state
            .iter()
            .any(|state| matches!(state.as_str(), "INCOMPLETE" | "FAILED" | "NOARP"))
        {
            continue;
        }
        if let Some(mac) = neighbour.lladdr.as_deref().and_then(parse_mac)
            && mac != [0; 6]
            && mac[0] & 1 == 0
        {
            addresses
                .entry((neighbour.dev, neighbour.dst))
                .or_default()
                .insert(mac);
        }
    }
    for tv in televisions {
        if let Some(mac) = addresses
            .get(&(tv.interface.name.clone(), tv.ip))
            .filter(|candidates| candidates.len() == 1)
            .and_then(|candidates| candidates.first())
        {
            tv.mac = Some(mac.map(|byte| format!("{byte:02x}")).join(":"));
        }
    }
}

fn deduplicate(televisions: &mut Vec<Television>) {
    televisions.sort_by(|a, b| {
        (a.ip, &a.mac, &a.interface.name, a.interface.source_ip).cmp(&(
            b.ip,
            &b.mac,
            &b.interface.name,
            b.interface.source_ip,
        ))
    });
    televisions.dedup_by(|a, b| {
        a.ip == b.ip && a.mac == b.mac && (a.mac.is_some() || a.interface.name == b.interface.name)
    });
}

async fn ip_json<T: DeserializeOwned>(args: &[&str]) -> Result<T> {
    let output = timeout(
        IO_TIMEOUT,
        Command::new("ip").args(args).kill_on_drop(true).output(),
    )
    .await?
    .map_err(|error| format!("Cannot run ip from iproute2: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ip {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Invalid iproute2 JSON: {error}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    const LG: &str = include_str!("discovery/fixtures/lg-description.xml");

    #[test]
    fn accepts_only_lg_webos_metadata_on_the_root_device() {
        let description = parse_description(LG).unwrap();
        assert_eq!(description.name, "Office TV & HDMI");
        assert_eq!(description.model.as_deref(), Some("OLED65"));
        assert_eq!(description.udn.as_deref(), Some("uuid:example"));
        assert!(parse_description(&LG.replace("LG Electronics.", "Another vendor")).is_none());
        assert!(
            parse_description(
                &LG.replace("LG webOS TV", "Legacy TV")
                    .replace(WEBOS, "not-webos")
            )
            .is_none()
        );
        let (_, root) = LG.split_once('\n').unwrap();
        let with_dtd = format!("<!DOCTYPE root [<!ENTITY test 'a'>]>{root}");
        assert!(parse_description(&with_dtd).is_none());
    }

    #[test]
    fn ssdp_location_must_be_an_unambiguous_http_url_for_the_responder() {
        let ip = "192.0.2.10".parse().unwrap();
        let packet = |headers| format!("HTTP/1.1 200 OK\r\n{headers}\r\n\r\n");
        let accepted = packet("lOcAtIoN: http://192.0.2.10:1234/description.xml");
        assert!(response_location(accepted.as_bytes(), ip).is_some());
        for headers in [
            "Location: http://192.0.2.11/",
            "Location: http://router.local/",
            "Location: https://192.0.2.10/",
            "Location: http://user@192.0.2.10/",
            "Location: http://192.0.2.10:0/",
            "Location: http://192.0.2.10/a\r\nLocation: http://192.0.2.10/b",
        ] {
            assert!(
                response_location(packet(headers).as_bytes(), ip).is_none(),
                "{headers}"
            );
        }
    }

    #[test]
    fn discovers_both_lan_interfaces_and_matches_neighbours_on_the_right_link() {
        let data = r#"[
            {"ifname":"lo","flags":["UP","LOOPBACK"],"addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},
            {"ifname":"lan","flags":["UP","MULTICAST"],"addr_info":[{"family":"inet","local":"192.0.2.5","prefixlen":24}]},
            {"ifname":"wlan","flags":["UP","MULTICAST"],"addr_info":[{"family":"inet","local":"192.0.2.6","prefixlen":25}]},
            {"ifname":"link","flags":["UP","POINTOPOINT","MULTICAST"],"addr_info":[{"family":"inet","local":"198.51.100.1","prefixlen":24}]}
        ]"#;
        let interfaces = eligible_interfaces(serde_json::from_str(data).unwrap(), &[]);
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[1].netmask, Ipv4Addr::new(255, 255, 255, 128));
        assert_eq!(
            eligible_interfaces(serde_json::from_str(data).unwrap(), &["wlan".into()]).len(),
            1
        );
        let mut observations: Vec<_> = interfaces
            .into_iter()
            .map(|interface| Television {
                ip: "192.0.2.10".parse().unwrap(),
                interface,
                mac: None,
                name: "Device".into(),
                model: None,
                udn: None,
            })
            .collect();
        let neighbours = serde_json::from_str(r#"[
            {"dev":"wlan","dst":"192.0.2.10","lladdr":"02:11:22:33:44:55","state":["STALE"]},
            {"dev":"lan","dst":"192.0.2.10","lladdr":"02:11:22:33:44:66","state":["FAILED"]},
            {"dev":"other-lan","dst":"192.0.2.10","lladdr":"02:11:22:33:44:77","state":["REACHABLE"]}
        ]"#).unwrap();
        assign_macs(&mut observations, neighbours);
        assert_eq!(observations[0].mac, None);
        assert_eq!(observations[1].mac.as_deref(), Some("02:11:22:33:44:55"));
        observations.push(observations[1].clone());
        deduplicate(&mut observations);
        assert_eq!(observations.len(), 2);
    }

    #[tokio::test]
    async fn fetches_a_description_over_the_selected_lan_address() {
        let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target = match tcp.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => unreachable!(),
        };
        let udp = UdpSocket::bind(target).await.unwrap();
        let interface = Interface {
            name: "mock".into(),
            source_ip: Ipv4Addr::new(127, 0, 0, 2),
            netmask: Ipv4Addr::new(255, 0, 0, 0),
        };
        let responder = tokio::spawn(async move {
            let mut probes = BTreeSet::new();
            let mut buffer = [0; 2048];
            for _ in 0..2 {
                let (size, peer) = timeout(IO_TIMEOUT, udp.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(peer.ip(), Ipv4Addr::new(127, 0, 0, 2));
                let request = std::str::from_utf8(&buffer[..size]).unwrap();
                assert!(request.starts_with("M-SEARCH * HTTP/1.1"));
                let service = request
                    .lines()
                    .find_map(|line| line.strip_prefix("ST: "))
                    .unwrap();
                probes.insert(service.to_owned());
                let response =
                    format!("HTTP/1.1 200 OK\r\nLOCATION: http://{target}/description.xml\r\n\r\n");
                udp.send_to(response.as_bytes(), peer).await.unwrap();
            }
            assert_eq!(probes, [WEBOS.to_owned(), RENDERER.to_owned()].into());
            let (socket, peer) = timeout(IO_TIMEOUT, tcp.accept()).await.unwrap().unwrap();
            assert_eq!(peer.ip(), Ipv4Addr::new(127, 0, 0, 2));
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(line, "GET /description.xml HTTP/1.1\r\n");
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty());
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{LG}",
                LG.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let (found, limited) = scan_interface(&interface, target, Duration::from_millis(250))
            .await
            .unwrap();
        assert!(!limited);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ip, Ipv4Addr::LOCALHOST);
        assert_eq!(found[0].name, "Office TV & HDMI");
        responder.await.unwrap();
    }
}

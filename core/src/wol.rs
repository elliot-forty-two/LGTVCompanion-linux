//! Wire format for Wake-on-LAN. Source interface selection belongs to the transport.

use std::{net::Ipv4Addr, time::Duration};

pub const PORT: u16 = 9;
pub const REPEAT_INTERVAL: Duration = Duration::from_secs(1);

pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xff; 102];
    for repetition in packet[6..].chunks_exact_mut(mac.len()) {
        repetition.copy_from_slice(&mac);
    }
    packet
}

pub fn subnet_broadcast(address: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(address) | !u32::from(netmask))
}

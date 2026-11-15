//! Pick the local address a device can reach us on.

use std::net::{Ipv4Addr, UdpSocket};

/// IPv4 address of the local interface that routes to `peer`.
///
/// "Connecting" a UDP socket sends nothing; it just makes the OS pick the
/// outgoing interface, which is the one the speaker can reach us on (this
/// matters on machines with VPNs, Docker bridges or several NICs).
pub fn lan_ip_for(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((peer, 9)).ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_routes_via_loopback() {
        assert_eq!(lan_ip_for(Ipv4Addr::LOCALHOST), Some(Ipv4Addr::LOCALHOST));
    }
}

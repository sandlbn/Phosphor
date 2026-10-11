//! mDNS discovery of Google Cast and AirPlay targets.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use mdns_sd::RecvTimeoutError;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};

use super::{airplay, netaddr, AirPlayAuth, CastEvent, CastTarget, Protocol};

const GOOGLECAST: &str = "_googlecast._tcp.local.";
const AIRPLAY: &str = "_airplay._tcp.local.";
const REVERIFY_EVERY: Duration = Duration::from_secs(60);

type MakeTarget = fn(&ResolvedService, Ipv4Addr) -> Option<CastTarget>;

/// Browse in the background; each protocol pushes its full, sorted device
/// list to `events` whenever it changes.
pub fn spawn(events: Sender<CastEvent>) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[cast] mDNS unavailable: {e}");
            return;
        }
    };
    browse(
        &daemon,
        GOOGLECAST,
        Protocol::GoogleCast,
        google_target,
        events.clone(),
    );
    if cfg!(feature = "airplay") {
        browse(&daemon, AIRPLAY, Protocol::AirPlay, airplay_target, events);
    }
}

fn browse(
    daemon: &ServiceDaemon,
    ty: &'static str,
    proto: Protocol,
    make: MakeTarget,
    events: Sender<CastEvent>,
) {
    let rx = match daemon.browse(ty) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("[cast] mDNS browse {ty} failed: {e}");
            return;
        }
    };
    // Keep the daemon alive as long as any browse thread runs.
    let daemon = daemon.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("mdns-{}", proto.label()))
        .spawn(move || {
            let mut devices: BTreeMap<String, CastTarget> = BTreeMap::new();
            let mut last_verify = Instant::now();
            loop {
                // Receivers can restart on a new port (seen: a Fire TV's
                // AirPlay moved 7000 -> 42651 after a session) without the
                // cached record updating promptly. Ask the network to
                // re-confirm every known instance now and then; changes
                // arrive as fresh ServiceResolved / ServiceRemoved events.
                if last_verify.elapsed() >= REVERIFY_EVERY {
                    last_verify = Instant::now();
                    for id in devices.keys() {
                        let _ = daemon.verify(id.clone(), mdns_sd::VERIFY_TIMEOUT_DEFAULT);
                    }
                }
                let ev = match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(ev) => ev,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                let changed = match ev {
                    ServiceEvent::ServiceResolved(info) => {
                        let addr = info.get_addresses_v4().into_iter().next();
                        match addr.and_then(|a| make(&info, a)) {
                            Some(t) => devices.insert(t.id.clone(), t.clone()).as_ref() != Some(&t),
                            None => false,
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        devices.remove(&fullname).is_some()
                    }
                    _ => false,
                };
                if changed {
                    let mut list: Vec<CastTarget> = devices.values().cloned().collect();
                    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    if events.send(CastEvent::Devices(proto, list)).is_err() {
                        break;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[cast] cannot start discovery thread: {e}");
    }
}

/// Re-resolve one target from scratch with a fresh, cache-less daemon.
/// Used when a cached host/port stops answering (receiver restarted on a
/// new port). Blocks up to ~3 s; `None` if the device isn't announcing.
pub fn refresh_target(target: &CastTarget) -> Option<CastTarget> {
    let (ty, make): (&str, MakeTarget) = match target.protocol {
        Protocol::GoogleCast => (GOOGLECAST, google_target),
        Protocol::AirPlay => (AIRPLAY, airplay_target),
        Protocol::Upnp => return None,
    };
    let daemon = ServiceDaemon::new().ok()?;
    let rx = daemon.browse(ty).ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) if info.get_fullname() == target.id => {
                let addr = info.get_addresses_v4().into_iter().next();
                found = addr.and_then(|a| make(&info, a));
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    found
}

/// Instance name: the part of the mDNS fullname before the service type.
fn instance_name(info: &ResolvedService, ty: &str) -> String {
    let full = info.get_fullname();
    full.strip_suffix(ty)
        .map(|n| n.trim_end_matches('.'))
        .unwrap_or(full)
        .to_string()
}

fn google_target(info: &ResolvedService, addr: Ipv4Addr) -> Option<CastTarget> {
    // AirReceiver's emulated Cast receiver accepts LOAD but never fetches
    // the URL (verified: zero requests across every content/stream type).
    // The same device is listed under DLNA and AirPlay, which do work.
    if info
        .get_property_val_str("md")
        .is_some_and(|m| m.contains("AirReceiver"))
    {
        return None;
    }
    // TXT: fn = friendly name ("Living Room speaker"), md = model.
    // Speaker groups advertise their own instance on a non-8009 port,
    // so the port always comes from SRV.
    let name = info
        .get_property_val_str("fn")
        .map(str::to_string)
        .unwrap_or_else(|| instance_name(info, GOOGLECAST));
    Some(CastTarget {
        id: info.get_fullname().to_string(),
        name,
        model: info.get_property_val_str("md").unwrap_or("").to_string(),
        protocol: Protocol::GoogleCast,
        addr,
        port: info.get_port(),
        location: String::new(),
        sonos: false,
        airplay: None,
    })
}

fn airplay_target(info: &ResolvedService, addr: Ipv4Addr) -> Option<CastTarget> {
    // This Mac's own AirPlay receiver: casting there would loop our audio.
    if netaddr::lan_ip_for(addr) == Some(addr) {
        return None;
    }
    let prop = |k: &str| info.get_property_val_str(k).unwrap_or("");
    // AirReceiver's AirPlay 1 emulation answers SETUP without the ports
    // audio needs (verified: "SETUP reply carried no server_port"). Its
    // DLNA entry works; Fire TVs also have a native AirPlay 2 receiver.
    if prop("rmodel").contains("AirReceiver") {
        return None;
    }
    // Sonos also speaks UPnP, which needs no pairing and supports groups.
    if prop("manufacturer").to_ascii_lowercase().contains("sonos") {
        return None;
    }
    let model = prop("model");
    let (auth, airplay2) = airplay::auth_for(model, airplay::parse_features(prop("features")));
    let maker = prop("manufacturer");
    Some(CastTarget {
        id: info.get_fullname().to_string(),
        name: instance_name(info, AIRPLAY),
        model: if maker.is_empty() {
            model.to_string()
        } else {
            format!("{maker} {model}")
        },
        protocol: Protocol::AirPlay,
        addr,
        port: info.get_port(),
        location: String::new(),
        sonos: false,
        airplay: Some(AirPlayAuth {
            auth,
            airplay2,
            device_id: prop("deviceid").to_string(),
        }),
    })
}

#[cfg(test)]
mod tests {
    /// Live check against the local network:
    /// `cargo test --release -- --ignored discover_live --nocapture`
    #[test]
    #[ignore]
    fn discover_live() {
        let (tx, rx) = crossbeam_channel::unbounded();
        super::spawn(tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut by_proto = std::collections::BTreeMap::new();
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            if let Ok(super::CastEvent::Devices(p, list)) = rx.recv_timeout(left) {
                by_proto.insert(p, list);
            }
        }
        let last: Vec<_> = by_proto.into_values().flatten().collect();
        for d in &last {
            eprintln!(
                "{:?}  {:24} {:24} {}:{}",
                d.protocol, d.name, d.model, d.addr, d.port
            );
        }
        eprintln!("{} device(s)", last.len());
    }

    /// `<test-bin> --ignored refresh_target_live --nocapture`
    #[test]
    #[ignore]
    fn refresh_target_live() {
        let stale = super::CastTarget {
            id: "Marcin's TV._airplay._tcp.local.".into(),
            name: "Marcin's TV".into(),
            model: String::new(),
            protocol: super::Protocol::AirPlay,
            addr: std::net::Ipv4Addr::new(10, 0, 0, 131),
            port: 7000,
            location: String::new(),
            sonos: false,
            airplay: None,
        };
        let t0 = std::time::Instant::now();
        let fresh = super::refresh_target(&stale);
        eprintln!(
            "refreshed in {:?}: {:?}",
            t0.elapsed(),
            fresh.map(|f| (f.addr, f.port, f.airplay))
        );
    }
}

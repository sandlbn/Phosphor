//! UPnP / DLNA media renderers (Sonos, smart TVs, DLNA speakers).
//!
//! Like Google Cast, the renderer pulls our live MP3 itself; we just tell
//! it the URI (AVTransport `SetAVTransportURI` + `Play`) and control
//! volume (RenderingControl). rupnp is async, so each operation runs on a
//! small current-thread tokio runtime inside the cast worker's helper
//! thread.
//!
//! Sonos specifics:
//! * endless MP3 must use the `x-rincon-mp3radio://` scheme — plain
//!   `http://` errors or stops once its buffer fills;
//! * commands go to a group's coordinator, so only coordinators are
//!   listed (named after their group's rooms), and volume uses
//!   GroupRenderingControl so the whole group follows the slider.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::time::Duration;

use crossbeam_channel::Sender;
use iced::futures::StreamExt;
use rupnp::http::Uri;
use rupnp::ssdp::{SearchTarget, URN};
use rupnp::{Device, DeviceSpec, Service};

use super::{netaddr, CastEvent, CastTarget, Protocol, Sink, TrackMeta};

const RENDERER: URN = URN::device("schemas-upnp-org", "MediaRenderer", 1);
const SEARCH_WINDOW: Duration = Duration::from_secs(3);
const RESCAN_EVERY: Duration = Duration::from_secs(30);
const ACTION_TIMEOUT: Duration = Duration::from_secs(8);

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))
}

/// Find a service by type name, ignoring its version (renderers ship
/// AVTransport:1 through :3).
fn find_service<'a>(dev: &'a DeviceSpec, typ: &str) -> Option<&'a Service> {
    let needle = format!(":service:{typ}:");
    dev.services_iter()
        .find(|s| s.service_type().to_string().contains(&needle))
}

async fn call(
    dev: &Device,
    typ: &str,
    action: &str,
    args: &str,
) -> Result<HashMap<String, String>, String> {
    let svc = find_service(dev, typ).ok_or_else(|| format!("device has no {typ} service"))?;
    match tokio::time::timeout(ACTION_TIMEOUT, svc.action(dev.url(), action, args)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{action}: {e}")),
        Err(_) => Err(format!("{action}: device did not answer")),
    }
}

// ── Discovery ───────────────────────────────────────────────────────────

pub fn spawn_discovery(events: Sender<CastEvent>) {
    let spawned = std::thread::Builder::new()
        .name("upnp-discovery".into())
        .spawn(move || {
            let rt = match runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[cast] UPnP discovery unavailable: {e}");
                    return;
                }
            };
            let mut last: Option<Vec<CastTarget>> = None;
            loop {
                let list = rt.block_on(scan());
                if last.as_ref() != Some(&list) {
                    if events
                        .send(CastEvent::Devices(Protocol::Upnp, list.clone()))
                        .is_err()
                    {
                        break;
                    }
                    last = Some(list);
                }
                std::thread::sleep(RESCAN_EVERY);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[cast] cannot start UPnP discovery thread: {e}");
    }
}

async fn scan() -> Vec<CastTarget> {
    let found = match rupnp::discover(&SearchTarget::URN(RENDERER), SEARCH_WINDOW, None).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[cast] SSDP search failed: {e}");
            return Vec::new();
        }
    };
    let mut found = std::pin::pin!(found);
    let mut devices: BTreeMap<String, Device> = BTreeMap::new();
    while let Some(d) = found.next().await {
        match d {
            Ok(d) => {
                devices.insert(d.udn().to_string(), d);
            }
            Err(e) => crate::dlog!("[cast] UPnP device description failed: {e}"),
        }
    }

    // Sonos: one topology query tells us which players coordinate a group.
    let topology = match devices.values().find(|d| is_sonos(d)) {
        Some(d) => match call(d, "ZoneGroupTopology", "GetZoneGroupState", "").await {
            Ok(v) => v
                .get("ZoneGroupState")
                .map(|x| parse_zone_groups(x))
                .unwrap_or_default(),
            Err(e) => {
                eprintln!("[cast] Sonos topology query failed: {e}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let mut list: Vec<CastTarget> = devices
        .values()
        .filter_map(|d| target_for(d, &topology))
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn is_sonos(d: &DeviceSpec) -> bool {
    d.manufacturer().to_ascii_lowercase().contains("sonos")
}

fn target_for(d: &Device, groups: &[ZoneGroup]) -> Option<CastTarget> {
    find_service(d, "AVTransport")?;
    let url = d.url();
    let addr: Ipv4Addr = url.host()?.parse().ok()?;
    let sonos = is_sonos(d);
    let mut name = d.friendly_name().to_string();
    if sonos && !groups.is_empty() {
        let uuid = d.udn().trim_start_matches("uuid:");
        // Only a group's coordinator accepts playback; members would be
        // pulled out of their group.
        let group = groups.iter().find(|g| g.coordinator == uuid)?;
        name = group.name();
    }
    Some(CastTarget {
        id: d.udn().to_string(),
        name,
        model: format!("{} {}", d.manufacturer(), d.model_name())
            .trim()
            .to_string(),
        protocol: Protocol::Upnp,
        addr,
        port: url.port_u16().unwrap_or(80),
        location: url.to_string(),
        sonos,
        airplay: None,
    })
}

// ── Sonos zone groups ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct ZoneGroup {
    coordinator: String,
    /// Visible room names; the coordinator's first.
    rooms: Vec<String>,
}

impl ZoneGroup {
    fn name(&self) -> String {
        match self.rooms.len() {
            0 => "Sonos".into(),
            1 => self.rooms[0].clone(),
            _ => self.rooms.join(" + "),
        }
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(xml_unescape(&tag[start..end]))
}

/// Parse `GetZoneGroupState`'s XML into groups. Hidden members (subs,
/// surrounds: `Invisible="1"`) don't name a group.
fn parse_zone_groups(xml: &str) -> Vec<ZoneGroup> {
    let mut out = Vec::new();
    for chunk in xml.split("<ZoneGroup ").skip(1) {
        let head_end = chunk.find('>').unwrap_or(chunk.len());
        let Some(coordinator) = attr(&format!(" {}", &chunk[..head_end]), "Coordinator") else {
            continue;
        };
        let body = chunk.split("</ZoneGroup>").next().unwrap_or(chunk);
        let mut members: Vec<(String, String)> = Vec::new();
        for m in body.split("<ZoneGroupMember ").skip(1) {
            let tag = format!(" {}", &m[..m.find('>').unwrap_or(m.len())]);
            if attr(&tag, "Invisible").as_deref() == Some("1") {
                continue;
            }
            if let (Some(uuid), Some(room)) = (attr(&tag, "UUID"), attr(&tag, "ZoneName")) {
                if !members.iter().any(|(_, r)| *r == room) {
                    members.push((uuid, room));
                }
            }
        }
        members.sort_by_key(|(uuid, _)| *uuid != coordinator);
        out.push(ZoneGroup {
            coordinator,
            rooms: members.into_iter().map(|(_, r)| r).collect(),
        });
    }
    out
}

// ── Metadata ────────────────────────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// DIDL-Lite for a live MP3 broadcast. `DLNA.ORG_OP=00` = not seekable;
/// some TVs reject empty metadata, so this is always sent.
fn didl(meta: &TrackMeta, res_url: &str, mime: &str, art_url: &str, sonos: bool) -> String {
    let title = if meta.title.is_empty() {
        "Phosphor"
    } else {
        &meta.title
    };
    let sonos_desc = if sonos {
        r#"<desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/">SA_RINCON65031_</desc>"#
    } else {
        ""
    };
    format!(
        concat!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" "#,
            r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
            r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" "#,
            r#"xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/">"#,
            r#"<item id="phosphor-live" parentID="0" restricted="1">"#,
            "<dc:title>{title}</dc:title>",
            "<upnp:artist>{artist}</upnp:artist>",
            "<dc:creator>{artist}</dc:creator>",
            "<upnp:albumArtURI>{art}</upnp:albumArtURI>",
            "<upnp:class>object.item.audioItem.audioBroadcast</upnp:class>",
            r#"<res protocolInfo="{proto}">{res}</res>"#,
            "{desc}</item></DIDL-Lite>"
        ),
        title = xml_escape(title),
        artist = xml_escape(&meta.artist),
        art = xml_escape(art_url),
        res = xml_escape(res_url),
        proto = protocol_info(mime),
        desc = sonos_desc,
    )
}

/// DLNA protocolInfo: not seekable (OP=00), streaming transfer flags.
fn protocol_info(mime: &str) -> String {
    const FLAGS: &str = "DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000";
    if mime == "audio/mpeg" {
        format!("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;{FLAGS}")
    } else {
        format!("http-get:*:{mime}:{FLAGS}")
    }
}

/// Sonos only plays an endless MP3 through its internet-radio scheme.
fn transport_uri(http_url: &str, sonos: bool) -> String {
    if sonos {
        http_url.replacen("http://", "x-rincon-mp3radio://", 1)
    } else {
        http_url.to_string()
    }
}

// ── Session ─────────────────────────────────────────────────────────────

pub(crate) struct Session {
    target: CastTarget,
    device: Device,
    stream_url: String,
    mime: &'static str,
    art_url: String,
    volume: Option<f32>,
}

impl Session {
    pub(crate) fn start(target: &CastTarget, port: u16, meta: &TrackMeta) -> Result<Self, String> {
        let lan = netaddr::lan_ip_for(target.addr)
            .ok_or_else(|| format!("no network route to {}", target.addr))?;
        let base = format!("http://{lan}:{port}");
        let (path, mime) = super::stream_path(target.sonos);
        let url: Uri = target
            .location
            .parse()
            .map_err(|e| format!("bad device URL: {e}"))?;
        let device = runtime()?.block_on(async {
            tokio::time::timeout(ACTION_TIMEOUT, Device::from_url(url))
                .await
                .map_err(|_| "device description: no answer".to_string())?
                .map_err(|e| format!("device description: {e}"))
        })?;
        let mut s = Self {
            target: target.clone(),
            device,
            stream_url: format!("{base}{path}"),
            mime,
            art_url: format!("{base}/api/cast-art.png"),
            volume: None,
        };
        s.load(meta, false)?;
        s.volume = s.read_volume();
        Ok(s)
    }

    /// Current speaker volume (0..1); Sonos coordinators report the group.
    fn read_volume(&self) -> Option<f32> {
        let rt = runtime().ok()?;
        let res =
            if self.target.sonos && find_service(&self.device, "GroupRenderingControl").is_some() {
                rt.block_on(call(
                    &self.device,
                    "GroupRenderingControl",
                    "GetGroupVolume",
                    "<InstanceID>0</InstanceID>",
                ))
            } else {
                rt.block_on(call(
                    &self.device,
                    "RenderingControl",
                    "GetVolume",
                    "<InstanceID>0</InstanceID><Channel>Master</Channel>",
                ))
            };
        let v: f32 = res.ok()?.get("CurrentVolume")?.trim().parse().ok()?;
        Some((v / 100.0).clamp(0.0, 1.0))
    }

    /// Point the renderer at our stream and start it. `restart` stops the
    /// current item first (track change), which some renderers require
    /// before accepting a new URI.
    fn load(&self, meta: &TrackMeta, restart: bool) -> Result<(), String> {
        let sonos = self.target.sonos;
        let uri = transport_uri(&self.stream_url, sonos);
        let meta_xml = didl(meta, &self.stream_url, self.mime, &self.art_url, sonos);
        let set = format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
            xml_escape(&uri),
            xml_escape(&meta_xml)
        );
        runtime()?.block_on(async {
            if restart {
                let _ = call(
                    &self.device,
                    "AVTransport",
                    "Stop",
                    "<InstanceID>0</InstanceID>",
                )
                .await;
            }
            call(&self.device, "AVTransport", "SetAVTransportURI", &set).await?;
            call(
                &self.device,
                "AVTransport",
                "Play",
                "<InstanceID>0</InstanceID><Speed>1</Speed>",
            )
            .await?;
            Ok(())
        })
    }
}

impl Sink for Session {
    fn target(&self) -> &CastTarget {
        &self.target
    }

    fn stream_url(&self) -> &str {
        &self.stream_url
    }

    fn stop(&self) -> Result<(), String> {
        runtime()?.block_on(call(
            &self.device,
            "AVTransport",
            "Stop",
            "<InstanceID>0</InstanceID>",
        ))?;
        Ok(())
    }

    fn set_volume(&self, level: f32) -> Result<(), String> {
        let v = (level.clamp(0.0, 1.0) * 100.0).round() as u32;
        runtime()?.block_on(async {
            // Sonos coordinators: move the whole group together.
            if self.target.sonos && find_service(&self.device, "GroupRenderingControl").is_some() {
                call(
                    &self.device,
                    "GroupRenderingControl",
                    "SetGroupVolume",
                    &format!("<InstanceID>0</InstanceID><DesiredVolume>{v}</DesiredVolume>"),
                )
                .await?;
            } else {
                call(
                    &self.device,
                    "RenderingControl",
                    "SetVolume",
                    &format!(
                        "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{v}</DesiredVolume>"
                    ),
                )
                .await?;
            }
            Ok(())
        })
    }

    fn update_track(&self, meta: &TrackMeta) -> Result<(), String> {
        self.load(meta, true)
    }

    fn volume(&self) -> Option<f32> {
        self.volume
    }

    fn still_ours(&self) -> Result<bool, String> {
        runtime()?.block_on(async {
            let media = call(
                &self.device,
                "AVTransport",
                "GetMediaInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await?;
            let ours = media
                .get("CurrentURI")
                .is_some_and(|u| u.contains("/api/stream."));
            if !ours {
                return Ok(false);
            }
            let info = call(
                &self.device,
                "AVTransport",
                "GetTransportInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await?;
            let state = info
                .get("CurrentTransportState")
                .map(String::as_str)
                .unwrap_or("");
            Ok(!matches!(state, "STOPPED" | "NO_MEDIA_PRESENT"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonos_uri_rewrite() {
        assert_eq!(
            transport_uri("http://10.0.0.2:8364/api/stream.mp3", true),
            "x-rincon-mp3radio://10.0.0.2:8364/api/stream.mp3"
        );
        assert_eq!(
            transport_uri("http://10.0.0.2:8364/api/stream.mp3", false),
            "http://10.0.0.2:8364/api/stream.mp3"
        );
    }

    #[test]
    fn didl_escapes_and_marks_broadcast() {
        let meta = TrackMeta {
            title: "Rock & <Roll> \"SID\"".into(),
            artist: "Hubbard's".into(),
        };
        let x = didl(
            &meta,
            "http://h/a?x=1&y=2",
            "audio/mpeg",
            "http://h/art.png",
            false,
        );
        assert!(x.contains("<dc:title>Rock &amp; &lt;Roll&gt; &quot;SID&quot;</dc:title>"));
        assert!(x.contains("Hubbard&apos;s"));
        assert!(x.contains(">http://h/a?x=1&amp;y=2</res>"));
        assert!(x.contains("object.item.audioItem.audioBroadcast"));
        assert!(x.contains("audio/mpeg:DLNA.ORG_PN=MP3"));
        assert!(!x.contains("SA_RINCON"));
        assert!(didl(&meta, "u", "audio/mpeg", "a", true).contains("SA_RINCON65031_"));
        assert!(didl(&meta, "u", "audio/flac", "a", false)
            .contains("http-get:*:audio/flac:DLNA.ORG_OP=00"));
        // The whole DIDL is escaped again inside the SOAP body.
        assert!(!xml_escape(&x).contains('<'));
    }

    #[test]
    fn empty_title_falls_back() {
        assert!(didl(&TrackMeta::default(), "u", "audio/mpeg", "a", false)
            .contains("<dc:title>Phosphor</dc:title>"));
    }

    #[test]
    fn zone_groups_keep_coordinators_and_skip_hidden_members() {
        let xml = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="RINCON_A01400" ID="RINCON_A01400:1">
  <ZoneGroupMember UUID="RINCON_B01400" ZoneName="Kitchen" Invisible="0"/>
  <ZoneGroupMember UUID="RINCON_A01400" ZoneName="Living Room">
    <Satellite UUID="RINCON_S01400" ZoneName="Living Room" Invisible="1"/>
  </ZoneGroupMember>
  <ZoneGroupMember UUID="RINCON_SUB" ZoneName="Living Room Sub" Invisible="1"/>
</ZoneGroup>
<ZoneGroup Coordinator="RINCON_C01400" ID="RINCON_C01400:7">
  <ZoneGroupMember UUID="RINCON_C01400" ZoneName="Office &amp; Study"/>
</ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;
        let g = parse_zone_groups(xml);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].coordinator, "RINCON_A01400");
        assert_eq!(g[0].rooms, vec!["Living Room", "Kitchen"]);
        assert_eq!(g[0].name(), "Living Room + Kitchen");
        assert_eq!(g[1].name(), "Office & Study");
    }

    /// Live SSDP scan: `<test-bin> --ignored upnp_discover_live --nocapture`
    #[test]
    #[ignore]
    fn upnp_discover_live() {
        let list = runtime().unwrap().block_on(scan());
        for d in &list {
            eprintln!(
                "{:24} {:28} {}:{} sonos={} {}",
                d.name, d.model, d.addr, d.port, d.sonos, d.location
            );
        }
        eprintln!("{} renderer(s)", list.len());
    }

    /// Cast a silent stream to one renderer and trace its transport state:
    /// `UPNP_LOCATION=http://10.0.0.131:3500/ <test-bin> --ignored upnp_trace_live --nocapture`
    #[test]
    #[ignore]
    fn upnp_trace_live() {
        use std::sync::{Arc, Mutex};
        let location = std::env::var("UPNP_LOCATION").expect("UPNP_LOCATION");
        super::super::set_lossless(std::env::var_os("LOSSLESS").is_some());
        let rt = runtime().unwrap();
        let target = rt
            .block_on(scan())
            .into_iter()
            .find(|t| t.location == location)
            .expect("renderer not found by scan");
        let http_port = 18_368;
        let state = Arc::new(Mutex::new(crate::remote::SharedRemoteState::default()));
        state.lock().unwrap().stream_enabled = true;
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let server = crate::remote::start_server(http_port, state, tx).expect("bind");
        let meta = TrackMeta {
            title: "Phosphor test (silence)".into(),
            artist: "Phosphor".into(),
        };
        let s = Session::start(&target, http_port, &meta).expect("start");
        eprintln!("started {} on {}", s.stream_url, target.name);
        for t in 0..10 {
            std::thread::sleep(Duration::from_secs(1));
            let info = rt.block_on(call(
                &s.device,
                "AVTransport",
                "GetTransportInfo",
                "<InstanceID>0</InstanceID>",
            ));
            eprintln!(
                "t={t}s transport={:?} ours={:?}",
                info.map(|m| m.get("CurrentTransportState").cloned()),
                s.still_ours()
            );
        }
        eprintln!("stop -> {:?}", s.stop());
        server.unblock();
    }

    /// Stop backstop: cutting the stream server-side must disconnect the
    /// renderer even without a polite Stop.
    /// `UPNP_LOCATION=… <test-bin> --ignored upnp_kick_live --nocapture`
    #[test]
    #[ignore]
    fn upnp_kick_live() {
        use std::sync::{Arc, Mutex};
        let location = std::env::var("UPNP_LOCATION").expect("UPNP_LOCATION");
        let rt = runtime().unwrap();
        let target = rt
            .block_on(scan())
            .into_iter()
            .find(|t| t.location == location)
            .expect("found");
        let http_port = 18_369;
        let state = Arc::new(Mutex::new(crate::remote::SharedRemoteState::default()));
        state.lock().unwrap().stream_enabled = true;
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let server = crate::remote::start_server(http_port, state, tx).expect("bind");
        let ip = std::net::IpAddr::V4(target.addr);
        crate::remote::unkick_stream(ip);
        let s = Session::start(&target, http_port, &TrackMeta::default()).expect("start");
        std::thread::sleep(Duration::from_secs(3));
        eprintln!(
            "streaming before kick: {}",
            crate::remote::is_streaming_to(ip)
        );
        crate::remote::kick_stream(ip);
        let t0 = std::time::Instant::now();
        while crate::remote::is_streaming_to(ip) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!(
            "streaming after kick: {} ({:?})",
            crate::remote::is_streaming_to(ip),
            t0.elapsed()
        );
        let _ = s.stop();
        crate::remote::unkick_stream(ip);
        server.unblock();
    }
}

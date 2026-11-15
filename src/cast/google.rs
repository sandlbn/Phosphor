//! Google Cast sessions (Chromecast, Nest speakers, Google TV, groups).
//!
//! The Default Media Receiver keeps playing a loaded URL after the sender
//! disconnects, so every operation uses a short-lived connection instead
//! of a long-lived one. rust_cast's blocking `receive()` holds the same
//! lock the writer needs, so a persistent connection would make Stop and
//! volume wait for the device's next ping (~5 s).

use rust_cast::channels::media::{
    Image, Media, Metadata, MusicTrackMediaMetadata, PlayerState, StreamType,
};
use rust_cast::channels::receiver::CastDeviceApp;
use rust_cast::CastDevice;

use super::{netaddr, CastTarget, Sink, TrackMeta};

const RECEIVER: &str = "receiver-0";

type CastResult<T> = Result<T, rust_cast::errors::Error>;

/// A dropped / reset connection: worth one retry. Battery- and Wi-Fi-
/// power-saving devices (e.g. Lenovo's smart clock) sometimes cut the
/// first connection after idling, mid-reply.
fn transient(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "failed to fill whole buffer",
        "unexpected eof",
        "connection reset",
        "connection aborted",
        "broken pipe",
    ]
    .iter()
    .any(|m| e.contains(m))
}

/// Turn a LOAD failure into something a user can act on.
fn load_error(raw: &str) -> String {
    if raw.contains("INVALID_MEDIA_SESSION_ID") {
        // LOAD answered Idle and the media session vanished at once: the
        // receiver never even fetched the URL (seen with AirReceiver's
        // emulated Cast receiver, which doesn't play URLs at all).
        "the device accepted the cast but its player dropped the stream at once — \
         its Cast support can't play URLs (emulated receivers such as AirReceiver \
         behave like this)"
            .to_string()
    } else if transient(raw) {
        format!("the speaker closed the connection while loading the stream ({raw})")
    } else {
        format!("couldn't load the stream: {raw}")
    }
}

#[derive(Clone)]
pub(crate) struct Session {
    target: CastTarget,
    transport_id: String,
    session_id: String,
    stream_url: String,
    mime: &'static str,
    art_url: String,
    volume: Option<f32>,
}

impl Session {
    fn connect(target: &CastTarget) -> CastResult<CastDevice<'static>> {
        // Cast devices present self-signed certificates.
        let dev =
            CastDevice::connect_without_host_verification(target.addr.to_string(), target.port)?;
        dev.connection.connect(RECEIVER)?;
        Ok(dev)
    }

    pub(crate) fn start(
        target: &CastTarget,
        stream_port: u16,
        meta: &TrackMeta,
    ) -> Result<Self, String> {
        match Self::start_once(target, stream_port, meta) {
            Err(e) if transient(&e) => {
                eprintln!(
                    "[cast] {}: connection dropped ({e}); retrying once",
                    target.name
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
                // launch_app returns the already-running receiver session.
                Self::start_once(target, stream_port, meta)
            }
            r => r,
        }
    }

    fn start_once(target: &CastTarget, stream_port: u16, meta: &TrackMeta) -> Result<Self, String> {
        let lan = netaddr::lan_ip_for(target.addr)
            .ok_or_else(|| format!("no network route to {}", target.addr))?;
        let base = format!("http://{lan}:{stream_port}");
        let (path, mime) = super::stream_path(false);
        let dev = Self::connect(target).map_err(|e| format!("connect: {e}"))?;
        let app = dev
            .receiver
            .launch_app(&CastDeviceApp::DefaultMediaReceiver)
            .map_err(|e| format!("launch receiver: {e}"))?;
        let mut s = Self {
            target: target.clone(),
            transport_id: app.transport_id,
            session_id: app.session_id,
            stream_url: format!("{base}{path}"),
            mime,
            art_url: format!("{base}/api/cast-art.png"),
            volume: None,
        };
        // The speaker keeps its own volume; read it so the slider matches.
        s.volume = dev
            .receiver
            .get_status()
            .ok()
            .and_then(|st| st.volume.level);
        drop(dev);
        s.load_confirmed(meta)?;
        Ok(s)
    }

    /// Send LOAD and wait for proof it worked: either the reply, or the
    /// speaker pulling our stream. Some devices (seen: Lenovo Smart Clock)
    /// start playing but never answer LOAD, or drop connections on the
    /// media channel, so the reply alone isn't trustworthy.
    fn load_confirmed(&self, meta: &TrackMeta) -> Result<(), String> {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(8);
        let speaker = std::net::IpAddr::V4(self.target.addr);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (me, meta) = (self.clone(), meta.clone());
        // rust_cast's device isn't Send: LOAD runs on its own connection
        // in a helper thread; if it never answers, the thread is left to
        // die with its socket.
        std::thread::Builder::new()
            .name("cast-load".into())
            .spawn(move || {
                let r = Self::connect(&me.target)
                    .and_then(|d| me.load_on(&d, &meta))
                    .map_err(|e| e.to_string());
                let _ = tx.send(r);
            })
            .map_err(|e| format!("load thread: {e}"))?;
        let deadline = std::time::Instant::now() + WAIT;
        let mut reply: Option<String> = None;
        loop {
            if crate::remote::is_streaming_to(speaker) {
                if reply.is_some() {
                    eprintln!(
                        "[cast] {}: LOAD reply failed but the speaker is streaming",
                        self.target.name
                    );
                }
                return Ok(());
            }
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Ok(())) => return Ok(()),
                // Keep watching the stream for a moment before giving up.
                Ok(Err(e)) if reply.is_none() => reply = Some(e),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(match reply {
                    Some(e) => load_error(&e),
                    None => "the speaker accepted the cast but never started the stream".into(),
                });
            }
        }
    }

    fn media(&self, meta: &TrackMeta) -> Media {
        Media {
            content_id: self.stream_url.clone(),
            stream_type: StreamType::Live,
            content_type: self.mime.into(),
            metadata: Some(Metadata::MusicTrack(MusicTrackMediaMetadata {
                title: Some(if meta.title.is_empty() {
                    "Phosphor".into()
                } else {
                    meta.title.clone()
                }),
                artist: (!meta.artist.is_empty()).then(|| meta.artist.clone()),
                album_name: Some("Phosphor — SID player".into()),
                images: vec![Image::new(self.art_url.clone())],
                ..Default::default()
            })),
            duration: None,
        }
    }

    fn load_on(&self, dev: &CastDevice<'static>, meta: &TrackMeta) -> CastResult<()> {
        dev.connection.connect(self.transport_id.as_str())?;
        let status = dev.media.load(
            self.transport_id.as_str(),
            self.session_id.as_str(),
            &self.media(meta),
        )?;
        // LOAD carries autoplay=true, which real Chromecasts honour. Some
        // receivers answer Idle and wait for PLAY.
        if let Some(e) = status.entries.first() {
            if e.player_state == PlayerState::Idle {
                dev.media
                    .play(self.transport_id.as_str(), e.media_session_id)?;
            }
        }
        Ok(())
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
        Self::connect(&self.target)
            .and_then(|d| d.receiver.stop_app(self.session_id.as_str()))
            .map_err(|e| e.to_string())
    }

    fn set_volume(&self, level: f32) -> Result<(), String> {
        Self::connect(&self.target)
            .and_then(|d| d.receiver.set_volume(level.clamp(0.0, 1.0)))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Re-LOAD with new metadata (track change). Brief gap on the speaker.
    fn update_track(&self, meta: &TrackMeta) -> Result<(), String> {
        self.load_confirmed(meta)
    }

    fn volume(&self) -> Option<f32> {
        self.volume
    }

    fn still_ours(&self) -> Result<bool, String> {
        let status = Self::connect(&self.target)
            .and_then(|d| d.receiver.get_status())
            .map_err(|e| e.to_string())?;
        Ok(status
            .applications
            .iter()
            .any(|a| a.session_id == self.session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_connections_are_transient() {
        assert!(transient("I/O error: failed to fill whole buffer"));
        assert!(transient("Connection reset by peer (os error 54)"));
        assert!(!transient("Invalid request (INVALID_MEDIA_SESSION_ID)"));
        assert!(load_error("failed to fill whole buffer").contains("closed the connection"));
    }

    /// Read-only probe of a cast device's receiver + media status:
    /// `CAST_HOST=10.0.0.131 CAST_PORT=8010 cargo test --release -- --ignored cast_status_live --nocapture`
    #[test]
    #[ignore]
    fn cast_status_live() {
        let host = std::env::var("CAST_HOST").expect("CAST_HOST");
        let port: u16 = std::env::var("CAST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8009);
        let dev = CastDevice::connect_without_host_verification(host, port).expect("connect");
        dev.connection.connect(RECEIVER).expect("connect receiver");
        let status = dev.receiver.get_status().expect("receiver status");
        eprintln!("standby={} volume={:?}", status.is_stand_by, status.volume);
        for app in &status.applications {
            eprintln!(
                "app {} '{}' session={} transport={} status='{}'",
                app.app_id, app.display_name, app.session_id, app.transport_id, app.status_text
            );
            dev.connection
                .connect(app.transport_id.as_str())
                .expect("connect app");
            match dev.media.get_status(app.transport_id.as_str(), None) {
                Ok(ms) => {
                    for e in &ms.entries {
                        eprintln!(
                            "  media: state={:?} idle_reason={:?} media={:?}",
                            e.player_state,
                            e.idle_reason,
                            e.media.as_ref().map(|m| (
                                &m.content_id,
                                &m.content_type,
                                m.stream_type
                            ))
                        );
                    }
                    if ms.entries.is_empty() {
                        eprintln!("  media: no entries");
                    }
                }
                Err(e) => eprintln!("  media status error: {e}"),
            }
        }
        if status.applications.is_empty() {
            eprintln!("no running applications");
        }
    }

    /// Cast a silent live stream from a throwaway server and trace what the
    /// receiver does with it (plays only silence):
    /// `CAST_HOST=10.0.0.131 CAST_PORT=8010 cargo test --release -- --ignored cast_trace_live --nocapture`
    #[test]
    #[ignore]
    fn cast_trace_live() {
        use std::sync::{Arc, Mutex};
        let host: std::net::Ipv4Addr = std::env::var("CAST_HOST")
            .expect("CAST_HOST")
            .parse()
            .unwrap();
        let port: u16 = std::env::var("CAST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8009);
        let http_port = 18_365;
        let state = Arc::new(Mutex::new(crate::remote::SharedRemoteState::default()));
        state.lock().unwrap().stream_enabled = true;
        let (tx, _rx) = crossbeam_channel::bounded(4);
        let server = crate::remote::start_server(http_port, state, tx).expect("bind");

        let target = CastTarget {
            id: "trace".into(),
            name: "trace".into(),
            model: String::new(),
            protocol: super::super::Protocol::GoogleCast,
            location: String::new(),
            sonos: false,
            airplay: None,
            addr: host,
            port,
        };
        let meta = TrackMeta {
            title: "Phosphor test (silence)".into(),
            artist: "Phosphor".into(),
        };
        let s = Session::start(&target, http_port, &meta).expect("start");
        eprintln!("loaded {} (session {})", s.stream_url, s.session_id);
        for t in 0..12 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let dev = Session::connect(&target).expect("reconnect");
            dev.connection.connect(s.transport_id.as_str()).unwrap();
            match dev.media.get_status(s.transport_id.as_str(), None) {
                Ok(ms) if ms.entries.is_empty() => eprintln!("t={t}s media: no entries"),
                Ok(ms) => {
                    for e in &ms.entries {
                        eprintln!(
                            "t={t}s media: state={:?} idle_reason={:?}",
                            e.player_state, e.idle_reason
                        );
                    }
                }
                Err(e) => eprintln!("t={t}s media status error: {e}"),
            }
        }
        let _ = s.stop();
        server.unblock();
    }

    /// Stop whatever app runs on the receiver (to reproduce a cold start).
    /// `CAST_HOST=… CAST_PORT=… <test-bin> --ignored cast_stop_app_live --nocapture`
    #[test]
    #[ignore]
    fn cast_stop_app_live() {
        let host = std::env::var("CAST_HOST").expect("CAST_HOST");
        let port: u16 = std::env::var("CAST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8009);
        let dev = CastDevice::connect_without_host_verification(host, port).expect("connect");
        dev.connection.connect(RECEIVER).expect("connect receiver");
        for app in dev.receiver.get_status().expect("status").applications {
            eprintln!(
                "stopping {} -> {:?}",
                app.display_name,
                dev.receiver.stop_app(app.session_id.as_str())
            );
        }
    }
}

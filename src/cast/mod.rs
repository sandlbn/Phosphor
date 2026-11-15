//! Casting Phosphor's audio to network speakers.
//!
//! The software engines already publish a live MP3 at `/api/stream.mp3`
//! (see `audio_stream.rs` / `remote.rs`). Casting hands that URL to a
//! speaker which then pulls it itself, so no audio passes through this
//! module — it only does discovery and remote control.
//!
//! Threads: one discovery thread per protocol, and one worker that runs
//! device commands sequentially (each under a timeout). The App talks to
//! them through [`CastManager`].

#[cfg_attr(not(feature = "airplay"), allow(dead_code))]
pub mod airplay;
pub mod discovery;
pub mod google;
pub mod netaddr;
pub mod upnp;
pub mod volumes;

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    GoogleCast,
    AirPlay,
    Upnp,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::GoogleCast => "Google Cast",
            Self::AirPlay => "AirPlay",
            Self::Upnp => "Sonos & DLNA",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CastTarget {
    /// Stable id: mDNS fullname (Cast) or UDN (UPnP).
    pub id: String,
    pub name: String,
    pub model: String,
    pub protocol: Protocol,
    pub addr: Ipv4Addr,
    pub port: u16,
    /// UPnP device-description URL (empty for Google Cast).
    pub location: String,
    /// Sonos needs its own URI scheme and group handling.
    pub sonos: bool,
    /// How to pair with an AirPlay receiver (None for other protocols).
    pub airplay: Option<AirPlayAuth>,
}

/// Pairing / transport choice for an AirPlay receiver, derived from its
/// mDNS TXT record (see `discovery::airplay_auth`).
#[derive(Debug, Clone, PartialEq)]
pub struct AirPlayAuth {
    /// `phosphor_airplay.h` AP_AUTH_* value.
    pub auth: u8,
    pub airplay2: bool,
    /// Receiver's `deviceid` (keys stored pairing credentials).
    pub device_id: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CastState {
    Idle,
    Connecting {
        id: String,
        name: String,
    },
    Casting {
        id: String,
        name: String,
        /// Speaker volume (0..1) when the cast began, if the device told us
        /// (or, for AirPlay, the level we set). The volume slider shows it.
        volume: Option<f32>,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub enum CastEvent {
    /// Full current device list for one protocol.
    Devices(Protocol, Vec<CastTarget>),
    /// One device found at a new address / port (re-resolved after a
    /// failed connect).
    DeviceUpdated(CastTarget),
    State(CastState),
}

#[derive(Debug, Clone)]
pub enum CastCmd {
    Start {
        target: CastTarget,
        stream_port: u16,
        meta: TrackMeta,
        /// Starting volume for AirPlay, which otherwise plays at 0 dB
        /// (full). Cast / DLNA keep the speaker's own volume.
        airplay_volume: f32,
    },
    Stop,
    Volume(f32),
    Track(TrackMeta),
}

static LOSSLESS: AtomicBool = AtomicBool::new(false);

/// Have Cast / DLNA speakers pull lossless FLAC instead of MP3. Read when a
/// session starts. AirPlay is always lossless (ALAC) regardless.
pub fn set_lossless(on: bool) {
    LOSSLESS.store(on, Ordering::Relaxed);
}

/// (URL path, MIME type) speakers should pull. Sonos only plays endless
/// streams through its MP3 internet-radio scheme, so it always gets MP3.
pub(crate) fn stream_path(sonos: bool) -> (&'static str, &'static str) {
    if LOSSLESS.load(Ordering::Relaxed) && !sonos {
        ("/api/stream.flac", "audio/flac")
    } else {
        ("/api/stream.mp3", "audio/mpeg")
    }
}

pub struct CastManager {
    cmd_tx: Sender<CastCmd>,
    events: Receiver<CastEvent>,
    event_tx: Sender<CastEvent>,
    discovery_started: bool,
}

impl CastManager {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let (event_tx, events) = unbounded();
        let tx = event_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("cast-worker".into())
            .spawn(move || worker(cmd_rx, tx));
        if let Err(e) = spawned {
            eprintln!("[cast] cannot start worker thread: {e}");
        }
        Self {
            cmd_tx,
            events,
            event_tx,
            discovery_started: false,
        }
    }

    /// Start browsing for devices (lazy: only once the user opens the
    /// cast panel, so idle users generate no multicast traffic).
    pub fn ensure_discovery(&mut self) {
        if !self.discovery_started {
            self.discovery_started = true;
            discovery::spawn(self.event_tx.clone());
            upnp::spawn_discovery(self.event_tx.clone());
        }
    }

    pub fn send(&self, cmd: CastCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn try_event(&self) -> Option<CastEvent> {
        self.events.try_recv().ok()
    }

    /// Stop any cast and wait (bounded) until the speaker is released.
    /// Called on app exit: an AirPlay sender that just vanishes skips the
    /// TEARDOWN, and receivers then hold the dead session for minutes.
    pub fn shutdown(&self, wait: Duration) {
        let _ = self.cmd_tx.send(CastCmd::Stop);
        let deadline = std::time::Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(CastEvent::State(CastState::Idle)) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }
}

// ── Sessions ────────────────────────────────────────────────────────────

/// A running cast on one device. Implementations make blocking network
/// calls; the worker runs each under [`timed`].
pub(crate) trait Sink: Send + Sync {
    fn target(&self) -> &CastTarget;
    fn stream_url(&self) -> &str;
    fn stop(&self) -> Result<(), String>;
    fn set_volume(&self, level: f32) -> Result<(), String>;
    fn update_track(&self, meta: &TrackMeta) -> Result<(), String>;
    /// Speaker volume (0..1) when the session began, if known.
    fn volume(&self) -> Option<f32>;
    /// Is the device still playing our stream (not stopped / switched
    /// to another source by someone else)?
    fn still_ours(&self) -> Result<bool, String>;
}

pub(crate) type SinkRef = Arc<dyn Sink>;

fn start_sink(
    target: &CastTarget,
    port: u16,
    meta: &TrackMeta,
    airplay_volume: f32,
) -> Result<SinkRef, String> {
    match target.protocol {
        Protocol::GoogleCast => Ok(Arc::new(google::Session::start(target, port, meta)?)),
        Protocol::AirPlay => airplay::start(target, meta, airplay_volume),
        Protocol::Upnp => Ok(Arc::new(upnp::Session::start(target, port, meta)?)),
    }
}

/// The device didn't answer at its known address: worth re-resolving.
fn unreachable(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "refused",
        "unreachable",
        "no route",
        "timed out",
        "did not answer",
        "no answer",
    ]
    .iter()
    .any(|m| e.contains(m))
}

/// How often to check the session is still ours.
const POLL: Duration = Duration::from_secs(10);
/// Device libraries have no I/O timeouts: a device that stops answering
/// would block forever (seen: ~8 h on an emulated Cast receiver). Every
/// operation runs on a helper thread and is abandoned after this long.
const OP_TIMEOUT: Duration = Duration::from_secs(15);

/// Run a blocking device operation with a deadline. On timeout the helper
/// thread is left to finish (or die with its socket) on its own.
pub(crate) fn timed<T: Send + 'static>(
    what: &'static str,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    timed_for(OP_TIMEOUT, what, f)
}

/// Starting = connect + app launch + LOAD confirmation (+ one retry on a
/// dropped connection), so it gets a longer deadline than other ops.
const START_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn timed_for<T: Send + 'static>(
    limit: Duration,
    what: &'static str,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("cast-op".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| format!("{what}: {e}"))?;
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| Err(format!("{what}: device did not answer")))
}

fn on<T: Send + 'static>(
    s: &SinkRef,
    what: &'static str,
    op: impl FnOnce(&dyn Sink) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let s = Arc::clone(s);
    timed(what, move || op(s.as_ref()))
}

fn worker(cmds: Receiver<CastCmd>, events: Sender<CastEvent>) {
    let state = |s: CastState| {
        let _ = events.send(CastEvent::State(s));
    };
    let mut session: Option<SinkRef> = None;
    let mut pending: Option<CastCmd> = None;
    // One unreachable poll shouldn't end a cast; two in a row do.
    let mut failed_polls = 0u8;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match cmds.recv_timeout(POLL) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(s) = &session {
                        let gone = match on(s, "status", |s| s.still_ours()) {
                            Ok(ours) => {
                                failed_polls = 0;
                                !ours
                            }
                            Err(e) => {
                                failed_polls += 1;
                                eprintln!("[cast] status poll failed ({failed_polls}): {e}");
                                failed_polls >= 2
                            }
                        };
                        if gone {
                            eprintln!(
                                "[cast] session on {} ended outside Phosphor",
                                s.target().name
                            );
                            session = None;
                            state(CastState::Idle);
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match cmd {
            CastCmd::Start {
                target,
                stream_port,
                meta,
                airplay_volume,
            } => {
                if let Some(old) = session.take() {
                    let _ = on(&old, "stop", |s| s.stop());
                }
                failed_polls = 0;
                state(CastState::Connecting {
                    id: target.id.clone(),
                    name: target.name.clone(),
                });
                let attempt = |t: CastTarget, meta: TrackMeta| {
                    timed_for(START_TIMEOUT, "start", move || {
                        start_sink(&t, stream_port, &meta, airplay_volume)
                    })
                };
                let mut target = target;
                let mut result = attempt(target.clone(), meta.clone());
                if let Err(e) = &result {
                    // Receivers can restart on a new port; a cached record
                    // keeps pointing at the old one. Re-resolve from
                    // scratch and retry once if the device moved.
                    if unreachable(e) {
                        if let Some(fresh) = discovery::refresh_target(&target) {
                            if (fresh.addr, fresh.port) != (target.addr, target.port) {
                                eprintln!(
                                    "[cast] {} moved {}:{} -> {}:{}; retrying",
                                    target.name, target.addr, target.port, fresh.addr, fresh.port
                                );
                                let _ = events.send(CastEvent::DeviceUpdated(fresh.clone()));
                                target = fresh;
                                result = attempt(target.clone(), meta.clone());
                            }
                        }
                    }
                }
                match result {
                    Ok(s) => {
                        eprintln!(
                            "[cast] casting {} to {} ({}, {})",
                            s.stream_url(),
                            target.name,
                            target.protocol.label(),
                            target.addr
                        );
                        state(CastState::Casting {
                            id: target.id.clone(),
                            name: target.name.clone(),
                            volume: s.volume(),
                        });
                        session = Some(s);
                    }
                    Err(e) => {
                        eprintln!("[cast] start on {} failed: {e}", target.name);
                        state(CastState::Error(format!("{}: {e}", target.name)));
                    }
                }
            }
            CastCmd::Stop => {
                if let Some(s) = session.take() {
                    if let Err(e) = on(&s, "stop", |s| s.stop()) {
                        eprintln!("[cast] stop on {} failed: {e}", s.target().name);
                    }
                    // Whether or not the speaker obeyed, stop feeding it.
                    crate::remote::kick_stream(std::net::IpAddr::V4(s.target().addr));
                }
                state(CastState::Idle);
            }
            CastCmd::Volume(mut v) => {
                // Slider drags send a burst; only the latest value matters.
                while let Ok(next) = cmds.try_recv() {
                    match next {
                        CastCmd::Volume(n) => v = n,
                        other => {
                            pending = Some(other);
                            break;
                        }
                    }
                }
                if let Some(s) = &session {
                    if let Err(e) = on(s, "volume", move |s| s.set_volume(v)) {
                        eprintln!("[cast] volume on {} failed: {e}", s.target().name);
                    }
                }
            }
            CastCmd::Track(meta) => {
                if let Some(s) = session.clone() {
                    if let Err(e) = on(&s, "track", move |s| s.update_track(&meta)) {
                        eprintln!("[cast] track update on {} failed: {e}", s.target().name);
                        // A title update failing isn't fatal while the
                        // speaker still plays our stream (some devices
                        // refuse media-channel messages but play fine).
                        if matches!(on(&s, "status", |s| s.still_ours()), Ok(true)) {
                            continue;
                        }
                        session = None;
                        state(CastState::Error(format!(
                            "{}: cast session ended ({e})",
                            s.target().name
                        )));
                    }
                }
            }
        }
    }
}

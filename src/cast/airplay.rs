//! AirPlay 1 / 2 sender (HomePod, Apple TV, AirPlay TVs and speakers).
//!
//! Unlike Cast and UPnP, AirPlay receivers don't pull a URL: we push PCM.
//! A pump thread moves chunks from the engines' raw PCM tap
//! (`audio_stream::subscribe_pcm`) into the vendored C++ sender
//! (`native/airplay`, airplay2-sender-cpp), which pairs, encrypts, ALAC-
//! frames and paces them. Built only with the `airplay` cargo feature.

use std::sync::Arc;

use super::{CastTarget, SinkRef, TrackMeta};

/// AP_AUTH_* values from `native/airplay/phosphor_airplay.h`.
pub const AUTH_NONE: u8 = 0;
pub const AUTH_HAP_TRANSIENT: u8 = 4;
pub const AUTH_HAP_PIN: u8 = 5;

pub(super) fn start(target: &CastTarget, meta: &TrackMeta, volume: f32) -> Result<SinkRef, String> {
    #[cfg(feature = "airplay")]
    {
        Ok(Arc::new(imp::Session::start(target, meta, volume)?))
    }
    #[cfg(not(feature = "airplay"))]
    {
        let _ = (target, meta, volume, Arc::<()>::new(()));
        Err("this build has no AirPlay support (built without the `airplay` feature)".into())
    }
}

/// Feature bits from the `features` TXT key ("0xLO,0xHI").
pub fn parse_features(s: &str) -> u64 {
    let mut parts = s.split(',').map(|p| {
        u64::from_str_radix(
            p.trim().trim_start_matches("0x").trim_start_matches("0X"),
            16,
        )
        .unwrap_or(0)
    });
    let lo = parts.next().unwrap_or(0);
    let hi = parts.next().unwrap_or(0);
    (hi << 32) | (lo & 0xFFFF_FFFF)
}

/// Pick pairing + transport from the receiver's TXT record.
///
/// * HomePod (`AudioAccessory*`) and Macs: AirPlay 2, HAP transient
///   pairing (fixed PIN 3939, no UI).
/// * Apple TV 4+ (`AppleTV*` with AirPlay 2 pairing bits): on-screen PIN.
/// * Other AirPlay 2 receivers (TVs, Fire TV, speakers): transient —
///   verified against Amazon's Fire TV receiver.
/// * Everything else: plain AirPlay 1 (RAOP, no pairing).
///
/// AirPlay 2 = feature bit 38 (CoreUtils pairing + encryption) or bit 46
/// (HomeKit pairing + access control), the test pyatv uses.
pub fn auth_for(model: &str, features: u64) -> (u8, bool) {
    let ap2 = features & (1 << 38) != 0 || features & (1 << 46) != 0;
    if model.starts_with("AudioAccessory") || model.starts_with("Mac") {
        (AUTH_HAP_TRANSIENT, true)
    } else if model.starts_with("AppleTV") && ap2 {
        (AUTH_HAP_PIN, true)
    } else if ap2 {
        (AUTH_HAP_TRANSIENT, true)
    } else {
        (AUTH_NONE, false)
    }
}

#[cfg(feature = "airplay")]
mod imp {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

    use super::super::{CastTarget, Sink, TrackMeta};
    use super::AUTH_HAP_PIN;

    #[repr(C)]
    struct ApSession {
        _private: [u8; 0],
    }

    type EventCb = extern "C" fn(*mut c_void, c_int, *const c_char);

    extern "C" {
        fn ap_create(
            host: *const c_char,
            port: u16,
            sender_name: *const c_char,
            auth: c_int,
            airplay2: c_int,
            receiver_device_id: *const c_char,
            sample_rate: u32,
            initial_volume_pct: f64,
            cb: EventCb,
            user: *mut c_void,
        ) -> *mut ApSession;
        fn ap_push_pcm(s: *mut ApSession, samples: *const i16, n: usize) -> c_int;
        fn ap_set_volume(s: *mut ApSession, pct: f64);
        fn ap_set_now_playing(
            s: *mut ApSession,
            title: *const c_char,
            artist: *const c_char,
            album: *const c_char,
        );
        fn ap_destroy(s: *mut ApSession);
    }

    enum Ev {
        Launched,
        Failed(String),
        Closed,
        PinRequired,
    }

    /// Called on the C++ session thread. `user` is the `Box<Sender<Ev>>`
    /// owned by `Session`, which outlives the C session (ap_destroy runs
    /// first and no event fires after it returns).
    extern "C" fn on_event(user: *mut c_void, kind: c_int, msg: *const c_char) {
        let tx = unsafe { &*(user as *const Sender<Ev>) };
        let text = if msg.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned()
        };
        let _ = tx.send(match kind {
            1 => Ev::Launched,
            2 => Ev::Failed(text),
            3 => Ev::Closed,
            _ => Ev::PinRequired,
        });
    }

    /// Raw session pointer. The C API is thread-safe for everything we call
    /// across threads (volume / now-playing take a mutex); `ap_push_pcm` is
    /// only ever called from the single pump thread.
    #[derive(Clone, Copy)]
    struct Handle(*mut ApSession);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    pub(crate) struct Session {
        target: CastTarget,
        url: String,
        handle: Mutex<Option<Handle>>,
        events: Receiver<Ev>,
        _events_tx: Box<Sender<Ev>>,
        closed: AtomicBool,
        volume: f32,
        pump_stop: Arc<AtomicBool>,
        pump: Mutex<Option<JoinHandle<()>>>,
    }

    fn cstr(s: &str) -> CString {
        CString::new(s.replace('\0', "")).unwrap_or_default()
    }

    impl Session {
        pub(crate) fn start(
            target: &CastTarget,
            meta: &TrackMeta,
            volume: f32,
        ) -> Result<Self, String> {
            let volume = volume.clamp(0.0, 1.0);
            let ap = target
                .airplay
                .clone()
                .ok_or("missing AirPlay pairing info")?;
            if ap.auth == AUTH_HAP_PIN {
                return Err("this Apple TV needs on-screen PIN pairing, which Phosphor \
                     doesn't support yet"
                    .into());
            }
            let (tx, events) = crossbeam_channel::unbounded();
            let tx = Box::new(tx);
            let user = &*tx as *const Sender<Ev> as *mut c_void;
            let (host, name, dev) = (
                cstr(&target.addr.to_string()),
                cstr("Phosphor"),
                cstr(&ap.device_id),
            );
            let rate = crate::audio_stream::sample_rate();
            let raw = unsafe {
                ap_create(
                    host.as_ptr(),
                    target.port,
                    name.as_ptr(),
                    ap.auth as c_int,
                    ap.airplay2 as c_int,
                    dev.as_ptr(),
                    rate,
                    f64::from(volume) * 100.0,
                    on_event,
                    user,
                )
            };
            if raw.is_null() {
                return Err("out of memory".into());
            }
            let handle = Handle(raw);
            let launched = loop {
                match events.recv_timeout(Duration::from_secs(12)) {
                    Ok(Ev::Launched) => break Ok(()),
                    Ok(Ev::Failed(m)) => break Err(m),
                    Ok(Ev::PinRequired) => {
                        break Err("the receiver asks for a PIN, which Phosphor doesn't \
                             support yet"
                            .to_string())
                    }
                    Ok(Ev::Closed) => break Err("the receiver closed the connection".into()),
                    Err(_) => break Err("no answer from the receiver".into()),
                }
            };
            if let Err(e) = launched {
                unsafe { ap_destroy(handle.0) };
                return Err(e);
            }

            // Feed PCM only once streaming, so the ring doesn't fill up
            // during the handshake and add permanent latency.
            let pump_stop = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&pump_stop);
            let pump = std::thread::Builder::new()
                .name("airplay-pcm".into())
                .spawn(move || {
                    let h = handle;
                    let rx = crate::audio_stream::subscribe_pcm();
                    let mut buf: Vec<i16> = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(chunk) => {
                                buf.clear();
                                buf.extend(chunk.iter().flat_map(|&(l, r)| [l, r]));
                                // Full ring = receiver behind; drop rather
                                // than block (it pads silence itself).
                                unsafe { ap_push_pcm(h.0, buf.as_ptr(), buf.len()) };
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .map_err(|e| format!("pcm thread: {e}"))?;

            let s = Self {
                target: target.clone(),
                url: format!("airplay://{}:{}", target.addr, target.port),
                handle: Mutex::new(Some(handle)),
                events,
                _events_tx: tx,
                closed: AtomicBool::new(false),
                volume,
                pump_stop,
                pump: Mutex::new(Some(pump)),
            };
            s.update_track(meta)?;
            Ok(s)
        }

        fn with_handle(&self, f: impl FnOnce(*mut ApSession)) -> Result<(), String> {
            match *self.handle.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(h) => {
                    f(h.0);
                    Ok(())
                }
                None => Err("AirPlay session already stopped".into()),
            }
        }

        fn shutdown(&self) {
            self.pump_stop.store(true, Ordering::Relaxed);
            if let Some(j) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = j.join();
            }
            // The pump is gone, so nothing can touch the pointer any more.
            if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
                unsafe { ap_destroy(h.0) };
            }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    impl Sink for Session {
        fn target(&self) -> &CastTarget {
            &self.target
        }

        fn stream_url(&self) -> &str {
            &self.url
        }

        fn stop(&self) -> Result<(), String> {
            self.shutdown();
            Ok(())
        }

        fn set_volume(&self, level: f32) -> Result<(), String> {
            let pct = f64::from(level.clamp(0.0, 1.0)) * 100.0;
            self.with_handle(|h| unsafe { ap_set_volume(h, pct) })
        }

        fn update_track(&self, meta: &TrackMeta) -> Result<(), String> {
            let title = cstr(if meta.title.is_empty() {
                "Phosphor"
            } else {
                &meta.title
            });
            let (artist, album) = (cstr(&meta.artist), cstr("Phosphor — SID player"));
            self.with_handle(|h| unsafe {
                ap_set_now_playing(h, title.as_ptr(), artist.as_ptr(), album.as_ptr())
            })
        }

        fn volume(&self) -> Option<f32> {
            Some(self.volume)
        }

        fn still_ours(&self) -> Result<bool, String> {
            while let Ok(ev) = self.events.try_recv() {
                if matches!(ev, Ev::Closed | Ev::Failed(_)) {
                    self.closed.store(true, Ordering::Relaxed);
                }
            }
            Ok(!self.closed.load(Ordering::Relaxed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_parse_both_words() {
        assert_eq!(
            parse_features("0x7F8AD0,0x38BCF46"),
            (0x38BCF46u64 << 32) | 0x7F8AD0
        );
        assert_eq!(parse_features("0x527FFFF7"), 0x527FFFF7);
        assert_eq!(parse_features(""), 0);
    }

    #[test]
    fn auth_choice_by_receiver() {
        // Fire TV's own AirPlay 2 receiver (verified live with transient).
        let fire = parse_features("0x7F8AD0,0x38BCF46");
        assert_eq!(auth_for("AFTTIFF43", fire), (AUTH_HAP_TRANSIENT, true));
        // AirReceiver emulating an Apple TV 3: AirPlay 1.
        assert_eq!(
            auth_for("AppleTV3,1", parse_features("0x527FFFF7,0x8")),
            (AUTH_NONE, false)
        );
        // HomePod: transient regardless of advertised bits.
        assert_eq!(auth_for("AudioAccessory5,1", 0), (AUTH_HAP_TRANSIENT, true));
        // Modern Apple TV: PIN.
        assert_eq!(auth_for("AppleTV11,1", 1 << 46), (AUTH_HAP_PIN, true));
    }

    /// Live AirPlay session through the FFI (silent: no engine feeds PCM, so
    /// the sender pads silence):
    /// `AIRPLAY_HOST=10.0.0.131 AIRPLAY_PORT=7000 AIRPLAY_ID=… <bin> --ignored airplay_live --nocapture`
    #[cfg(feature = "airplay")]
    #[test]
    #[ignore]
    fn airplay_live() {
        use super::super::{Protocol, Sink};
        let host: std::net::Ipv4Addr = std::env::var("AIRPLAY_HOST").unwrap().parse().unwrap();
        let port: u16 = std::env::var("AIRPLAY_PORT").unwrap().parse().unwrap();
        let target = CastTarget {
            id: "t".into(),
            name: "test".into(),
            model: String::new(),
            protocol: Protocol::AirPlay,
            addr: host,
            port,
            location: String::new(),
            sonos: false,
            airplay: Some(super::super::AirPlayAuth {
                auth: AUTH_HAP_TRANSIENT,
                airplay2: true,
                device_id: std::env::var("AIRPLAY_ID").unwrap_or_default(),
            }),
        };
        let meta = TrackMeta {
            title: "Phosphor test (silence)".into(),
            artist: "Phosphor".into(),
        };
        let t0 = std::time::Instant::now();
        let s = imp::Session::start(&target, &meta, 0.3).expect("start");
        eprintln!("launched in {:?}", t0.elapsed());
        for i in 0..8 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if i == 3 {
                eprintln!(
                    "track update -> {:?}",
                    s.update_track(&TrackMeta {
                        title: "second".into(),
                        artist: "x".into()
                    })
                );
            }
            eprintln!("t={i}s still_ours={:?}", s.still_ours());
        }
        let t1 = std::time::Instant::now();
        eprintln!("stop -> {:?} in {:?}", s.stop(), t1.elapsed());
        eprintln!("after stop: set_volume -> {:?}", s.set_volume(0.5));
    }
}

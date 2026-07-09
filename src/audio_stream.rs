// Server-side MP3 audio bus for the built-in web UI.
//
// The two software SID engines (reSID and SIDLite) fan a copy of every
// stereo sample pair they push to their cpal ring buffer into the global
// `AudioTap` in this module. When one or more browsers open the
// `/api/stream.mp3` endpoint served by `remote.rs`, a background encoder
// thread wakes up, batches the PCM into MP3 frames using the `mp3lame`
// C library (vendored via `mp3lame-sys` — no system libmp3lame needed on
// any target), and fans the encoded bytes out to each subscriber. Each
// subscriber implements `std::io::Read`, so tiny_http can stream the
// body straight out to the socket without any buffering above ours.
//
// Design notes:
//   - The producer path (`push_pairs`) does a fast-path bail-out when
//     no browsers are listening. Cost when idle = one atomic-bool load.
//   - The encoder thread is spawned on the first subscribe and shut
//     down when the last subscriber drops. No CPU cost when idle.
//   - Each subscriber's outbound queue is bounded (`SUBSCRIBER_CAPACITY`
//     encoded chunks ~= 1-2 seconds of audio). On overrun the OLDEST
//     chunk is dropped so a slow client never blocks the encoder or
//     the other listeners. Diagnostic byte counter reports drops.
//   - If the audio engine goes idle (paused playback, track transition),
//     the encoder injects silence at the MP3 frame boundary so the
//     `<audio>` element doesn't underrun and terminate the stream.
//   - `is_available()` reports whether audio flowed recently enough
//     that a listener would hear something. The web UI polls it to
//     gate the 🔊 button when the active engine is USB / U64.
//
// Encoding config is HQ out of the box: 256 kbps CBR, LAME quality
// preset 2 ("V0" tier). Any modern browser handles it; CPU ceiling on
// current hardware is ~15% single-core while streaming.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Recover from a poisoned mutex instead of propagating the panic.
///
/// If one of the audio-pipeline threads (SID engines, encoder, HTTP
/// server) panics while holding a shared mutex, the standard
/// `.lock().unwrap()` cascades the poison into every other thread
/// that touches that mutex. In our case that took down the whole
/// HTTP server thread — every subsequent request refused at the TCP
/// layer.
///
/// The queues we lock through here are self-healing: the encoder
/// keeps draining fresh PCM regardless of what was in the queue
/// beforehand, and the subscriber `scratch` state is per-request.
fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

use mp3lame_encoder::{Bitrate, Builder, DualPcm, Mode, Quality};

// ── Config ────────────────────────────────────────────────────────

/// 256 kbps CBR — transparent for chip music, universal browser support.
const MP3_BITRATE: Bitrate = Bitrate::Kbps256;
/// LAME algorithm-quality preset 2 (~V0 tier). Best fidelity that still
/// fits well inside a single-core budget.
const MP3_QUALITY: Quality = Quality::NearBest;
/// One MP3 frame at MPEG-1 Layer III = 1152 samples/channel. We encode
/// exactly one frame per iteration so the output is a clean stream of
/// aligned frames a browser can start decoding mid-connection.
const PCM_BATCH_PAIRS: usize = 1152;
/// If the pcm ring gets this large, drop from the head — a runaway
/// producer with no consumer can't be allowed to eat memory. In steady
/// state the ring stays near-empty.
const PCM_MAX_PAIRS: usize = 48_000; // ~1 second at 48 kHz
/// Per-subscriber encoded-chunk backlog. Each chunk is one MP3 frame's
/// worth of bytes (~700–850 at 256 kbps).
const SUBSCRIBER_CAPACITY: usize = 64;
/// While a producer is actively pushing PCM, give the wait loop plenty
/// of time to accumulate a full 1152-sample frame — the SID engines
/// deliver at ~48 kHz so a frame fills in ~24 ms. 60 ms covers any
/// jitter without blocking the encoder if playback drops out mid-frame.
///
/// Using this longer timeout only in producer-active mode is critical:
/// truncating a real-audio frame partway (say, at 20 ms) and padding
/// the tail with 4 ms of injected zeros produces a per-frame silence
/// step that MSE-backed browser decoders reject as unplayable.
const PRODUCER_TIMEOUT: Duration = Duration::from_millis(60);
/// While the producer is idle, emit silence frames faster than
/// real-time so the browser's decode buffer keeps growing. 15 ms is
/// well below the ~24 ms/frame decode clock — steady positive margin.
const IDLE_TIMEOUT: Duration = Duration::from_millis(15);
/// A push older than this counts as "producer stale" — flip to the
/// short IDLE_TIMEOUT so silence stays ahead of real time.
const PRODUCER_STALE: Duration = Duration::from_millis(80);
/// `is_available()` returns true if audio flowed within this window.
/// Web UI polls to know whether to enable the 🔊 button.
const AVAILABILITY_WINDOW: Duration = Duration::from_secs(3);

// ── Global tap ────────────────────────────────────────────────────

struct AudioTap {
    pcm: Mutex<VecDeque<(i16, i16)>>,
    pcm_cond: Condvar,
    sample_rate: AtomicU32,
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    /// True while a background encoder thread is alive.
    encoder_running: AtomicBool,
    /// Monotonic timestamp of the last non-silent push. Read by
    /// `is_available()` and by the silence-injection path.
    last_push: Mutex<Instant>,
    /// Cheap-to-check subscriber presence — avoids a Mutex lock on the
    /// producer hot path.
    has_subscribers: AtomicBool,
}

fn tap() -> &'static Arc<AudioTap> {
    static TAP: OnceLock<Arc<AudioTap>> = OnceLock::new();
    TAP.get_or_init(|| {
        Arc::new(AudioTap {
            pcm: Mutex::new(VecDeque::new()),
            pcm_cond: Condvar::new(),
            sample_rate: AtomicU32::new(48_000),
            subscribers: Mutex::new(Vec::new()),
            encoder_running: AtomicBool::new(false),
            last_push: Mutex::new(Instant::now()),
            has_subscribers: AtomicBool::new(false),
        })
    })
}

// ── Producer API ──────────────────────────────────────────────────

/// Called by the audio engines on every batch of samples they push to
/// their cpal ring buffer. Fast-path bails out with a single atomic
/// load when no browsers are listening. `sample_rate` is the current
/// cpal output rate; the encoder is rebuilt if it changes.
pub fn push_pairs(pairs: &[(i16, i16)], sample_rate: u32) {
    let tap = tap();
    if !tap.has_subscribers.load(Ordering::Relaxed) {
        return;
    }
    tap.sample_rate.store(sample_rate, Ordering::Relaxed);
    {
        let mut pcm = lock_or_recover(&tap.pcm);
        // Drop from the head if a runaway producer overshoots the cap.
        // Steady state never hits this because the encoder drains at
        // real time; this is purely a memory-safety backstop.
        let overrun = pcm
            .len()
            .saturating_add(pairs.len())
            .saturating_sub(PCM_MAX_PAIRS);
        for _ in 0..overrun {
            pcm.pop_front();
        }
        pcm.extend(pairs.iter().copied());
    }
    *lock_or_recover(&tap.last_push) = Instant::now();
    tap.pcm_cond.notify_one();
}

/// Whether an `/api/stream/status` request should report the stream as
/// available for the current engine. True iff a producer pushed data
/// in the last few seconds.
pub fn is_available() -> bool {
    let last = *lock_or_recover(&tap().last_push);
    last.elapsed() < AVAILABILITY_WINDOW
}

/// Called by each audio engine's `open()` to lock in the cpal output
/// sample rate BEFORE any browser subscribes. Prevents the encoder
/// from initially building at the default 48 kHz and then rebuilding
/// mid-stream when the first real push arrives at, say, 44.1 kHz —
/// which produces an audible glitch or, on some browsers, a stall.
pub fn set_sample_rate(rate: u32) {
    if rate > 0 {
        tap().sample_rate.store(rate, Ordering::Relaxed);
    }
}

// ── Subscriber ────────────────────────────────────────────────────

pub struct Subscriber {
    queue: Mutex<VecDeque<Vec<u8>>>,
    cond: Condvar,
    /// Cumulative bytes dropped from the head when overrun. Exposed via
    /// a debug endpoint for a "the LAN is slow" indicator.
    dropped: AtomicU64,
    /// Set true when the tap is going away so the reader unblocks.
    closed: AtomicBool,
    /// Partial-chunk state for the `Read` impl — current chunk and its
    /// read offset. When both are consumed we fetch the next queued
    /// chunk.
    scratch: Mutex<(Vec<u8>, usize)>,
}

impl Subscriber {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(SUBSCRIBER_CAPACITY)),
            cond: Condvar::new(),
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            scratch: Mutex::new((Vec::new(), 0)),
        }
    }

    /// Push one encoded chunk; if the queue is full, evict the oldest
    /// so the writer never blocks the encoder or other subscribers.
    fn push_chunk(&self, chunk: Vec<u8>) {
        let mut q = lock_or_recover(&self.queue);
        if q.len() >= SUBSCRIBER_CAPACITY {
            if let Some(old) = q.pop_front() {
                self.dropped.fetch_add(old.len() as u64, Ordering::Relaxed);
            }
        }
        q.push_back(chunk);
        self.cond.notify_one();
    }
}

/// Handle handed to `tiny_http::Response::new` as the body. Owns a
/// clone of the subscriber Arc plus a back-pointer to the tap so
/// `Drop` can unregister and shut down the encoder if it was the last
/// listener.
pub struct SubscriberReader {
    sub: Arc<Subscriber>,
    tap: Arc<AudioTap>,
    bytes_read: u64,
    /// Last time we logged the "still connected" heartbeat.
    last_heartbeat: Instant,
}

impl SubscriberReader {
    /// For tests / debug — total bytes evicted from this subscriber's
    /// backlog due to slow-consumer back-pressure.
    #[allow(dead_code)]
    pub fn dropped_bytes(&self) -> u64 {
        self.sub.dropped.load(Ordering::Relaxed)
    }
}

impl Read for SubscriberReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Serve from the scratch chunk first; only touch the queue
        // (and its Condvar) when we've drained the current chunk.
        loop {
            {
                let mut scratch = lock_or_recover(&self.sub.scratch);
                let (chunk, off) = &mut *scratch;
                if *off < chunk.len() {
                    let n = (chunk.len() - *off).min(buf.len());
                    buf[..n].copy_from_slice(&chunk[*off..*off + n]);
                    *off += n;
                    let was = self.bytes_read;
                    self.bytes_read += n as u64;
                    if was == 0 && self.bytes_read > 0 {
                        eprintln!("[audio-stream] first bytes delivered to client: {n}");
                    }
                    // Heartbeat — separates "encoder stopped" from
                    // "browser gave up" when diagnosing a failure.
                    if self.last_heartbeat.elapsed() >= Duration::from_secs(5) {
                        eprintln!(
                            "[audio-stream] client still connected: {} bytes delivered",
                            self.bytes_read,
                        );
                        self.last_heartbeat = Instant::now();
                    }
                    return Ok(n);
                }
            }
            // Scratch drained — block until a new chunk arrives, or
            // return EOF if the tap has closed us.
            let mut q = lock_or_recover(&self.sub.queue);
            while q.is_empty() {
                if self.sub.closed.load(Ordering::Relaxed) {
                    return Ok(0);
                }
                // Timeout so we can periodically re-check `closed`.
                let (nq, _wr) = self
                    .sub
                    .cond
                    .wait_timeout(q, Duration::from_secs(1))
                    .unwrap_or_else(|p| p.into_inner());
                q = nq;
            }
            let chunk = q.pop_front().expect("queue non-empty after !q.is_empty()");
            drop(q);
            let mut scratch = lock_or_recover(&self.sub.scratch);
            *scratch = (chunk, 0);
        }
    }
}

impl Drop for SubscriberReader {
    fn drop(&mut self) {
        eprintln!(
            "[audio-stream] subscriber drop after delivering {} bytes ({} dropped by back-pressure)",
            self.bytes_read,
            self.sub.dropped.load(Ordering::Relaxed),
        );
        // Unregister first — no more chunks will be pushed.
        {
            let mut subs = lock_or_recover(&self.tap.subscribers);
            subs.retain(|s| !Arc::ptr_eq(s, &self.sub));
            if subs.is_empty() {
                self.tap.has_subscribers.store(false, Ordering::Relaxed);
            }
        }
        // Wake the encoder so it observes the empty subscribers list
        // and can exit its own loop cleanly.
        self.tap.pcm_cond.notify_all();
    }
}

/// Open a new listener. Spawns the encoder thread on the first call.
pub fn subscribe() -> SubscriberReader {
    let tap = tap();
    let sub = Arc::new(Subscriber::new());
    {
        let mut subs = lock_or_recover(&tap.subscribers);
        subs.push(sub.clone());
        tap.has_subscribers.store(true, Ordering::Relaxed);
    }
    // Spawn the encoder thread on the first live subscriber. If one is
    // already running the atomic swap prevents a duplicate.
    if !tap.encoder_running.swap(true, Ordering::AcqRel) {
        spawn_encoder_thread(tap.clone());
    }
    SubscriberReader {
        sub,
        tap: tap.clone(),
        bytes_read: 0,
        last_heartbeat: Instant::now(),
    }
}

// ── Encoder thread ────────────────────────────────────────────────

fn build_encoder(rate: u32) -> Option<mp3lame_encoder::Encoder> {
    let e = Builder::new()?
        .with_sample_rate(rate)
        .ok()?
        .with_num_channels(2)
        .ok()?
        .with_brate(MP3_BITRATE)
        .ok()?
        .with_quality(MP3_QUALITY)
        .ok()?
        .with_mode(Mode::JointStereo)
        .ok()?
        .build()
        .ok()?;
    Some(e)
}

fn broadcast(tap: &AudioTap, bytes: &[u8]) {
    let subs = lock_or_recover(&tap.subscribers);
    for sub in subs.iter() {
        sub.push_chunk(bytes.to_vec());
    }
}

fn spawn_encoder_thread(tap: Arc<AudioTap>) {
    thread::Builder::new()
        .name("mp3-encoder".into())
        .spawn(move || {
            eprintln!("[audio-stream] encoder thread started");
            let mut current_rate: u32 = 0;
            let mut encoder: Option<mp3lame_encoder::Encoder> = None;
            let mut left: Vec<i16> = Vec::with_capacity(PCM_BATCH_PAIRS);
            let mut right: Vec<i16> = Vec::with_capacity(PCM_BATCH_PAIRS);
            let mut frames_out: u64 = 0;
            let mut bytes_out: u64 = 0;
            let mut zero_frames: u64 = 0;
            // Composition counters — reset after every diagnostic log.
            // pure_audio = 1152 producer samples, no padding.
            // silence_pad = 0 producer samples, 1152 injected zeros.
            // hybrid = mix of producer + zero pad (the failure mode we're
            // hunting; should stay at zero after the timeout fix).
            let mut pure_audio: u32 = 0;
            let mut silence_pad: u32 = 0;
            let mut hybrid: u32 = 0;

            loop {
                // Exit path: last listener dropped, encoder can idle.
                if !tap.has_subscribers.load(Ordering::Relaxed) {
                    break;
                }

                // (Re)build encoder if the sample rate changed.
                let rate = tap.sample_rate.load(Ordering::Relaxed);
                if rate != current_rate {
                    encoder = build_encoder(rate);
                    current_rate = rate;
                    if encoder.is_none() {
                        eprintln!("[audio-stream] LAME builder failed for {rate} Hz");
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    eprintln!(
                        "[audio-stream] encoder (re)built: {rate} Hz stereo, \
                         256 kbps CBR, quality=NearBest"
                    );
                }

                // Adaptive timeout: while a producer has pushed
                // recently, wait long enough for a full 1152-sample
                // frame to accumulate cleanly. When it hasn't, flip to
                // the short idle timeout so silence stays ahead of the
                // decode clock.
                //
                // This split fixes the "hybrid frame" problem — a real
                // audio frame padded on the tail with a few ms of
                // injected silence, which browsers reject as unplayable.
                let producer_alive = lock_or_recover(&tap.last_push).elapsed() < PRODUCER_STALE;
                let timeout = if producer_alive {
                    PRODUCER_TIMEOUT
                } else {
                    IDLE_TIMEOUT
                };

                left.clear();
                right.clear();
                let mut pcm = lock_or_recover(&tap.pcm);
                let wait_start = Instant::now();
                let mut samples_from_producer = 0usize;
                while pcm.len() < PCM_BATCH_PAIRS {
                    let elapsed = wait_start.elapsed();
                    if elapsed >= timeout {
                        // Snapshot producer contribution before we pad,
                        // so composition counters get the true split.
                        samples_from_producer = pcm.len();
                        let need = PCM_BATCH_PAIRS - pcm.len();
                        for _ in 0..need {
                            pcm.push_back((0, 0));
                        }
                        break;
                    }
                    let (nq, _wr) = tap
                        .pcm_cond
                        .wait_timeout(pcm, timeout - elapsed)
                        .unwrap_or_else(|p| p.into_inner());
                    pcm = nq;
                    if !tap.has_subscribers.load(Ordering::Relaxed) {
                        break;
                    }
                }
                if !tap.has_subscribers.load(Ordering::Relaxed) {
                    break;
                }
                // If we broke out because the batch filled naturally
                // (all samples from producer) `samples_from_producer`
                // is still 0 — set it to the batch size for the stat.
                if samples_from_producer == 0 && pcm.len() >= PCM_BATCH_PAIRS {
                    samples_from_producer = PCM_BATCH_PAIRS;
                }
                for _ in 0..PCM_BATCH_PAIRS {
                    if let Some((l, r)) = pcm.pop_front() {
                        left.push(l);
                        right.push(r);
                    } else {
                        left.push(0);
                        right.push(0);
                    }
                }
                drop(pcm);

                if samples_from_producer == PCM_BATCH_PAIRS {
                    pure_audio += 1;
                } else if samples_from_producer == 0 {
                    silence_pad += 1;
                } else {
                    hybrid += 1;
                }

                let enc = match encoder.as_mut() {
                    Some(e) => e,
                    None => continue,
                };

                // Encode the frame. Reserve the LAME-recommended
                // headroom then set_len via encode's returned count.
                let cap = mp3lame_encoder::max_required_buffer_size(left.len());
                let mut mp3_buf: Vec<u8> = Vec::with_capacity(cap);
                let n = match enc.encode(
                    DualPcm {
                        left: left.as_slice(),
                        right: right.as_slice(),
                    },
                    mp3_buf.spare_capacity_mut(),
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("[audio-stream] encode error: {e:?}");
                        0
                    }
                };
                if n > 0 {
                    unsafe {
                        mp3_buf.set_len(n);
                    }
                    frames_out += 1;
                    bytes_out += n as u64;
                    if frames_out == 1 {
                        eprintln!(
                            "[audio-stream] first MP3 frame: {n} bytes, \
                             first 4 bytes = {:02x} {:02x} {:02x} {:02x} \
                             (should start 0xFF 0xFB)",
                            mp3_buf.first().copied().unwrap_or(0),
                            mp3_buf.get(1).copied().unwrap_or(0),
                            mp3_buf.get(2).copied().unwrap_or(0),
                            mp3_buf.get(3).copied().unwrap_or(0),
                        );
                    }
                    if frames_out % 200 == 0 {
                        eprintln!(
                            "[audio-stream] {frames_out} frames, {bytes_out} bytes, \
                             {zero_frames} zero-encodes, composition: \
                             {pure_audio} pure-audio / {silence_pad} silence-pad / {hybrid} hybrid"
                        );
                        pure_audio = 0;
                        silence_pad = 0;
                        hybrid = 0;
                    }
                    broadcast(&tap, &mp3_buf);
                } else {
                    zero_frames += 1;
                    if zero_frames <= 8 || zero_frames % 100 == 0 {
                        eprintln!("[audio-stream] encode returned 0 (priming) x{zero_frames}");
                    }
                }
            }

            // Notify readers so their `Read` unblocks and returns EOF.
            {
                let subs = lock_or_recover(&tap.subscribers);
                for s in subs.iter() {
                    s.closed.store(true, Ordering::Relaxed);
                    s.cond.notify_all();
                }
            }
            tap.encoder_running.store(false, Ordering::Relaxed);
        })
        .expect("mp3 encoder thread spawn");
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriber_drops_oldest_on_overrun() {
        // Push more chunks than the queue holds; oldest should be
        // evicted and `dropped_bytes` should reflect the eviction.
        let sub = Subscriber::new();
        let chunk_size = 100;
        let total = SUBSCRIBER_CAPACITY + 25;
        for i in 0..total {
            sub.push_chunk(vec![i as u8; chunk_size]);
        }
        assert_eq!(
            lock_or_recover(&sub.queue).len(),
            SUBSCRIBER_CAPACITY,
            "queue must cap at SUBSCRIBER_CAPACITY"
        );
        let dropped_expected = ((total - SUBSCRIBER_CAPACITY) * chunk_size) as u64;
        assert_eq!(
            sub.dropped.load(Ordering::Relaxed),
            dropped_expected,
            "dropped bytes count should match evicted chunks"
        );
    }

    #[test]
    fn drop_removes_subscriber_from_tap() {
        // Snapshot the subscriber-count delta across subscribe/drop.
        // Uses the real global tap; concurrency-safe because the test
        // holds the only references it creates.
        let before = lock_or_recover(&tap().subscribers).len();
        let a = subscribe();
        let b = subscribe();
        let c = subscribe();
        assert_eq!(lock_or_recover(&tap().subscribers).len(), before + 3);
        drop(b);
        assert_eq!(lock_or_recover(&tap().subscribers).len(), before + 2);
        drop(a);
        drop(c);
        assert_eq!(lock_or_recover(&tap().subscribers).len(), before);
        // has_subscribers should reflect the empty state — but only if
        // this test started from empty. Guard against parallel tests.
        if before == 0 {
            assert!(!tap().has_subscribers.load(Ordering::Relaxed));
        }
    }
}

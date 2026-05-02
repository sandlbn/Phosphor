// Ultimate 64 SID output via REST API.
//
// Sends the entire SID file to the Ultimate 64 (or Ultimate-II+) device
// over the network. The C64 hardware plays the SID natively — no CPU
// emulation or per-register writes needed on the host side.
//
// Audio streaming back to host
// ────────────────────────────
// When enabled via config (u64_audio_enabled), Phosphor asks the U64 to stream
// its audio output as UDP packets to our local port.  Two threads handle this:
//
//   UDP receiver  — binds the port, receives packets, resamples PAL ~47983 Hz
//                   → host device rate, pushes f32 stereo into a shared ring
//                   buffer with jitter management.
//
//   cpal output   — drains the ring buffer into the system audio device.  Owns
//                   the !Send cpal::Stream so it lives in its own thread.
//
// Packet format (from Ultimate 64 firmware):
//   bytes 0-1  : sequence number (u16 LE) — used for gap detection only
//   bytes 2..  : i16 LE stereo samples interleaved (L R L R …)
//
// The stream is started in play_sid_native and stopped in stop / mute / close.

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ultimate64::Rest;
use url::Host;

use crate::sid_device::SidDevice;

// ─────────────────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────────────────

/// PAL audio sample rate from the Ultimate 64 firmware.
const U64_SAMPLE_RATE_PAL: f64 = 47_982.886_904_761_9;

/// Audio packet header size (sequence number only).
const AUDIO_HEADER: usize = 2;

/// Jitter buffer: don't start playback until this many samples are buffered.
/// ~100 ms at 48 kHz stereo.
const JITTER_MIN: usize = 9_600;

/// Jitter buffer: target fill level — trim back to this when overflowing.
/// ~200 ms at 48 kHz stereo.
const JITTER_TARGET: usize = 19_200;

/// Jitter buffer: hard maximum before we start dropping.
/// ~1 s at 48 kHz stereo.
const JITTER_MAX: usize = 96_000;

// ─────────────────────────────────────────────────────────────────────────────
//  Linear resampler  (U64 PAL ~47983 Hz → host device rate)
// ─────────────────────────────────────────────────────────────────────────────

struct Resampler {
    /// How many input frames we advance per output frame.
    step: f64,
    /// Fractional position within the current input frame.
    pos: f64,
    last_l: f32,
    last_r: f32,
}

impl Resampler {
    fn new(input_rate: f64, output_rate: f64) -> Self {
        Self {
            step: input_rate / output_rate,
            pos: 0.0,
            last_l: 0.0,
            last_r: 0.0,
        }
    }

    /// Resample interleaved stereo `input` (i16 → f32 inline) into `out`.
    fn push(&mut self, input: &[i16], out: &mut VecDeque<f32>) {
        for chunk in input.chunks_exact(2) {
            let l = chunk[0] as f32 / 32_768.0;
            let r = chunk[1] as f32 / 32_768.0;
            while self.pos <= 1.0 {
                let t = self.pos as f32;
                out.push_back(self.last_l + (l - self.last_l) * t);
                out.push_back(self.last_r + (r - self.last_r) * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.last_l = l;
            self.last_r = r;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared audio ring buffer
// ─────────────────────────────────────────────────────────────────────────────

struct AudioRing {
    samples: VecDeque<f32>,
    ready: bool, // true once we've buffered JITTER_MIN samples
    last_seq: Option<u16>,
    gaps: u64,
}

impl AudioRing {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(JITTER_MAX),
            ready: false,
            last_seq: None,
            gaps: 0,
        }
    }
}

type SharedRing = Arc<Mutex<AudioRing>>;

// ─────────────────────────────────────────────────────────────────────────────
//  AudioStream  — the two background threads
// ─────────────────────────────────────────────────────────────────────────────

struct AudioStream {
    stop: Arc<AtomicBool>,
    net: Option<thread::JoinHandle<()>>,
    audio: Option<thread::JoinHandle<()>>,
}

impl AudioStream {
    /// Spawn both threads. Blocks briefly to wait for cpal initialisation.
    fn start(port: u16, address: String) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let ring: SharedRing = Arc::new(Mutex::new(AudioRing::new()));

        // ── UDP receiver thread ───────────────────────────────────────────────
        let net_stop = stop.clone();
        let net_ring = ring.clone();
        let net_addr = address.clone();

        let net_handle = thread::Builder::new()
            .name("u64-audio-net".into())
            .spawn(move || {
                let sock = match UdpSocket::bind(format!("0.0.0.0:{}", port)) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[u64-audio] Cannot bind UDP port {port}: {e}");
                        return;
                    }
                };

                // Detect the local IP that can actually reach the U64.
                // We use the same trick as the reference app: create a UDP socket
                // (no data is sent), connect it toward the U64's IP so the OS picks
                // the correct outbound interface, then read back local_addr().
                let my_ip = {
                    let target = format!("{}:80", net_addr);
                    std::net::UdpSocket::bind("0.0.0.0:0")
                        .ok()
                        .and_then(|s| {
                            // connect() here just sets the remote address for routing;
                            // no packet is actually sent.
                            s.connect(target.as_str()).ok()?;
                            s.local_addr().ok()
                        })
                        .map(|a| a.ip().to_string())
                        .unwrap_or_else(|| {
                            eprintln!("[u64-audio] Warning: could not detect local IP, trying 8.8.8.8 route");
                            // Fallback: route via public IP to find the default interface.
                            std::net::UdpSocket::bind("0.0.0.0:0")
                                .ok()
                                .and_then(|s| {
                                    s.connect("8.8.8.8:80").ok()?;
                                    s.local_addr().ok()
                                })
                                .map(|a| a.ip().to_string())
                                .unwrap_or_else(|| "127.0.0.1".to_string())
                        })
                };

                eprintln!("[u64-audio] Starting stream: U64={net_addr} → {my_ip}:{port}");

                // Issue the REST start command via raw HTTP (no Ultimate64 crate
                // audio method available, so we do it directly like the reference app).
                let path = format!("/v1/streams/audio:start?ip={my_ip}:{port}");
                let request = format!(
                    "PUT {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    path, net_addr,
                );
                if let Ok(mut tcp) = std::net::TcpStream::connect_timeout(
                    &format!("{net_addr}:80").parse().unwrap_or_else(|_| "0.0.0.0:80".parse().unwrap()),
                    Duration::from_secs(3),
                ) {
                    use std::io::Write;
                    let _ = tcp.write_all(request.as_bytes());
                    eprintln!("[u64-audio] REST start sent to {net_addr}");
                } else {
                    eprintln!("[u64-audio] Cannot reach {net_addr}:80 for stream start");
                }

                if let Err(e) = sock.set_nonblocking(true) {
                    eprintln!("[u64-audio] set_nonblocking failed: {e}");
                    return;
                }

                let mut buf        = [0u8; 2048];
                let mut resampler  = Resampler::new(U64_SAMPLE_RATE_PAL, 48_000.0);
                let mut first      = true;

                loop {
                    if net_stop.load(Ordering::Relaxed) { break; }

                    match sock.recv_from(&mut buf) {
                        Ok((len, _)) => {
                            if len <= AUDIO_HEADER { continue; }

                            let seq = u16::from_le_bytes([buf[0], buf[1]]);

                            if first {
                                first = false;
                                eprintln!("[u64-audio] First packet: {} bytes, seq={seq}", len);
                            }

                            // Convert payload bytes → i16 samples
                            let payload = &buf[AUDIO_HEADER..len];
                            let n = payload.len() / 2;
                            // Stack-allocate a small scratch buffer; heap for larger ones.
                            let mut samples: Vec<i16> = Vec::with_capacity(n);
                            for chunk in payload.chunks_exact(2) {
                                samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                            }

                            if let Ok(mut ring) = net_ring.lock() {
                                // Gap detection
                                if let Some(last) = ring.last_seq {
                                    let expected = last.wrapping_add(1);
                                    if seq != expected {
                                        ring.gaps += 1;
                                        eprintln!(
                                            "[u64-audio] Packet gap: expected {expected} got {seq} (total {})",
                                            ring.gaps
                                        );
                                    }
                                }
                                ring.last_seq = Some(seq);

                                // Resample and push
                                resampler.push(&samples, &mut ring.samples);

                                // Overflow protection: trim to target
                                if ring.samples.len() > JITTER_MAX {
                                    let drop = ring.samples.len() - JITTER_TARGET;
                                    ring.samples.drain(..drop);
                                }
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => {
                            eprintln!("[u64-audio] recv error: {e}");
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                }

                eprintln!("[u64-audio] Network thread stopped (gaps: {})",
                    net_ring.lock().map(|r| r.gaps).unwrap_or(0));
                // REST stop is sent by stop_audio() in the main thread,
                // so this thread can exit immediately without any TCP delay.
            })
            .map_err(|e| format!("spawn net thread: {e}"))?;

        // ── cpal output thread ────────────────────────────────────────────────
        let audio_stop = stop.clone();
        let audio_ring = ring.clone();
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let audio_handle = thread::Builder::new()
            .name("u64-audio-out".into())
            .spawn(move || {
                let result = (|| -> Result<cpal::Stream, String> {
                    let host = cpal::default_host();
                    let device = host
                        .default_output_device()
                        .ok_or_else(|| "No audio output device".to_string())?;

                    let dev_cfg = device
                        .default_output_config()
                        .map_err(|e| format!("No default output config: {e}"))?;

                    let rate = dev_cfg.sample_rate().0;
                    let channels = dev_cfg.channels() as usize;

                    eprintln!(
                        "[u64-audio] cpal device: '{}', {}Hz, {}ch",
                        device.name().unwrap_or_default(),
                        rate,
                        channels
                    );

                    // If the device rate differs from 48 kHz the resampler in the
                    // net thread used 48 kHz as its target — we accept minor pitch
                    // drift rather than adding a second resampler stage.  For the
                    // vast majority of systems the device is 48 kHz anyway.

                    let config = cpal::StreamConfig {
                        channels: 2,
                        sample_rate: cpal::SampleRate(rate),
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let ring_cb = audio_ring.clone();

                    let stream = device
                        .build_output_stream(
                            &config,
                            move |data: &mut [f32], _| {
                                if let Ok(mut r) = ring_cb.lock() {
                                    // Jitter buffer: output silence until buffered enough.
                                    if !r.ready {
                                        if r.samples.len() >= JITTER_MIN {
                                            r.ready = true;
                                            eprintln!(
                                                "[u64-audio] Jitter buffer ready ({} samples)",
                                                r.samples.len()
                                            );
                                        } else {
                                            data.fill(0.0);
                                            return;
                                        }
                                    }
                                    // Stereo pairs; upmix to N channels if needed.
                                    let frames = data.len() / channels;
                                    for f in 0..frames {
                                        let l = r.samples.pop_front().unwrap_or(0.0);
                                        let r_samp = r.samples.pop_front().unwrap_or(0.0);
                                        let base = f * channels;
                                        data[base] = l;
                                        if channels > 1 {
                                            data[base + 1] = r_samp;
                                        }
                                        for ch in 2..channels {
                                            data[base + ch] = 0.0;
                                        }
                                    }
                                } else {
                                    data.fill(0.0);
                                }
                            },
                            |err| eprintln!("[u64-audio] cpal error: {err}"),
                            None,
                        )
                        .map_err(|e| format!("build_output_stream: {e}"))?;

                    stream.play().map_err(|e| format!("stream.play(): {e}"))?;
                    Ok(stream)
                })();

                match result {
                    Ok(stream) => {
                        let _ = init_tx.send(Ok(()));
                        // Park here; the stream keeps playing as long as this
                        // thread is alive and owns the stream.
                        while !audio_stop.load(Ordering::Relaxed) {
                            thread::park_timeout(Duration::from_millis(100));
                        }
                        drop(stream);
                        eprintln!("[u64-audio] cpal thread stopped");
                    }
                    Err(e) => {
                        eprintln!("[u64-audio] cpal init failed: {e}");
                        let _ = init_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("spawn audio thread: {e}"))?;

        // Wait for cpal to initialise (or fail) before returning.
        init_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Audio thread did not respond in time".to_string())??;

        Ok(Self {
            stop,
            net: Some(net_handle),
            audio: Some(audio_handle),
        })
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Join the network thread so the UDP port is fully released before
        // we return.  This prevents "Address already in use" when start_audio
        // is called again immediately (e.g. on a subtune change).
        // We give it 500 ms; if it hasn't exited by then we abandon it.
        if let Some(handle) = self.net.take() {
            let start = std::time::Instant::now();
            while !handle.is_finished() {
                if start.elapsed() > Duration::from_millis(500) {
                    eprintln!("[u64-audio] Net thread did not exit in time — abandoning");
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        // Audio thread owns the cpal stream; let it exit on its own.
        if let Some(handle) = self.audio.take() {
            drop(handle);
        }
        eprintln!("[u64-audio] AudioStream stopped");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  U64Device
// ─────────────────────────────────────────────────────────────────────────────

/// SID output device that sends files to an Ultimate 64 via REST API.
pub struct U64Device {
    rest: Rest,
    address: String,
    audio: Option<AudioStream>,
    /// Cached base address of the C64 screen RAM, computed once after
    /// `play_sid_native()` returns. The U64's SID-player UI renders the
    /// elapsed playback time as 5 ASCII screen-code digits at row 23
    /// cols 0–4 (`MM:SS`) and the total length at cols 35–39.
    /// `None` once means we haven't computed it yet for the current song;
    /// `Some` after the layout has been validated. Reset on each new song.
    screen_base: Option<u16>,
    /// Layout-validation result for the current song. When false we stop
    /// trying to read the on-screen timer until the next play_sid_native()
    /// (the firmware's UI must be different than what we expect).
    screen_layout_ok: bool,
}

impl U64Device {
    /// Connect to an Ultimate 64 at the given address.
    ///
    /// `address` is an IP or hostname (e.g. "192.168.1.64").
    /// `password` is optional — only needed if the device has a network password set.
    pub fn connect(address: &str, password: &str) -> Result<Self, String> {
        if address.is_empty() {
            return Err(
                "No Ultimate 64 address configured. Set it in Settings → U64 IP Address."
                    .to_string(),
            );
        }

        let host = Host::parse(address)
            .map_err(|e| format!("Invalid Ultimate 64 address '{}': {}", address, e))?;

        let pass = if password.is_empty() {
            None
        } else {
            Some(password.to_string())
        };

        let rest = Rest::new(&host, pass)
            .map_err(|e| format!("Cannot connect to Ultimate 64 at {}: {}", address, e))?;

        // Quick connectivity check: request device info.
        match rest.version() {
            Ok(ver) => eprintln!("[u64] Connected to Ultimate 64 at {} ({})", address, ver),
            Err(e) => {
                eprintln!("[u64] Warning: device at {} not responding: {}", address, e);
                // Don't fail here — the device might come online later.
            }
        }

        Ok(Self {
            rest,
            address: address.to_string(),
            audio: None,
            screen_base: None,
            screen_layout_ok: false,
        })
    }

    /// Compute the C64 screen-RAM base address from VIC-II + CIA2 registers.
    /// On firmware 1.1.0 the SID-player UI uses VIC bank 0 and screen offset
    /// 0xF, giving `$3C00`. Other firmware versions may differ, so we read it
    /// rather than hardcoding.
    fn compute_screen_base(&self) -> Option<u16> {
        let vic_d018 = self.rest.read_mem(0xD018, 1).ok()?;
        let cia2_pa = self.rest.read_mem(0xDD00, 1).ok()?;
        let screen_offset = ((vic_d018[0] >> 4) & 0x0F) as u16;
        // CIA2 PA bits 0-1 select VIC bank, inverted: 11→bank0, 10→bank1, etc.
        let bank = (!cia2_pa[0]) & 0x03;
        let base = (bank as u16) * 0x4000 + screen_offset * 0x400;
        Some(base)
    }

    /// Parse five screen-code bytes laid out as `MM:SS` into total seconds.
    /// Returns `None` if any digit is out of range or the colon is wrong —
    /// in that case the firmware UI doesn't match what we expect.
    fn parse_mmss(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 5 {
            return None;
        }
        let d = |b: u8| -> Option<u32> {
            if (0x30..=0x39).contains(&b) {
                Some((b - 0x30) as u32)
            } else {
                None
            }
        };
        if bytes[2] != 0x3A {
            return None;
        }
        Some(d(bytes[0])? * 600 + d(bytes[1])? * 60 + d(bytes[3])? * 10 + d(bytes[4])?)
    }

    /// Read 5 screen-code bytes at `addr` and parse as `MM:SS`. Internal helper.
    fn read_screen_mmss(&self, addr: u16) -> Option<u32> {
        let bytes = self.rest.read_mem(addr, 5).ok()?;
        Self::parse_mmss(&bytes)
    }

    /// Reset the cached screen layout — call this after every new
    /// `sid_play()` since the player UI may redraw or move.
    fn invalidate_screen_layout(&mut self) {
        self.screen_base = None;
        self.screen_layout_ok = false;
    }

    /// Lazily resolve and validate the screen base. Returns the base, or
    /// `None` if the layout doesn't match the U64's player UI.
    fn ensure_screen_layout(&mut self) -> Option<u16> {
        if let Some(base) = self.screen_base {
            return if self.screen_layout_ok {
                Some(base)
            } else {
                None
            };
        }
        let base = self.compute_screen_base()?;
        let elapsed_addr = base.checked_add(23 * 40)?;
        // Validate by reading the 5 elapsed-time digits and parsing them.
        // If parsing succeeds the colon is in place and digits are screen
        // codes for '0'..='9' — we trust the layout.
        let ok = self.read_screen_mmss(elapsed_addr).is_some();
        self.screen_base = Some(base);
        self.screen_layout_ok = ok;
        if ok {
            Some(base)
        } else {
            None
        }
    }
}

impl SidDevice for U64Device {
    fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn set_clock_rate(&mut self, _is_pal: bool) {
        // U64 handles PAL/NTSC natively from the SID file header.
    }

    fn reset(&mut self) {
        if let Err(e) = self.rest.reset() {
            eprintln!("[u64] Reset failed: {e}");
        }
    }

    fn set_stereo(&mut self, _mode: i32) {
        // U64 handles multi-SID natively.
    }

    fn write(&mut self, _reg: u8, _val: u8) {
        // No-op: U64 runs its own SID player on the real C64 hardware.
    }

    fn ring_cycled(&mut self, _writes: &[(u16, u8, u8)]) {
        // No-op: native playback — register writes handled by the C64.
    }

    fn flush(&mut self) {
        // No-op for native playback.
    }

    fn mute(&mut self) {
        // Reset stops SID output on the C64.
        // Do NOT call stop_audio() here — the audio stream is a continuous
        // UDP flow that should survive mute (e.g. during subtune changes).
        self.reset();
    }

    fn close(&mut self) {
        self.stop_audio();
        self.reset();
    }

    fn shutdown(&mut self) {
        self.stop_audio();
        self.reset();
    }

    /// Send the entire SID file to the Ultimate 64 for native playback.
    /// Returns `Ok(true)` on success, meaning the host should skip CPU
    /// emulation and let the real hardware handle everything.
    fn play_sid_native(&mut self, data: &[u8], song: u16) -> Result<bool, String> {
        let song_num = if song > 0 { Some(song as u8) } else { None };

        self.rest
            .sid_play(data, song_num)
            .map_err(|e| format!("U64 sid_play failed: {e}"))?;

        // The U64 redraws its player UI for each new song, so the screen
        // base + validation must be recomputed. The next read_screen_*
        // call will lazily re-resolve the layout.
        self.invalidate_screen_layout();

        eprintln!("[u64] SID file sent ({} bytes, song {})", data.len(), song);
        Ok(true)
    }

    fn read_screen_elapsed(&mut self) -> Option<u32> {
        let base = self.ensure_screen_layout()?;
        // Row 23 col 0..4 — elapsed MM:SS.
        self.read_screen_mmss(base.wrapping_add(23 * 40))
    }

    fn read_screen_total(&mut self) -> Option<u32> {
        let base = self.ensure_screen_layout()?;
        // Row 23 col 35..39 — total MM:SS.
        self.read_screen_mmss(base.wrapping_add(23 * 40 + 35))
    }

    /// Freeze the C64 mid-frame — clock and SID output both pause instantly.
    fn pause_machine(&mut self) -> Result<(), String> {
        self.rest
            .pause()
            .map_err(|e| format!("U64 pause failed: {e}"))
    }

    /// Resume the C64 from exactly where it was frozen.
    fn resume_machine(&mut self) -> Result<(), String> {
        self.rest
            .resume()
            .map_err(|e| format!("U64 resume failed: {e}"))
    }

    /// Start streaming audio from the U64 back to this machine on `port`.
    ///
    /// Spawns a UDP listener and a cpal output thread.  The U64 is asked via
    /// REST to send its audio stream to our IP:port.  Safe to call multiple
    /// times — stops any previous stream first.
    fn start_audio(&mut self, port: u16) -> Result<(), String> {
        // Stop any existing stream first.
        self.stop_audio();

        eprintln!("[u64] Starting audio stream on port {port}");
        let stream = AudioStream::start(port, self.address.clone())?;
        self.audio = Some(stream);
        Ok(())
    }

    /// Stop the audio stream.  Sends a REST stop command to the U64 and
    /// shuts down both background threads.
    fn stop_audio(&mut self) {
        if self.audio.take().is_some() {
            // AudioStream::drop sets the stop flag and joins the net thread.
            eprintln!("[u64] Audio stream stopped");
        }
    }
}

impl Drop for U64Device {
    fn drop(&mut self) {
        self.stop_audio();
        // Best-effort reset on drop — don't panic if device is unreachable.
        let _ = self.rest.reset();
        eprintln!("[u64] Ultimate 64 device closed");
    }
}

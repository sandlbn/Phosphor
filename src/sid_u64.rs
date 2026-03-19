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
    _net: thread::JoinHandle<()>,
    _audio: thread::JoinHandle<()>,
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

                // Send REST stop command
                let stop_path = "/v1/streams/audio:stop";
                let stop_req  = format!(
                    "PUT {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    stop_path, net_addr,
                );
                if let Ok(mut tcp) = std::net::TcpStream::connect_timeout(
                    &format!("{net_addr}:80").parse().unwrap_or_else(|_| "0.0.0.0:80".parse().unwrap()),
                    Duration::from_secs(2),
                ) {
                    use std::io::Write;
                    let _ = tcp.write_all(stop_req.as_bytes());
                    eprintln!("[u64-audio] REST stop sent to {net_addr}");
                }

                eprintln!("[u64-audio] Network thread stopped (gaps: {})",
                    net_ring.lock().map(|r| r.gaps).unwrap_or(0));
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
            _net: net_handle,
            _audio: audio_handle,
        })
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Threads are background; we don't join them — they will exit on their
        // own once they see the stop flag, avoiding blocking the player thread.
        eprintln!("[u64-audio] AudioStream dropped, threads will exit");
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
        })
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
        self.stop_audio();
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

        eprintln!("[u64] SID file sent ({} bytes, song {})", data.len(), song);
        Ok(true)
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
            eprintln!("[u64] Audio stream stopped");
            // AudioStream::drop fires automatically, sets the stop flag.
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

// Software SID emulation output using resid-rs + cpal audio.
//
// Key design decisions:
//   - Query cpal for the device's ACTUAL sample rate
//   - Tell resid to generate at that rate via set_sampling_parameters()
//   - ring_cycled() clocks SID between writes (cycle-accurate intra-frame)
//   - flush() clocks remaining frame cycles and resets frame counter
//   - Audio thread owns the !Send cpal::Stream on a dedicated thread
//
// Reference usage from resid-rs README:
//   while delta > 0 {
//       let (samples, next_delta) = resid.sample(delta, &mut buffer[..], 1);
//       for i in 0..samples { output.write(buffer[i]); }
//       delta = next_delta;
//   }

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use resid::{ChipModel, SamplingMethod, Sid};

use crate::sid_device::SidDevice;

// ─────────────────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────────────────

const PAL_CLOCK: u32 = 985_248;
const NTSC_CLOCK: u32 = 1_022_727;
const PAL_CYCLES_PER_FRAME: u32 = 19_705;
const NTSC_CYCLES_PER_FRAME: u32 = 17_045;

/// Number of SID registers per chip (0x00-0x1F).
const SID_REGS: u8 = 0x20;

/// Max ring buffer capacity in stereo pairs.
/// ~170ms at 48kHz - enough to absorb jitter.
const MAX_BUFFER_SAMPLES: usize = 8192;

/// Scratch buffer for resid sample() output.
const SCRATCH_SIZE: usize = 2048;

// ─────────────────────────────────────────────────────────────────────────────
//  Send wrapper for resid::Sid  (Sid is !Send due to internal Rc)
// ─────────────────────────────────────────────────────────────────────────────

struct SendSid(Sid);
unsafe impl Send for SendSid {}

impl SendSid {
    fn new(model: ChipModel) -> Self {
        Self(Sid::new(model))
    }
    fn inner(&mut self) -> &mut Sid {
        &mut self.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared audio ring buffer  (player pushes, cpal callback pops)
// ─────────────────────────────────────────────────────────────────────────────

type AudioBuffer = Arc<Mutex<VecDeque<(i16, i16)>>>;

fn new_audio_buffer() -> AudioBuffer {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_BUFFER_SAMPLES)))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Audio thread  (owns the !Send cpal::Stream)
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a dedicated thread for cpal audio output.
/// Returns the device's actual sample rate on success.
fn spawn_audio_thread(audio_buf: AudioBuffer, shutdown: Arc<AtomicBool>) -> Result<u32, String> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<Result<u32, String>>(1);

    thread::Builder::new()
        .name("sid-audio".into())
        .spawn(move || {
            let result = (|| -> Result<(cpal::Stream, u32), String> {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| "No audio output device found".to_string())?;

                let dev_name = device.name().unwrap_or_else(|_| "unknown".into());

                // Query the device's preferred config to get the REAL sample rate.
                let default_config = device
                    .default_output_config()
                    .map_err(|e| format!("No default output config: {e}"))?;

                let actual_rate = default_config.sample_rate().0;
                eprintln!(
                    "[emulated] Audio device: '{}', native rate: {}Hz",
                    dev_name, actual_rate,
                );

                // Build stream at the device's native rate to avoid resampling.
                let config = cpal::StreamConfig {
                    channels: 2,
                    sample_rate: cpal::SampleRate(actual_rate),
                    buffer_size: cpal::BufferSize::Default,
                };

                let buf = audio_buf;

                let stream = device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            let mut ring = buf.lock().unwrap();
                            // data is interleaved [L, R, L, R, ...]
                            let frames = data.len() / 2;
                            for f in 0..frames {
                                let idx = f * 2;
                                if let Some((l, r)) = ring.pop_front() {
                                    data[idx] = l as f32 / 32768.0;
                                    data[idx + 1] = r as f32 / 32768.0;
                                } else {
                                    // Underrun: silence
                                    data[idx] = 0.0;
                                    data[idx + 1] = 0.0;
                                }
                            }
                        },
                        move |err| {
                            eprintln!("[emulated] Audio error: {err}");
                        },
                        None,
                    )
                    .map_err(|e| format!("build_output_stream failed: {e}"))?;

                stream
                    .play()
                    .map_err(|e| format!("stream.play() failed: {e}"))?;

                Ok((stream, actual_rate))
            })();

            match result {
                Ok((stream, rate)) => {
                    let _ = result_tx.send(Ok(rate));
                    // Park this thread: it owns the stream.
                    while !shutdown.load(Ordering::Relaxed) {
                        thread::park_timeout(std::time::Duration::from_millis(100));
                    }
                    drop(stream);
                    eprintln!("[emulated] Audio thread exiting");
                }
                Err(e) => {
                    let _ = result_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| format!("spawn audio thread: {e}"))?;

    result_rx
        .recv()
        .map_err(|_| "Audio thread died before reporting status".to_string())?
}

// ─────────────────────────────────────────────────────────────────────────────
//  EmulatedDevice
// ─────────────────────────────────────────────────────────────────────────────

pub struct EmulatedDevice {
    sid1: SendSid,
    sid2: Option<SendSid>,
    sid3: Option<SendSid>,

    clock_freq: u32,
    sample_rate: u32,
    chip_model: ChipModel,

    cycles_per_frame: u32,

    /// Total cycles clocked so far in the current frame (reset by flush).
    cycles_this_frame: u32,

    audio_buf: AudioBuffer,
    audio_shutdown: Arc<AtomicBool>,

    /// Diagnostic frame counter.
    frame_counter: u64,
}

impl EmulatedDevice {
    pub fn open() -> Result<Self, String> {
        let audio_buf = new_audio_buffer();
        let audio_shutdown = Arc::new(AtomicBool::new(false));

        // Spawn audio thread: returns the device's actual sample rate.
        let sample_rate = spawn_audio_thread(audio_buf.clone(), audio_shutdown.clone())?;

        let chip_model = ChipModel::Mos6581;
        let clock_freq = PAL_CLOCK;

        let mut sid1 = SendSid::new(chip_model);
        sid1.inner()
            .set_sampling_parameters(SamplingMethod::Fast, clock_freq, sample_rate);

        eprintln!(
            "[emulated] SID opened: MOS6581, clock={}Hz, output={}Hz",
            clock_freq, sample_rate,
        );

        Ok(Self {
            sid1,
            sid2: None,
            sid3: None,
            clock_freq,
            sample_rate,
            chip_model,
            cycles_per_frame: PAL_CYCLES_PER_FRAME,
            cycles_this_frame: 0,
            audio_buf,
            audio_shutdown,
            frame_counter: 0,
        })
    }

    // ── Internal helpers ─────────────────────────────────────────────────

    fn make_sid(&self) -> SendSid {
        let mut sid = SendSid::new(self.chip_model);
        sid.inner().set_sampling_parameters(
            SamplingMethod::Fast,
            self.clock_freq,
            self.sample_rate,
        );
        sid
    }

    /// Clock one SID by `delta` C64 cycles, collect generated samples.
    /// Follows the resid-rs README pattern exactly.
    fn clock_sid(sid: &mut SendSid, delta: u32, out: &mut Vec<i16>) {
        if delta == 0 {
            return;
        }
        let mut scratch = [0i16; SCRATCH_SIZE];
        let mut remaining = delta;
        let mut loops = 0u32;
        while remaining > 0 {
            let (n_samples, next_delta) = sid.inner().sample(remaining, &mut scratch, 1);
            if n_samples > 0 {
                out.extend_from_slice(&scratch[..n_samples]);
            }
            // Guard against infinite loop if sample() doesn't make progress.
            if next_delta >= remaining && n_samples == 0 {
                // Force clock to consume the cycles even without generating samples.
                sid.inner().clock_delta(remaining);
                break;
            }
            remaining = next_delta;
            loops += 1;
            if loops > 50000 {
                eprintln!("[emulated] WARNING: sample() loop exceeded 50k iterations, remaining={remaining}");
                break;
            }
        }
    }

    /// Route a register write to the correct SID chip.
    ///   0x00-0x1F -> SID1, 0x20-0x3F -> SID2, 0x40-0x5F -> SID3
    fn write_to_sid(&mut self, reg: u8, val: u8) {
        let chip = reg / SID_REGS;
        let local = reg % SID_REGS;
        match chip {
            0 => self.sid1.inner().write(local, val),
            1 => {
                if let Some(ref mut s) = self.sid2 {
                    s.inner().write(local, val);
                }
            }
            2 => {
                if let Some(ref mut s) = self.sid3 {
                    s.inner().write(local, val);
                }
            }
            _ => {}
        }
    }

    /// Clock all active SIDs forward by `delta` cycles, push stereo samples.
    fn clock_and_push(&mut self, delta: u32) {
        if delta == 0 {
            return;
        }

        let mut s1: Vec<i16> = Vec::with_capacity(1024);
        let mut s2: Vec<i16> = Vec::new();
        let mut s3: Vec<i16> = Vec::new();

        Self::clock_sid(&mut self.sid1, delta, &mut s1);
        if let Some(ref mut sid) = self.sid2 {
            Self::clock_sid(sid, delta, &mut s2);
        }
        if let Some(ref mut sid) = self.sid3 {
            Self::clock_sid(sid, delta, &mut s3);
        }

        if s1.is_empty() {
            return;
        }

        // Push to ring buffer as stereo pairs.
        let mut buf = self.audio_buf.lock().unwrap();
        let room = MAX_BUFFER_SAMPLES.saturating_sub(buf.len());
        let count = s1.len().min(room);

        for i in 0..count {
            let left = s1[i];
            let right = if !s2.is_empty() {
                *s2.get(i).unwrap_or(&0)
            } else {
                left // mono: mirror to both channels
            };

            if !s3.is_empty() {
                let centre = *s3.get(i).unwrap_or(&0) / 2;
                buf.push_back((left.saturating_add(centre), right.saturating_add(centre)));
            } else {
                buf.push_back((left, right));
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  SidDevice trait implementation
// ─────────────────────────────────────────────────────────────────────────────

impl SidDevice for EmulatedDevice {
    fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn set_clock_rate(&mut self, is_pal: bool) {
        self.clock_freq = if is_pal { PAL_CLOCK } else { NTSC_CLOCK };
        self.cycles_per_frame = if is_pal {
            PAL_CYCLES_PER_FRAME
        } else {
            NTSC_CYCLES_PER_FRAME
        };

        // Reconfigure all SIDs with the correct clock-to-sample ratio.
        self.sid1.inner().set_sampling_parameters(
            SamplingMethod::Fast,
            self.clock_freq,
            self.sample_rate,
        );
        if let Some(ref mut s) = self.sid2 {
            s.inner().set_sampling_parameters(
                SamplingMethod::Fast,
                self.clock_freq,
                self.sample_rate,
            );
        }
        if let Some(ref mut s) = self.sid3 {
            s.inner().set_sampling_parameters(
                SamplingMethod::Fast,
                self.clock_freq,
                self.sample_rate,
            );
        }

        eprintln!(
            "[emulated] Clock: {} {}Hz, {}/frame, output={}Hz",
            if is_pal { "PAL" } else { "NTSC" },
            self.clock_freq,
            self.cycles_per_frame,
            self.sample_rate,
        );
    }

    fn reset(&mut self) {
        self.sid1.inner().reset();
        if let Some(ref mut s) = self.sid2 {
            s.inner().reset();
        }
        if let Some(ref mut s) = self.sid3 {
            s.inner().reset();
        }
        self.cycles_this_frame = 0;
        if let Ok(mut buf) = self.audio_buf.lock() {
            buf.clear();
        }
    }

    fn set_stereo(&mut self, mode: i32) {
        if mode >= 1 && self.sid2.is_none() {
            self.sid2 = Some(self.make_sid());
            eprintln!("[emulated] SID2 enabled");
        }
        if mode >= 2 && self.sid3.is_none() {
            self.sid3 = Some(self.make_sid());
            eprintln!("[emulated] SID3 enabled");
        }
        if mode == 0 {
            self.sid2 = None;
            self.sid3 = None;
        }
    }

    fn write(&mut self, reg: u8, val: u8) {
        self.write_to_sid(reg, val);
    }

    /// Process cycle-stamped SID writes for one frame.
    ///
    /// Each entry: (delta_cycles, register, value).
    /// For each write: clock SID(s) by delta, then apply write.
    /// Caller MUST call flush() after this to generate remaining samples.
    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]) {
        if writes.is_empty() {
            return;
        }

        for &(delta, reg, val) in writes {
            let d = delta as u32;
            if d > 0 {
                self.clock_and_push(d);
                self.cycles_this_frame += d;
            }
            self.write_to_sid(reg, val);
        }
    }

    /// Generate audio for remaining frame cycles after ring_cycled().
    ///
    /// ring_cycled covers cycles 0..last_write_position.
    /// flush covers last_write_position..cycles_per_frame.
    fn flush(&mut self) {
        let remaining = self.cycles_per_frame.saturating_sub(self.cycles_this_frame);
        if remaining > 0 {
            self.clock_and_push(remaining);
        }

        // Periodic diagnostics (every 5 seconds at 50Hz).
        self.frame_counter += 1;
        if self.frame_counter % 250 == 1 {
            let buf_len = self.audio_buf.lock().map(|b| b.len()).unwrap_or(0);
            eprintln!(
                "[emulated] frame {}: wrote={} remain={} total={} cycles, buf={}",
                self.frame_counter,
                self.cycles_this_frame,
                remaining,
                self.cycles_this_frame + remaining,
                buf_len,
            );
        }

        self.cycles_this_frame = 0;
    }

    fn mute(&mut self) {
        self.sid1.inner().write(0x18, 0x00);
        if let Some(ref mut s) = self.sid2 {
            s.inner().write(0x18, 0x00);
        }
        if let Some(ref mut s) = self.sid3 {
            s.inner().write(0x18, 0x00);
        }
        self.cycles_this_frame = 0;
        if let Ok(mut buf) = self.audio_buf.lock() {
            buf.clear();
        }
    }

    fn close(&mut self) {
        self.mute();
        self.reset();
    }

    fn shutdown(&mut self) {
        self.close();
        self.audio_shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for EmulatedDevice {
    fn drop(&mut self) {
        self.mute();
        self.audio_shutdown.store(true, Ordering::Relaxed);
        eprintln!("[emulated] Software SID shut down");
    }
}

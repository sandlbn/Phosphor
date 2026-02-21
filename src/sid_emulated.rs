// Software SID emulation output using resid-rs + cpal audio.
//
//   Models the C64 mainboard RC output stage present on every real C64:
//     - Low-pass:  R=10kΩ, C=1000pF  → cutoff ~15.9kHz  (kills ultrasonic content)
//     - High-pass: R=10kΩ, C=10μF    → cutoff ~1.6Hz    (DC-blocker, kills clicks)
//   Each SID chip gets its own ExternalFilter instance, applied per-sample
//   before the stereo mix, matching how a real C64 sounds through the board.
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

//  Models the C64 mainboard two-stage RC network on the SID audio output line.
//  Every C64 has this circuit, so its frequency response is part of the authentic
//  sound — especially the LP roll-off that softens the harshness at the top end,
//  and the HP DC-blocker that prevents the audible "thump" when the SID is muted.
//
//  Circuit (from libresidfp docs):
//      SID out → 1kΩ → ─┬─ 1000pF → GND    (LP, ~15.9 kHz)
//                        └─ 10kΩ  → C 10μF → amp  (HP, ~1.59 Hz)
//
//  Fixed-point implementation directly matches libresidfp ExternalFilter.h:
//      Vi   = input << 11
//      dVlp = (w0lp_1_s7  * (Vi  - Vlp)) >> 7
//      dVhp = (w0hp_1_s17 * (Vlp - Vhp)) >> 17
//      Vlp += dVlp
//      Vhp += dVhp
//      output = (Vlp - Vhp) >> 11
// ─────────────────────────────────────────────────────────────────────────────

struct ExternalFilter {
    /// Low-pass integrator state (fixed-point ×2¹¹).
    vlp: i32,
    /// High-pass integrator state (fixed-point ×2¹¹).
    vhp: i32,
    /// LP coefficient: dt/(dt+RC_lp) × 2⁷, nearest int.
    /// PAL 985 248 Hz → 12   NTSC 1 022 727 Hz → 11
    w0lp_1_s7: i32,
    /// HP coefficient: dt/(dt+RC_hp) × 2¹⁷, nearest int.
    /// Both PAL and NTSC → 1  (RC_hp is so large the step is tiny)
    w0hp_1_s17: i32,
}

impl ExternalFilter {
    fn new() -> Self {
        Self {
            vlp: 0,
            vhp: 0,
            w0lp_1_s7: 0,
            w0hp_1_s17: 0,
        }
    }

    /// Compute and store filter coefficients for the given C64 clock frequency.
    ///
    /// Must be called after construction and again on every PAL↔NTSC switch.
    fn set_clock_frequency(&mut self, frequency: f64) {
        let dt = 1.0 / frequency;
        // LP: R = 10 kΩ, C = 1000 pF  →  RC = 10e-6 s
        let rc_lp: f64 = 10_000.0 * 1_000e-12;
        // HP: R = 10 kΩ, C = 10 μF   →  RC = 0.1 s
        let rc_hp: f64 = 10_000.0 * 10e-6;
        self.w0lp_1_s7 = ((dt / (dt + rc_lp)) * 128.0 + 0.5) as i32;
        self.w0hp_1_s17 = ((dt / (dt + rc_hp)) * 131_072.0 + 0.5) as i32;
    }

    fn reset(&mut self) {
        self.vlp = 0;
        self.vhp = 0;
    }

    /// Clock the filter by one sample.  Input/output are signed 16-bit audio.
    ///
    /// Matches ExternalFilter::clock() from libresidfp — one multiply-shift per stage.
    #[inline(always)]
    fn clock(&mut self, input: i16) -> i16 {
        let vi = (input as i32) << 11;
        let dvlp = (self.w0lp_1_s7 * (vi - self.vlp)) >> 7;
        let dvhp = (self.w0hp_1_s17 * (self.vlp - self.vhp)) >> 17;
        self.vlp += dvlp;
        self.vhp += dvhp;
        // Shift back to i16 range.  Clamp defends against cold-start transients.
        ((self.vlp - self.vhp) >> 11).clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }
}

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

    // ExternalFilter — one instance per SID chip, matching real C64 hardware.
    // Each filter models the RC output stage on the C64 mainboard (libresidfp port).
    ext1: ExternalFilter,
    ext2: ExternalFilter,
    ext3: ExternalFilter,

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

        // Build ExternalFilter for the initial clock rate.
        let mut ext1 = ExternalFilter::new();
        let mut ext2 = ExternalFilter::new();
        let mut ext3 = ExternalFilter::new();
        ext1.set_clock_frequency(clock_freq as f64);
        ext2.set_clock_frequency(clock_freq as f64);
        ext3.set_clock_frequency(clock_freq as f64);

        eprintln!(
            "[emulated] SID opened: MOS6581, clock={}Hz, output={}Hz, ExternalFilter=ON",
            clock_freq, sample_rate,
        );

        Ok(Self {
            sid1,
            sid2: None,
            sid3: None,
            ext1,
            ext2,
            ext3,
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
                sid.inner().clock_delta(remaining);
                break;
            }
            remaining = next_delta;
            loops += 1;
            if loops > 50000 {
                eprintln!(
                    "[emulated] WARNING: sample() loop exceeded 50k iterations, remaining={remaining}"
                );
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

    /// Clock all active SIDs forward by `delta` cycles, apply ExternalFilter
    /// per sample, then push stereo pairs to the audio ring buffer.
    ///
    /// ExternalFilter is applied to each SID's raw samples before mixing,
    /// modelling the C64 mainboard RC output stage (LP ~15.9kHz + HP ~1.6Hz).
    /// The filter state is per-SID so stereo separation is preserved.
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

        // Apply ExternalFilter to each SID's samples before mixing.
        // Done here (before locking audio_buf) to keep the borrow checker happy.
        //
        // Each ExternalFilter::clock() call is 4 multiplies + 4 adds — negligible cost.
        // The LP stage rolls off content above ~15.9kHz.
        // The HP stage removes DC, eliminating the "thump" on SID mute/silence.
        let filtered1: Vec<i16> = s1.iter().map(|&s| self.ext1.clock(s)).collect();
        let filtered2: Vec<i16> = s2.iter().map(|&s| self.ext2.clock(s)).collect();
        let filtered3: Vec<i16> = s3.iter().map(|&s| self.ext3.clock(s)).collect();

        // Push to ring buffer as stereo pairs.
        let mut buf = self.audio_buf.lock().unwrap();
        let room = MAX_BUFFER_SAMPLES.saturating_sub(buf.len());
        let count = filtered1.len().min(room);

        for i in 0..count {
            let left = filtered1[i];
            let right = if !filtered2.is_empty() {
                *filtered2.get(i).unwrap_or(&0)
            } else {
                left // mono: mirror SID1 (already filtered) to right channel
            };

            if !filtered3.is_empty() {
                // SID3 centre-mixed equally into both channels at half volume.
                let centre = *filtered3.get(i).unwrap_or(&0) / 2;
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

        // Update ExternalFilter coefficients to match the new clock frequency.
        // Cutoff frequencies are physical constants (RC values), so the coefficients
        // change slightly between PAL (~985kHz) and NTSC (~1023kHz).
        let freq = self.clock_freq as f64;
        self.ext1.set_clock_frequency(freq);
        self.ext2.set_clock_frequency(freq);
        self.ext3.set_clock_frequency(freq);

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
        // Reset ExternalFilter state — prevents DC transient after reset.
        self.ext1.reset();
        self.ext2.reset();
        self.ext3.reset();

        self.cycles_this_frame = 0;
        if let Ok(mut buf) = self.audio_buf.lock() {
            buf.clear();
        }
    }

    fn set_stereo(&mut self, mode: i32) {
        if mode >= 1 && self.sid2.is_none() {
            self.sid2 = Some(self.make_sid());
            // ext2 already created and configured; just reset state.
            self.ext2.reset();
            eprintln!("[emulated] SID2 enabled");
        }
        if mode >= 2 && self.sid3.is_none() {
            self.sid3 = Some(self.make_sid());
            self.ext3.reset();
            eprintln!("[emulated] SID3 enabled");
        }
        if mode == 0 {
            self.sid2 = None;
            self.sid3 = None;
            self.ext2.reset();
            self.ext3.reset();
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
        // Reset filter state to avoid a DC-offset thump after mute.
        self.ext1.reset();
        self.ext2.reset();
        self.ext3.reset();

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

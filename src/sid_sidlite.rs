// SIDLite emulation output using libsidplayfp's SIDLite engine + cpal audio.
//
// Drop-in alternative to the resid-rs emulated engine.
// Uses the same ExternalFilter, audio thread, and ring buffer architecture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sidlite_sys::{ChipModel, Sid};

use crate::sid_device::SidDevice;

// ─────────────────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────────────────

const PAL_CLOCK: u32 = 985_248;
const NTSC_CLOCK: u32 = 1_022_727;
const PAL_CYCLES_PER_FRAME: u32 = 19_656;
const NTSC_CYCLES_PER_FRAME: u32 = 17_095;

const SID_REGS: u8 = 0x20;
const MAX_BUFFER_SAMPLES: usize = 8192;
const SCRATCH_SIZE: usize = 2048;

// ─────────────────────────────────────────────────────────────────────────────
//  ExternalFilter — C64 mainboard RC output stage (same as sid_emulated.rs)
// ─────────────────────────────────────────────────────────────────────────────

struct ExternalFilter {
    vlp: i32,
    vhp: i32,
    w0lp_1_s7: i32,
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

    fn set_clock_frequency(&mut self, frequency: f64) {
        let dt = 1.0 / frequency;
        let rc_lp: f64 = 10_000.0 * 1_000e-12;
        let rc_hp: f64 = 10_000.0 * 10e-6;
        self.w0lp_1_s7 = ((dt / (dt + rc_lp)) * 128.0 + 0.5) as i32;
        self.w0hp_1_s17 = ((dt / (dt + rc_hp)) * 131_072.0 + 0.5) as i32;
    }

    fn reset(&mut self) {
        self.vlp = 0;
        self.vhp = 0;
    }

    #[inline(always)]
    fn clock(&mut self, input: i16) -> i16 {
        let vi = (input as i32) << 11;
        let dvlp = (self.w0lp_1_s7 * (vi - self.vlp)) >> 7;
        let dvhp = (self.w0hp_1_s17 * (self.vlp - self.vhp)) >> 17;
        self.vlp += dvlp;
        self.vhp += dvhp;
        ((self.vlp - self.vhp) >> 11).clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared audio ring buffer
// ─────────────────────────────────────────────────────────────────────────────

type AudioBuffer = Arc<Mutex<VecDeque<(i16, i16)>>>;

fn new_audio_buffer() -> AudioBuffer {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_BUFFER_SAMPLES)))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Audio thread (owns the !Send cpal::Stream)
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_audio_thread(audio_buf: AudioBuffer, shutdown: Arc<AtomicBool>) -> Result<u32, String> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<Result<u32, String>>(1);

    thread::Builder::new()
        .name("sidlite-audio".into())
        .spawn(move || {
            let result = (|| -> Result<(cpal::Stream, u32), String> {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| "No audio output device found".to_string())?;

                let dev_name = device.name().unwrap_or_else(|_| "unknown".into());
                let default_config = device
                    .default_output_config()
                    .map_err(|e| format!("No default output config: {e}"))?;

                let actual_rate = default_config.sample_rate().0;
                eprintln!(
                    "[sidlite] Audio device: '{}', native rate: {}Hz",
                    dev_name, actual_rate,
                );

                let config = cpal::StreamConfig {
                    channels: 2,
                    sample_rate: cpal::SampleRate(actual_rate),
                    buffer_size: cpal::BufferSize::Default,
                };

                let buf = audio_buf;
                let fade_len = (actual_rate / 200).max(64) as usize;
                let mut fade_pos: usize = 0;
                let mut was_underrun = true;

                let stream = device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            let mut ring = buf.lock().unwrap();
                            let frames = data.len() / 2;
                            for f in 0..frames {
                                let idx = f * 2;
                                if let Some((l, r)) = ring.pop_front() {
                                    let mut lf = l as f32 / 32768.0;
                                    let mut rf = r as f32 / 32768.0;
                                    if was_underrun {
                                        was_underrun = false;
                                        fade_pos = 0;
                                    }
                                    if fade_pos < fade_len {
                                        let gain = fade_pos as f32 / fade_len as f32;
                                        lf *= gain;
                                        rf *= gain;
                                        fade_pos += 1;
                                    }
                                    data[idx] = lf;
                                    data[idx + 1] = rf;
                                } else {
                                    data[idx] = 0.0;
                                    data[idx + 1] = 0.0;
                                    was_underrun = true;
                                }
                            }
                        },
                        move |err| {
                            eprintln!("[sidlite] Audio error: {err}");
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
                    while !shutdown.load(Ordering::Relaxed) {
                        thread::park_timeout(std::time::Duration::from_millis(100));
                    }
                    drop(stream);
                    eprintln!("[sidlite] Audio thread exiting");
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
//  SidLiteDevice
// ─────────────────────────────────────────────────────────────────────────────

pub struct SidLiteDevice {
    sid1: Sid,
    sid2: Option<Sid>,
    sid3: Option<Sid>,
    sid4: Option<Sid>,

    ext1: ExternalFilter,
    ext2: ExternalFilter,
    ext3: ExternalFilter,
    ext4: ExternalFilter,

    clock_freq: u32,
    sample_rate: u32,
    chip_model: ChipModel,

    cycles_per_frame: u32,
    cycles_this_frame: u32,

    audio_buf: AudioBuffer,
    audio_shutdown: Arc<AtomicBool>,

    frame_counter: u64,
}

impl SidLiteDevice {
    pub fn open() -> Result<Self, String> {
        let audio_buf = new_audio_buffer();
        let audio_shutdown = Arc::new(AtomicBool::new(false));

        let sample_rate = spawn_audio_thread(audio_buf.clone(), audio_shutdown.clone())?;

        // SIDLite supports sample rates up to 48000.
        // If the device rate exceeds that, clamp to 48000.
        let effective_rate = sample_rate.min(48000) as u16;

        let chip_model = ChipModel::Mos6581;
        let clock_freq = PAL_CLOCK;

        let mut sid1 = Sid::new(chip_model);
        sid1.set_sampling_parameters(clock_freq, effective_rate);

        let mut ext1 = ExternalFilter::new();
        ext1.set_clock_frequency(clock_freq as f64);

        eprintln!(
            "[sidlite] SID opened: MOS6581, clock={}Hz, output={}Hz (device={}Hz), ExternalFilter=ON",
            clock_freq, effective_rate, sample_rate,
        );

        Ok(Self {
            sid1,
            sid2: None,
            sid3: None,
            sid4: None,
            ext1,
            ext2: ExternalFilter::new(),
            ext3: ExternalFilter::new(),
            ext4: ExternalFilter::new(),
            clock_freq,
            sample_rate: effective_rate as u32,
            chip_model,
            cycles_per_frame: PAL_CYCLES_PER_FRAME,
            cycles_this_frame: 0,
            audio_buf,
            audio_shutdown,
            frame_counter: 0,
        })
    }

    fn make_sid(&self) -> Sid {
        let mut sid = Sid::new(self.chip_model);
        sid.set_sampling_parameters(self.clock_freq, self.sample_rate as u16);
        sid
    }

    fn clock_sid(sid: &mut Sid, delta: u32, out: &mut Vec<i16>) {
        if delta == 0 {
            return;
        }
        let mut scratch = [0i16; SCRATCH_SIZE];
        let n = sid.clock(delta, &mut scratch);
        if n > 0 {
            out.extend_from_slice(&scratch[..n]);
        }
    }

    fn write_to_sid(&mut self, reg: u8, val: u8) {
        let chip = reg / SID_REGS;
        let local = reg % SID_REGS;
        match chip {
            0 => self.sid1.write(local, val),
            1 => {
                if let Some(ref mut s) = self.sid2 {
                    s.write(local, val);
                }
            }
            2 => {
                if let Some(ref mut s) = self.sid3 {
                    s.write(local, val);
                }
            }
            3 => {
                if let Some(ref mut s) = self.sid4 {
                    s.write(local, val);
                }
            }
            _ => {}
        }
    }

    fn clock_and_push(&mut self, delta: u32) {
        if delta == 0 {
            return;
        }

        let mut s1: Vec<i16> = Vec::with_capacity(1024);
        let mut s2: Vec<i16> = Vec::new();
        let mut s3: Vec<i16> = Vec::new();
        let mut s4: Vec<i16> = Vec::new();

        Self::clock_sid(&mut self.sid1, delta, &mut s1);
        if let Some(ref mut sid) = self.sid2 {
            Self::clock_sid(sid, delta, &mut s2);
        }
        if let Some(ref mut sid) = self.sid3 {
            Self::clock_sid(sid, delta, &mut s3);
        }
        if let Some(ref mut sid) = self.sid4 {
            Self::clock_sid(sid, delta, &mut s4);
        }

        if s1.is_empty() {
            return;
        }

        let filtered1: Vec<i16> = s1.iter().map(|&s| self.ext1.clock(s)).collect();
        let filtered2: Vec<i16> = s2.iter().map(|&s| self.ext2.clock(s)).collect();
        let filtered3: Vec<i16> = s3.iter().map(|&s| self.ext3.clock(s)).collect();
        let filtered4: Vec<i16> = s4.iter().map(|&s| self.ext4.clock(s)).collect();

        let mut buf = self.audio_buf.lock().unwrap();
        let room = MAX_BUFFER_SAMPLES.saturating_sub(buf.len());
        let count = filtered1.len().min(room);

        for i in 0..count {
            let left = filtered1[i];
            let right = if !filtered2.is_empty() {
                *filtered2.get(i).unwrap_or(&0)
            } else {
                left
            };

            let mut centre: i16 = 0;
            if !filtered3.is_empty() {
                centre = centre.saturating_add(*filtered3.get(i).unwrap_or(&0) / 2);
            }
            if !filtered4.is_empty() {
                centre = centre.saturating_add(*filtered4.get(i).unwrap_or(&0) / 2);
            }

            if centre != 0 {
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

impl SidDevice for SidLiteDevice {
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

        let rate = self.sample_rate as u16;
        self.sid1.set_sampling_parameters(self.clock_freq, rate);
        if let Some(ref mut s) = self.sid2 {
            s.set_sampling_parameters(self.clock_freq, rate);
        }
        if let Some(ref mut s) = self.sid3 {
            s.set_sampling_parameters(self.clock_freq, rate);
        }
        if let Some(ref mut s) = self.sid4 {
            s.set_sampling_parameters(self.clock_freq, rate);
        }

        let freq = self.clock_freq as f64;
        self.ext1.set_clock_frequency(freq);
        self.ext2.set_clock_frequency(freq);
        self.ext3.set_clock_frequency(freq);
        self.ext4.set_clock_frequency(freq);

        eprintln!(
            "[sidlite] Clock: {} {}Hz, {}/frame, output={}Hz",
            if is_pal { "PAL" } else { "NTSC" },
            self.clock_freq,
            self.cycles_per_frame,
            self.sample_rate,
        );
    }

    fn set_cycles_per_frame(&mut self, cycles: u32) {
        if cycles != self.cycles_per_frame {
            eprintln!(
                "[sidlite] cycles_per_frame: {} -> {}",
                self.cycles_per_frame, cycles,
            );
            self.cycles_per_frame = cycles;
        }
    }

    fn reset(&mut self) {
        self.sid1.reset();
        if let Some(ref mut s) = self.sid2 {
            s.reset();
        }
        if let Some(ref mut s) = self.sid3 {
            s.reset();
        }
        if let Some(ref mut s) = self.sid4 {
            s.reset();
        }
        self.ext1.reset();
        self.ext2.reset();
        self.ext3.reset();
        self.ext4.reset();

        self.cycles_this_frame = 0;
        if let Ok(mut buf) = self.audio_buf.lock() {
            buf.clear();
        }
    }

    fn set_stereo(&mut self, mode: i32) {
        if mode >= 1 && self.sid2.is_none() {
            self.sid2 = Some(self.make_sid());
            self.ext2.reset();
            eprintln!("[sidlite] SID2 enabled");
        }
        if mode >= 2 && self.sid3.is_none() {
            self.sid3 = Some(self.make_sid());
            self.ext3.reset();
            eprintln!("[sidlite] SID3 enabled");
        }
        if mode >= 3 && self.sid4.is_none() {
            self.sid4 = Some(self.make_sid());
            self.ext4.reset();
            eprintln!("[sidlite] SID4 enabled");
        }
        if mode == 0 {
            self.sid2 = None;
            self.sid3 = None;
            self.sid4 = None;
            self.ext2.reset();
            self.ext3.reset();
            self.ext4.reset();
        }
    }

    fn write(&mut self, reg: u8, val: u8) {
        self.write_to_sid(reg, val);
    }

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

    fn flush(&mut self) {
        let remaining = self.cycles_per_frame.saturating_sub(self.cycles_this_frame);
        if remaining > 0 {
            self.clock_and_push(remaining);
        }

        self.frame_counter += 1;
        if self.frame_counter % 250 == 1 {
            let buf_len = self.audio_buf.lock().map(|b| b.len()).unwrap_or(0);
            eprintln!(
                "[sidlite] frame {}: wrote={} remain={} total={} cycles, buf={}",
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
        self.sid1.write(0x18, 0x00);
        if let Some(ref mut s) = self.sid2 {
            s.write(0x18, 0x00);
        }
        if let Some(ref mut s) = self.sid3 {
            s.write(0x18, 0x00);
        }
        if let Some(ref mut s) = self.sid4 {
            s.write(0x18, 0x00);
        }
        self.ext1.reset();
        self.ext2.reset();
        self.ext3.reset();
        self.ext4.reset();

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

impl Drop for SidLiteDevice {
    fn drop(&mut self) {
        self.mute();
        self.audio_shutdown.store(true, Ordering::Relaxed);
        eprintln!("[sidlite] SIDLite engine shut down");
    }
}

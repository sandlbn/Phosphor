//! Live lossless FLAC for `/api/stream.flac` (Cast / DLNA "lossless").
//!
//! An endless FLAC stream: one `fLaC` + STREAMINFO header (total length
//! and frame sizes unknown = 0, as the spec allows for streams), then a
//! fixed-size frame every `BLOCK` samples. Each listener encodes its own
//! copy from the raw PCM tap; FLAC at this size is a few % of one core.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};

/// Samples per channel per frame (~85 ms at 48 kHz).
pub const BLOCK: usize = 4096;

pub struct LiveFlac {
    cfg: Verified<config::Encoder>,
    info: StreamInfo,
    fb: FrameBuf,
    frame_no: usize,
    /// Interleaved samples not yet forming a whole frame.
    pending: Vec<i32>,
    /// Stereo pairs accepted so far (for real-time pacing).
    pairs_in: u64,
}

fn err(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("flac: {e:?}"))
}

impl LiveFlac {
    pub fn new(sample_rate: u32) -> io::Result<Self> {
        let mut cfg = config::Encoder::default();
        cfg.block_size = BLOCK;
        let cfg = cfg.into_verified().map_err(|(_, e)| err(e))?;
        let mut info = StreamInfo::new(sample_rate as usize, 2, 16).map_err(err)?;
        info.set_block_sizes(BLOCK, BLOCK).map_err(err)?;
        Ok(Self {
            cfg,
            info,
            fb: FrameBuf::with_size(2, BLOCK).map_err(err)?,
            frame_no: 0,
            pending: Vec::with_capacity(BLOCK * 4),
            pairs_in: 0,
        })
    }

    /// `fLaC` marker + STREAMINFO.
    pub fn header(&self) -> io::Result<Vec<u8>> {
        let mut sink = ByteSink::new();
        Stream::with_stream_info(self.info.clone())
            .write(&mut sink)
            .map_err(err)?;
        Ok(sink.as_slice().to_vec())
    }

    /// Feed stereo pairs; returns the bytes of every frame completed.
    pub fn push(&mut self, pairs: &[(i16, i16)]) -> io::Result<Vec<u8>> {
        self.pairs_in += pairs.len() as u64;
        self.pending.extend(
            pairs
                .iter()
                .flat_map(|&(l, r)| [i32::from(l), i32::from(r)]),
        );
        let mut sink = ByteSink::new();
        while self.pending.len() >= 2 * BLOCK {
            self.fb
                .fill_interleaved(&self.pending[..2 * BLOCK])
                .map_err(err)?;
            let frame =
                flacenc::encode_fixed_size_frame(&self.cfg, &self.fb, self.frame_no, &self.info)
                    .map_err(err)?;
            frame.write(&mut sink).map_err(err)?;
            self.frame_no += 1;
            self.pending.drain(..2 * BLOCK);
        }
        Ok(sink.as_slice().to_vec())
    }
}

/// Serve the live FLAC stream to `w` until the client disconnects or
/// `keep_going` returns false (casting to that device was stopped).
pub fn serve(w: &mut dyn Write, keep_going: &dyn Fn() -> bool) -> io::Result<()> {
    let rate = crate::audio_stream::sample_rate().max(8_000);
    let rx = crate::audio_stream::subscribe_pcm();
    let mut enc = LiveFlac::new(rate)?;
    w.write_all(&enc.header()?)?;
    w.flush()?;

    let start = Instant::now();
    let mut last_audio = Instant::now();
    let silence = vec![(0i16, 0i16); BLOCK];
    while keep_going() {
        let bytes = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                last_audio = Instant::now();
                enc.push(&chunk)?
            }
            Err(RecvTimeoutError::Timeout) => {
                // Engine idle (paused, between tunes, hardware engine):
                // pad silence to real-time so the receiver doesn't stall
                // or give up on the stream.
                let due = (start.elapsed().as_secs_f64() * f64::from(rate)) as u64;
                if last_audio.elapsed() > Duration::from_millis(100) && enc.pairs_in < due {
                    let n = ((due - enc.pairs_in) as usize).min(BLOCK);
                    enc.push(&silence[..n])?
                } else {
                    Vec::new()
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if !bytes.is_empty() {
            w.write_all(&bytes)?;
            w.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_flac_with_fixed_block_size() {
        let enc = LiveFlac::new(48_000).unwrap();
        let h = enc.header().unwrap();
        assert_eq!(&h[..4], b"fLaC");
        // STREAMINFO: last-metadata flag + type 0, length 34.
        assert_eq!(h[4] & 0x7F, 0);
        assert_eq!(&h[5..8], &[0, 0, 34]);
        // min / max block size = 4096.
        assert_eq!(&h[8..12], &[0x10, 0x00, 0x10, 0x00]);
    }

    #[test]
    fn frames_appear_per_block() {
        let mut enc = LiveFlac::new(44_100).unwrap();
        assert!(enc.push(&vec![(1, -1); BLOCK - 1]).unwrap().is_empty());
        let f = enc.push(&[(1, -1); 2]).unwrap();
        // FLAC frame sync code 0xFFF8 (fixed block size).
        assert_eq!(&f[..2], &[0xFF, 0xF8]);
        assert_eq!(enc.pending.len(), 2);
    }

    /// Decode a live-style stream (no total length) with the reference
    /// `flac` tool when it's installed; bit-exact round trip of a tone.
    #[test]
    fn reference_decoder_accepts_stream() {
        let Ok(out) = std::process::Command::new("flac").arg("--version").output() else {
            eprintln!("flac CLI not installed; skipping");
            return;
        };
        if !out.status.success() {
            return;
        }
        let rate = 48_000u32;
        let mut enc = LiveFlac::new(rate).unwrap();
        let mut bytes = enc.header().unwrap();
        let tone: Vec<(i16, i16)> = (0..rate as usize * 2)
            .map(|i| {
                let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0)
                    as i16;
                (v, -v)
            })
            .collect();
        for c in tone.chunks(777) {
            bytes.extend(enc.push(c).unwrap());
        }
        let dir = std::env::temp_dir().join(format!("phosphor-flac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("live.flac");
        let wav = dir.join("live.wav");
        std::fs::write(&src, &bytes).unwrap();
        let st = std::process::Command::new("flac")
            .args(["-d", "-f", "-s", "-o"])
            .arg(&wav)
            .arg(&src)
            .status()
            .unwrap();
        assert!(st.success(), "flac rejected the stream");
        let pcm = std::fs::read(&wav).unwrap();
        let data = &pcm[pcm.len() - (enc.frame_no * BLOCK * 4)..];
        let decoded: Vec<(i16, i16)> = data
            .chunks(4)
            .map(|b| {
                (
                    i16::from_le_bytes([b[0], b[1]]),
                    i16::from_le_bytes([b[2], b[3]]),
                )
            })
            .collect();
        assert_eq!(decoded, tone[..decoded.len()], "lossless round trip");
        let _ = std::fs::remove_dir_all(dir);
    }
}

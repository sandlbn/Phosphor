//! Renders per-chip sample buffers through the pedalboard and mixes them
//! to stereo i16 pairs. Shared by the reSID and SIDLite engines.

use fundsp::prelude::Shared;

use super::blocks::{self, Effect};
use super::{LiveBoard, MASTER, SID_LANES};

/// ~10 ms bypass ramp; chain-swap crossfade length.
const BYPASS_RAMP_S: f32 = 0.01;
const SWAP_FADE_S: f32 = 0.03;

struct Slot {
    fx: Box<dyn Effect>,
    bypass: Shared,
    wet: f32,
}

#[derive(Default)]
struct Chain {
    lanes: [Vec<Slot>; SID_LANES + 1],
}

impl Chain {
    fn build(board: &LiveBoard, sr: f64) -> Self {
        let mut chain = Chain::default();
        for lane in 0..=SID_LANES {
            for b in &board.spec().lane(lane).blocks {
                let Some(h) = board.handles(b.id) else {
                    continue;
                };
                let wet = if h.bypass.value() > 0.5 { 0.0 } else { 1.0 };
                chain.lanes[lane].push(Slot {
                    fx: blocks::build(b.kind, &h.params, sr),
                    bypass: h.bypass.clone(),
                    wet,
                });
            }
        }
        chain
    }
}

pub struct LaneMixer {
    sr: f64,
    board: Option<LiveBoard>,
    /// `None` = neutral board → integer fast path (bit-identical to the
    /// historic mix).
    chain: Option<Chain>,
    /// Previous chain fading out after a topology change, with the number
    /// of samples left in the fade.
    old: Option<(Chain, usize)>,
    /// Last applied (left, right) lane-out gains, ramped per buffer.
    lane_out: [(f32, f32); SID_LANES + 1],
    lane_bufs: [(Vec<f32>, Vec<f32>); SID_LANES + 1],
    dry: (Vec<f32>, Vec<f32>),
}

impl LaneMixer {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sr: sample_rate,
            board: None,
            chain: None,
            old: None,
            lane_out: [(1.0, 1.0); SID_LANES + 1],
            lane_bufs: Default::default(),
            dry: Default::default(),
        }
    }

    /// Install a new board. Knob / bypass changes don't need this — only
    /// topology changes (blocks added, removed, reordered, preset load).
    pub fn set_board(&mut self, board: LiveBoard) {
        let new = (!board.spec().is_neutral()).then(|| Chain::build(&board, self.sr));
        let had_fx = self.chain.is_some();
        let prev = std::mem::replace(&mut self.chain, new);
        if had_fx || self.chain.is_some() {
            // Fade from the previous chain (or the dry mix) to the new one.
            let fade = (SWAP_FADE_S * self.sr as f32) as usize;
            self.old = Some((prev.unwrap_or_default(), fade));
        }
        self.board = Some(board);
    }

    fn fx_active(&self) -> bool {
        self.chain.is_some() || self.old.is_some()
    }

    /// Mix `n` samples from each present chip into `out`.
    /// `chips[0]` must be present; absent chips are `None`.
    pub fn mix(&mut self, chips: [Option<&[i16]>; SID_LANES], n: usize, out: &mut Vec<(i16, i16)>) {
        if !self.fx_active() {
            self.mix_classic(chips, n, out);
            if let Some(b) = &self.board {
                b.dsp_load.set(0.0);
            }
            return;
        }
        let t0 = std::time::Instant::now();
        self.mix_fx(chips, n, out);
        if let Some(b) = &self.board {
            // Processing time over audio duration, smoothed (~0.5 s).
            let ratio = t0.elapsed().as_secs_f32() * self.sr as f32 / n.max(1) as f32;
            let a = (n as f32 / (0.5 * self.sr as f32)).min(1.0);
            b.dsp_load
                .set(b.dsp_load.value() + (ratio - b.dsp_load.value()) * a);
        }
    }

    /// The historic integer mix: SID1 left (both sides when mono), SID2
    /// right, SID3/4 centre at half level, saturating.
    fn mix_classic(
        &mut self,
        chips: [Option<&[i16]>; SID_LANES],
        n: usize,
        out: &mut Vec<(i16, i16)>,
    ) {
        let [c1, c2, c3, c4] = chips;
        let c1 = c1.expect("SID1 buffer");
        for i in 0..n {
            let left = c1[i];
            let right = c2.map_or(left, |c| c[i]);
            let mut centre: i16 = 0;
            if let Some(c) = c3 {
                centre = centre.saturating_add(c[i] / 2);
            }
            if let Some(c) = c4 {
                centre = centre.saturating_add(c[i] / 2);
            }
            out.push(if centre != 0 {
                (left.saturating_add(centre), right.saturating_add(centre))
            } else {
                (left, right)
            });
        }
        if let Some(board) = &self.board {
            for (lane, c) in chips.iter().enumerate() {
                let peak = c.map_or(0.0, |c| peak_i16(&c[..n]));
                decay_meter(&board.meters[lane], peak, n, self.sr);
            }
            let pk = out[out.len() - n..]
                .iter()
                .map(|&(l, r)| (l as f32).abs().max((r as f32).abs()) / 32768.0)
                .fold(0.0, f32::max);
            decay_meter(&board.meters[MASTER], pk, n, self.sr);
        }
    }

    fn mix_fx(&mut self, chips: [Option<&[i16]>; SID_LANES], n: usize, out: &mut Vec<(i16, i16)>) {
        let board = self.board.clone().expect("fx path requires a board");
        let bypass_all = board.bypass_all_handle().value() > 0.5;
        let stereo_pair = chips[1].is_some();
        let sr = self.sr as f32;

        // Take the chain out so the helpers can borrow `self` mutably.
        let mut chain = self.chain.take().unwrap_or_default();
        let (mut ml, mut mr) =
            self.render(&mut chain, &board, chips, n, bypass_all, stereo_pair, true);
        self.chain = Some(chain);

        if let Some((mut old, left)) = self.old.take() {
            let (ol, or) = self.render(&mut old, &board, chips, n, bypass_all, stereo_pair, false);
            let total = (SWAP_FADE_S * sr) as usize;
            for i in 0..n {
                let t = (left.saturating_sub(i)) as f32 / total.max(1) as f32;
                ml[i] = ml[i] * (1.0 - t) + ol[i] * t;
                mr[i] = mr[i] * (1.0 - t) + or[i] * t;
            }
            let rem = left.saturating_sub(n);
            if rem > 0 {
                self.old = Some((old, rem));
            }
        }
        // The new chain was the neutral board and the fade finished →
        // return to the integer fast path.
        if self.old.is_none() && board.spec().is_neutral() {
            self.chain = None;
        }

        let pk = ml.iter().chain(&mr).fold(0.0f32, |a, &b| a.max(b.abs()));
        decay_meter(&board.meters[MASTER], pk, n, self.sr);
        out.extend(
            ml.iter()
                .zip(&mr)
                .map(|(&l, &r)| (to_i16(soft_clip(l)), to_i16(soft_clip(r)))),
        );
    }

    /// Run every lane of `chain` and the master; returns the stereo sum.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        chain: &mut Chain,
        board: &LiveBoard,
        chips: [Option<&[i16]>; SID_LANES],
        n: usize,
        bypass_all: bool,
        stereo_pair: bool,
        meter: bool,
    ) -> (Vec<f32>, Vec<f32>) {
        let sr = self.sr as f32;
        let mut sum_l = vec![0.0f32; n];
        let mut sum_r = vec![0.0f32; n];
        for (lane, chip) in chips.iter().enumerate() {
            let Some(c) = chip else { continue };
            let (l, r) = &mut self.lane_bufs[lane];
            l.clear();
            l.extend(c[..n].iter().map(|&s| s as f32 / 32768.0));
            r.clear();
            r.extend_from_slice(l);
            run_slots(&mut chain.lanes[lane], l, r, bypass_all, sr, &mut self.dry);
            let target = lane_gains(board, lane, bypass_all, stereo_pair);
            let from = self.lane_out[lane];
            if meter {
                self.lane_out[lane] = target;
            }
            let mut peak = 0.0f32;
            for i in 0..n {
                let t = (i + 1) as f32 / n as f32;
                let gl = from.0 + (target.0 - from.0) * t;
                let gr = from.1 + (target.1 - from.1) * t;
                let (a, b) = (l[i] * gl, r[i] * gr);
                peak = peak.max(a.abs()).max(b.abs());
                sum_l[i] += a;
                sum_r[i] += b;
            }
            if meter {
                decay_meter(&board.meters[lane], peak, n, self.sr);
            }
        }
        run_slots(
            &mut chain.lanes[MASTER],
            &mut sum_l,
            &mut sum_r,
            bypass_all,
            sr,
            &mut self.dry,
        );
        let target = lane_gains(board, MASTER, bypass_all, stereo_pair);
        let from = self.lane_out[MASTER];
        if meter {
            self.lane_out[MASTER] = target;
        }
        for i in 0..n {
            let t = (i + 1) as f32 / n as f32;
            sum_l[i] *= from.0 + (target.0 - from.0) * t;
            sum_r[i] *= from.1 + (target.1 - from.1) * t;
        }
        (sum_l, sum_r)
    }
}

/// Lane-out (left, right) gain: user gain × balance, or the classic
/// routing when pan is automatic / everything is bypassed.
fn lane_gains(board: &LiveBoard, lane: usize, bypass_all: bool, stereo_pair: bool) -> (f32, f32) {
    let pan = board.lane_pan(lane).value();
    if bypass_all || LiveBoard::pan_is_auto(pan) {
        let g = if bypass_all {
            1.0
        } else {
            board.lane_gain(lane).value()
        };
        let (l, r) = match lane {
            0 if stereo_pair => (1.0, 0.0),
            0 | MASTER => (1.0, 1.0),
            1 => (0.0, 1.0),
            _ => (0.5, 0.5),
        };
        return (l * g, r * g);
    }
    let g = board.lane_gain(lane).value();
    (g * (1.0 - pan).min(1.0), g * (1.0 + pan).min(1.0))
}

fn run_slots(
    slots: &mut [Slot],
    l: &mut [f32],
    r: &mut [f32],
    bypass_all: bool,
    sr: f32,
    dry: &mut (Vec<f32>, Vec<f32>),
) {
    let step = 1.0 / (BYPASS_RAMP_S * sr);
    for slot in slots {
        let target = if bypass_all || slot.bypass.value() > 0.5 {
            0.0
        } else {
            1.0
        };
        if slot.wet == 0.0 && target == 0.0 {
            continue;
        }
        if slot.wet == 0.0 {
            // Coming out of bypass: don't replay a stale tail.
            slot.fx.reset();
        }
        if slot.wet == 1.0 && target == 1.0 {
            slot.fx.process(l, r);
            continue;
        }
        dry.0.clear();
        dry.0.extend_from_slice(l);
        dry.1.clear();
        dry.1.extend_from_slice(r);
        slot.fx.process(l, r);
        for i in 0..l.len() {
            slot.wet = if target > slot.wet {
                (slot.wet + step).min(1.0)
            } else {
                (slot.wet - step).max(0.0)
            };
            l[i] = dry.0[i] + (l[i] - dry.0[i]) * slot.wet;
            r[i] = dry.1[i] + (r[i] - dry.1[i]) * slot.wet;
        }
    }
}

fn peak_i16(s: &[i16]) -> f32 {
    s.iter().map(|&x| (x as f32).abs()).fold(0.0, f32::max) / 32768.0
}

/// Peak-hold meter with ~300 ms fall.
fn decay_meter(m: &Shared, peak: f32, n: usize, sr: f64) {
    let decay = 0.5f32.powf(n as f32 / (0.3 * sr as f32));
    m.set(peak.max(m.value() * decay));
}

/// Transparent below -1 dBFS, smooth knee up to full scale.
#[inline]
fn soft_clip(x: f32) -> f32 {
    const K: f32 = 0.9;
    let a = x.abs();
    if a <= K {
        x
    } else {
        x.signum() * (K + (1.0 - K) * ((a - K) / (1.0 - K)).tanh())
    }
}

#[inline]
fn to_i16(x: f32) -> i16 {
    (x * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::BlockKind;

    fn noise(seed: u32, n: usize) -> Vec<i16> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as i16
            })
            .collect()
    }

    /// Reference copy of the pre-pedalboard mix loop.
    fn legacy(chips: [Option<&[i16]>; 4], n: usize) -> Vec<(i16, i16)> {
        (0..n)
            .map(|i| {
                let left = chips[0].unwrap()[i];
                let right = chips[1].map_or(left, |c| c[i]);
                let mut centre: i16 = 0;
                if let Some(c) = chips[2] {
                    centre = centre.saturating_add(c[i] / 2);
                }
                if let Some(c) = chips[3] {
                    centre = centre.saturating_add(c[i] / 2);
                }
                (left.saturating_add(centre), right.saturating_add(centre))
            })
            .collect()
    }

    #[test]
    fn neutral_board_is_bit_identical() {
        let (a, b, c, d) = (
            noise(1, 4096),
            noise(2, 4096),
            noise(3, 4096),
            noise(4, 4096),
        );
        let mut m = LaneMixer::new(48_000.0);
        m.set_board(LiveBoard::default());
        for chips in [
            [Some(&a[..]), None, None, None],
            [Some(&a[..]), Some(&b[..]), None, None],
            [Some(&a[..]), Some(&b[..]), Some(&c[..]), Some(&d[..])],
        ] {
            let mut out = Vec::new();
            m.mix(chips, 4096, &mut out);
            assert_eq!(out, legacy(chips, 4096));
        }
    }

    #[test]
    fn fx_board_changes_output_and_returns_to_exact_when_cleared() {
        let a = noise(7, 48_000);
        let mut m = LaneMixer::new(48_000.0);
        let mut board = LiveBoard::default();
        let id = board.add_block(0, BlockKind::Reverb);
        m.set_board(board.clone());
        let mut out = Vec::new();
        for chunk in a.chunks(512) {
            m.mix([Some(chunk), None, None, None], chunk.len(), &mut out);
        }
        assert_ne!(out, legacy([Some(&a[..]), None, None, None], a.len()));

        board.remove_block(id);
        m.set_board(board);
        // Run past the fade, then it must be exact again.
        let mut out = Vec::new();
        for chunk in a.chunks(512) {
            out.clear();
            m.mix([Some(chunk), None, None, None], chunk.len(), &mut out);
        }
        let last = a.chunks(512).last().unwrap();
        assert_eq!(out, legacy([Some(last), None, None, None], last.len()));
    }

    #[test]
    fn swap_crossfade_has_no_jump() {
        // A constant input through Gain must move smoothly when the chain
        // is swapped for one with a different gain.
        let dc = vec![8000i16; 4800];
        let mut m = LaneMixer::new(48_000.0);
        let mut board = LiveBoard::default();
        let g = board.add_block(0, BlockKind::Gain);
        board.set_param(g, 0, -12.0);
        m.set_board(board.clone());
        let mut out = Vec::new();
        for c in dc.chunks(480) {
            m.mix([Some(c), None, None, None], c.len(), &mut out);
        }
        board.remove_block(g);
        m.set_board(board);
        let start = out.len();
        for c in dc.chunks(480) {
            m.mix([Some(c), None, None, None], c.len(), &mut out);
        }
        let max_step = out[start - 1..]
            .windows(2)
            .map(|w| (w[1].0 as i32 - w[0].0 as i32).abs())
            .max()
            .unwrap();
        assert!(max_step < 200, "step {max_step}");
    }

    #[test]
    fn bypass_ramps_instead_of_clicking() {
        let dc = vec![8000i16; 480];
        let mut m = LaneMixer::new(48_000.0);
        let mut board = LiveBoard::default();
        let g = board.add_block(0, BlockKind::Gain);
        board.set_param(g, 0, -24.0);
        m.set_board(board.clone());
        let mut out = Vec::new();
        for _ in 0..10 {
            m.mix([Some(&dc), None, None, None], 480, &mut out);
        }
        board.set_bypass(g, true);
        let start = out.len();
        for _ in 0..10 {
            m.mix([Some(&dc), None, None, None], 480, &mut out);
        }
        let max_step = out[start - 1..]
            .windows(2)
            .map(|w| (w[1].0 as i32 - w[0].0 as i32).abs())
            .max()
            .unwrap();
        assert!(max_step < 200, "step {max_step}");
        assert!((out.last().unwrap().0 as i32 - 8000).abs() < 5);
    }
}

/// Benchmarks: `cargo test --release -- --ignored perf --nocapture`
#[cfg(test)]
mod perf {
    use super::*;
    use crate::dsp::BlockKind;

    #[test]
    #[ignore]
    fn worst_case_realtime_factor() {
        let sr = 48_000.0;
        let mut board = LiveBoard::default();
        for lane in 0..SID_LANES {
            for k in [
                BlockKind::Reverb,
                BlockKind::Chorus,
                BlockKind::Delay,
                BlockKind::Eq3,
            ] {
                board.add_block(lane, k);
            }
        }
        board.add_block(MASTER, BlockKind::Reverb);
        board.add_block(MASTER, BlockKind::Limiter);
        let mut m = LaneMixer::new(sr);
        m.set_board(board);
        let chip: Vec<i16> = (0..960).map(|i| ((i * 37) % 2000) as i16 - 1000).collect();
        let secs = 5;
        let t = std::time::Instant::now();
        let mut out = Vec::with_capacity(960);
        for _ in 0..(secs * 50) {
            out.clear();
            m.mix(
                [Some(&chip), Some(&chip), Some(&chip), Some(&chip)],
                960,
                &mut out,
            );
        }
        let el = t.elapsed().as_secs_f64();
        eprintln!(
            "worst case: {secs}s of audio in {el:.3}s → {:.1}% of one core",
            el / secs as f64 * 100.0
        );
    }

    /// Hall preset fed the way the engines feed it: many tiny calls.
    #[test]
    #[ignore]
    fn hall_small_chunks() {
        let sr = 48_000.0;
        let hall = crate::dsp::presets::factory()
            .into_iter()
            .find(|p| p.name == "Hall")
            .unwrap();
        for chunk in [960usize, 64, 8] {
            let mut m = LaneMixer::new(sr);
            m.set_board(LiveBoard::from_spec(hall.board.clone()));
            let chip: Vec<i16> = (0..chunk)
                .map(|i| ((i * 37) % 2000) as i16 - 1000)
                .collect();
            let total = 48_000 * 2;
            let t = std::time::Instant::now();
            let mut out = Vec::new();
            for _ in 0..total / chunk {
                out.clear();
                m.mix([Some(&chip), None, None, None], chunk, &mut out);
            }
            let el = t.elapsed().as_secs_f64();
            eprintln!(
                "chunk {chunk:4}: 2s audio in {el:.3}s ({:.1}% core)",
                el / 2.0 * 100.0
            );
        }
    }
}

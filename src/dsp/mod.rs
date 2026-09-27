//! Pedalboard: per-SID and master effect chains for the software engines.
//!
//! * [`PedalboardSpec`] is the serialisable description (what gets saved).
//! * [`LiveBoard`] pairs a spec with the `Shared` atomics that carry live
//!   knob / bypass values. The UI keeps one; the audio side gets a clone
//!   (the atomics are shared), so turning a knob never needs a message.
//! * [`LaneMixer`] renders the chip buffers through the chains and mixes.

pub mod blocks;
pub mod mixer;
pub mod presets;

use std::collections::HashMap;

use fundsp::prelude::Shared;
use serde::{Deserialize, Serialize};

pub use blocks::{BlockKind, Category, ParamDef};
pub use mixer::LaneMixer;

/// Number of SID lanes (max SIDs a tune can use).
pub const SID_LANES: usize = 4;
/// Index of the master lane in per-lane arrays (`meters`, etc.).
pub const MASTER: usize = SID_LANES;

/// Sentinel stored in a lane's pan atomic meaning "automatic routing"
/// (the classic SID1-left / SID2-right / SID3+4-centre layout).
const PAN_AUTO: f32 = 2.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockSpec {
    pub id: u64,
    pub kind: BlockKind,
    #[serde(default)]
    pub bypass: bool,
    /// One value per [`BlockKind::params`] entry. Missing / extra values
    /// are repaired on load (see [`BlockSpec::normalised`]).
    pub params: Vec<f32>,
}

impl BlockSpec {
    pub fn new(id: u64, kind: BlockKind) -> Self {
        Self {
            id,
            kind,
            bypass: false,
            params: kind.params().iter().map(|p| p.default).collect(),
        }
    }

    /// Pad / truncate / clamp params against the catalog so an older or
    /// hand-edited preset can't feed out-of-range values to the DSP.
    fn normalised(mut self) -> Self {
        let defs = self.kind.params();
        self.params.resize(defs.len(), 0.0);
        for (v, d) in self.params.iter_mut().zip(defs) {
            if !v.is_finite() {
                *v = d.default;
            }
            *v = v.clamp(d.min, d.max);
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneSpec {
    #[serde(default)]
    pub blocks: Vec<BlockSpec>,
    #[serde(default)]
    pub gain_db: f32,
    /// `None` = automatic routing. `Some(-1.0..=1.0)` = balance.
    #[serde(default)]
    pub pan: Option<f32>,
}

impl Default for LaneSpec {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            gain_db: 0.0,
            pan: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PedalboardSpec {
    pub lanes: [LaneSpec; SID_LANES],
    pub master: LaneSpec,
    #[serde(default)]
    pub bypass_all: bool,
}

impl PedalboardSpec {
    /// True when no lane would change the classic mix: no blocks, unity
    /// gain, automatic pan. The mixer then takes the bit-exact fast path.
    pub fn is_neutral(&self) -> bool {
        self.lanes
            .iter()
            .chain(std::iter::once(&self.master))
            .all(|l| l.blocks.is_empty() && l.gain_db == 0.0 && l.pan.is_none())
    }

    pub fn lane(&self, lane: usize) -> &LaneSpec {
        if lane == MASTER {
            &self.master
        } else {
            &self.lanes[lane]
        }
    }

    fn lane_mut(&mut self, lane: usize) -> &mut LaneSpec {
        if lane == MASTER {
            &mut self.master
        } else {
            &mut self.lanes[lane]
        }
    }

    fn max_id(&self) -> u64 {
        self.lanes
            .iter()
            .chain(std::iter::once(&self.master))
            .flat_map(|l| l.blocks.iter().map(|b| b.id))
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn normalised(mut self) -> Self {
        for lane in self
            .lanes
            .iter_mut()
            .chain(std::iter::once(&mut self.master))
        {
            lane.blocks = std::mem::take(&mut lane.blocks)
                .into_iter()
                .map(BlockSpec::normalised)
                .collect();
            lane.gain_db = if lane.gain_db.is_finite() {
                lane.gain_db.clamp(-24.0, 12.0)
            } else {
                0.0
            };
            lane.pan = lane
                .pan
                .filter(|p| p.is_finite())
                .map(|p| p.clamp(-1.0, 1.0));
        }
        self
    }
}

/// Live atomics for one block.
#[derive(Clone)]
pub struct BlockHandles {
    pub params: Vec<Shared>,
    pub bypass: Shared,
}

/// Board most recently sent to the player, re-applied whenever an engine
/// is (re)created (engine switch, USB replug, first Play).
static ACTIVE: std::sync::Mutex<Option<LiveBoard>> = std::sync::Mutex::new(None);

pub fn set_active(board: LiveBoard) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(board);
}

pub fn active() -> Option<LiveBoard> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Spec + the atomics the audio thread reads. Cheap to clone (Arcs).
#[derive(Clone)]
pub struct LiveBoard {
    spec: PedalboardSpec,
    blocks: HashMap<u64, BlockHandles>,
    lane_gain: [Shared; SID_LANES + 1],
    lane_pan: [Shared; SID_LANES + 1],
    bypass_all: Shared,
    /// Peak level per lane (0..1+), written by the mixer, read by the UI.
    pub meters: [Shared; SID_LANES + 1],
    /// Smoothed DSP cost as a fraction of real time (0.01 = 1% of a core).
    pub dsp_load: Shared,
    next_id: u64,
}

impl std::fmt::Debug for LiveBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBoard")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl Default for LiveBoard {
    fn default() -> Self {
        Self::from_spec(PedalboardSpec::default())
    }
}

fn db_to_amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

impl LiveBoard {
    pub fn from_spec(spec: PedalboardSpec) -> Self {
        let spec = spec.normalised();
        let mut board = Self {
            blocks: HashMap::new(),
            lane_gain: std::array::from_fn(|_| Shared::new(1.0)),
            lane_pan: std::array::from_fn(|_| Shared::new(PAN_AUTO)),
            bypass_all: Shared::new(0.0),
            meters: std::array::from_fn(|_| Shared::new(0.0)),
            dsp_load: Shared::new(0.0),
            next_id: spec.max_id() + 1,
            spec,
        };
        board.sync_all();
        board
    }

    /// Replace the whole spec (preset load), keeping meter handles so the
    /// UI doesn't need to re-subscribe.
    pub fn load(&mut self, spec: PedalboardSpec) {
        let (meters, load) = (self.meters.clone(), self.dsp_load.clone());
        *self = Self::from_spec(spec);
        self.meters = meters;
        self.dsp_load = load;
    }

    fn sync_all(&mut self) {
        self.blocks.clear();
        for lane in 0..=SID_LANES {
            let l = self.spec.lane(lane).clone();
            self.lane_gain[lane].set(db_to_amp(l.gain_db));
            self.lane_pan[lane].set(l.pan.unwrap_or(PAN_AUTO));
            for b in &l.blocks {
                self.blocks.insert(b.id, Self::handles_for(b));
            }
        }
        self.bypass_all
            .set(if self.spec.bypass_all { 1.0 } else { 0.0 });
    }

    fn handles_for(b: &BlockSpec) -> BlockHandles {
        BlockHandles {
            params: b.params.iter().map(|&v| Shared::new(v)).collect(),
            bypass: Shared::new(if b.bypass { 1.0 } else { 0.0 }),
        }
    }

    pub fn spec(&self) -> &PedalboardSpec {
        &self.spec
    }

    pub fn handles(&self, id: u64) -> Option<&BlockHandles> {
        self.blocks.get(&id)
    }

    pub(crate) fn lane_gain(&self, lane: usize) -> &Shared {
        &self.lane_gain[lane]
    }

    pub(crate) fn lane_pan(&self, lane: usize) -> &Shared {
        &self.lane_pan[lane]
    }

    pub(crate) fn bypass_all_handle(&self) -> &Shared {
        &self.bypass_all
    }

    pub(crate) fn pan_is_auto(v: f32) -> bool {
        v > 1.5
    }

    // ── Live edits (no rebuild needed) ──────────────────────────────────

    pub fn set_param(&mut self, id: u64, idx: usize, value: f32) {
        let Some(b) = self.find_mut(id) else { return };
        let Some(def) = b.kind.params().get(idx) else {
            return;
        };
        let v = value.clamp(def.min, def.max);
        b.params[idx] = v;
        if let Some(h) = self.blocks.get(&id).and_then(|h| h.params.get(idx)) {
            h.set(v);
        }
    }

    pub fn set_bypass(&mut self, id: u64, bypass: bool) {
        if let Some(b) = self.find_mut(id) {
            b.bypass = bypass;
        }
        if let Some(h) = self.blocks.get(&id) {
            h.bypass.set(if bypass { 1.0 } else { 0.0 });
        }
    }

    pub fn set_bypass_all(&mut self, bypass: bool) {
        self.spec.bypass_all = bypass;
        self.bypass_all.set(if bypass { 1.0 } else { 0.0 });
    }

    pub fn set_lane_gain_db(&mut self, lane: usize, db: f32) {
        let db = db.clamp(-24.0, 12.0);
        self.spec.lane_mut(lane).gain_db = db;
        self.lane_gain[lane].set(db_to_amp(db));
    }

    pub fn set_lane_pan(&mut self, lane: usize, pan: Option<f32>) {
        let pan = pan.map(|p| p.clamp(-1.0, 1.0));
        self.spec.lane_mut(lane).pan = pan;
        self.lane_pan[lane].set(pan.unwrap_or(PAN_AUTO));
    }

    // ── Topology edits (caller must resend the board to the engine) ─────

    /// Append a block to `lane`; returns its id.
    pub fn add_block(&mut self, lane: usize, kind: BlockKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let b = BlockSpec::new(id, kind);
        self.blocks.insert(id, Self::handles_for(&b));
        self.spec.lane_mut(lane).blocks.push(b);
        id
    }

    pub fn remove_block(&mut self, id: u64) {
        for lane in 0..=SID_LANES {
            self.spec.lane_mut(lane).blocks.retain(|b| b.id != id);
        }
        self.blocks.remove(&id);
    }

    /// Move a block one slot left (`-1`) or right (`+1`) within its lane.
    pub fn move_block(&mut self, id: u64, dir: isize) {
        for lane in 0..=SID_LANES {
            let blocks = &mut self.spec.lane_mut(lane).blocks;
            if let Some(i) = blocks.iter().position(|b| b.id == id) {
                let j = i as isize + dir;
                if j >= 0 && (j as usize) < blocks.len() {
                    blocks.swap(i, j as usize);
                }
                return;
            }
        }
    }

    /// Lane holding block `id` and its position.
    pub fn locate(&self, id: u64) -> Option<(usize, usize)> {
        (0..=SID_LANES).find_map(|lane| {
            self.spec
                .lane(lane)
                .blocks
                .iter()
                .position(|b| b.id == id)
                .map(|i| (lane, i))
        })
    }

    pub fn block(&self, id: u64) -> Option<&BlockSpec> {
        self.locate(id).map(|(l, i)| &self.spec.lane(l).blocks[i])
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut BlockSpec> {
        let (l, i) = self.locate(id)?;
        Some(&mut self.spec.lane_mut(l).blocks[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_repairs_params() {
        let spec = PedalboardSpec {
            master: LaneSpec {
                blocks: vec![BlockSpec {
                    id: 1,
                    kind: BlockKind::Reverb,
                    bypass: false,
                    params: vec![f32::NAN, 1e9],
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let board = LiveBoard::from_spec(spec);
        let b = board.block(1).unwrap();
        let defs = BlockKind::Reverb.params();
        assert_eq!(b.params.len(), defs.len());
        assert_eq!(b.params[0], defs[0].default);
        assert_eq!(b.params[1], defs[1].max);
    }

    #[test]
    fn topology_edits() {
        let mut b = LiveBoard::default();
        let a = b.add_block(0, BlockKind::Eq3);
        let c = b.add_block(0, BlockKind::Delay);
        assert_eq!(b.locate(c), Some((0, 1)));
        b.move_block(c, -1);
        assert_eq!(b.locate(c), Some((0, 0)));
        b.remove_block(a);
        assert!(b.handles(a).is_none());
        assert!(!b.spec().is_neutral());
        b.remove_block(c);
        assert!(b.spec().is_neutral());
    }

    #[test]
    fn param_edit_reaches_atomic() {
        let mut b = LiveBoard::default();
        let id = b.add_block(MASTER, BlockKind::Gain);
        b.set_param(id, 0, 6.0);
        assert_eq!(b.handles(id).unwrap().params[0].value(), 6.0);
        assert_eq!(b.block(id).unwrap().params[0], 6.0);
    }
}

//! `pedalboard.json`: the active board plus named user presets, alongside
//! a few built-in factory presets.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{BlockKind, BlockSpec, LaneSpec, PedalboardSpec};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedPreset {
    pub name: String,
    pub board: PedalboardSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PedalboardStore {
    /// Board in use (restored at launch).
    #[serde(default)]
    pub active: PedalboardSpec,
    /// Name of the preset `active` was loaded from, if any.
    #[serde(default)]
    pub active_name: Option<String>,
    #[serde(default)]
    pub presets: Vec<NamedPreset>,
}

fn path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("pedalboard.json"))
}

impl PedalboardStore {
    pub fn load() -> Self {
        let Some(p) = path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str::<Self>(&s)
                .map(|mut st| {
                    st.active = st.active.normalised();
                    st
                })
                .unwrap_or_else(|e| {
                    eprintln!("[pedalboard] ignoring unreadable {}: {e}", p.display());
                    Self::default()
                }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                // Write-then-rename so a crash can't leave a truncated file.
                let tmp = p.with_extension("json.tmp");
                if std::fs::write(&tmp, s)
                    .and_then(|_| std::fs::rename(&tmp, &p))
                    .is_err()
                {
                    eprintln!("[pedalboard] cannot save {}", p.display());
                }
            }
            Err(e) => eprintln!("[pedalboard] serialise failed: {e}"),
        }
    }

    /// Save (or overwrite) a user preset.
    pub fn upsert(&mut self, name: &str, board: PedalboardSpec) {
        match self.presets.iter_mut().find(|p| p.name == name) {
            Some(p) => p.board = board,
            None => self.presets.push(NamedPreset {
                name: name.to_string(),
                board,
            }),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.presets.retain(|p| p.name != name);
    }

    /// User presets first, then factory presets not shadowed by a user one.
    pub fn all(&self) -> Vec<NamedPreset> {
        let mut out = self.presets.clone();
        for f in factory() {
            if !out.iter().any(|p| p.name == f.name) {
                out.push(f);
            }
        }
        out
    }
}

fn block(id: u64, kind: BlockKind, params: &[(usize, f32)]) -> BlockSpec {
    let mut b = BlockSpec::new(id, kind);
    for &(i, v) in params {
        b.params[i] = v;
    }
    b
}

fn master_only(blocks: Vec<BlockSpec>) -> PedalboardSpec {
    let mut s = PedalboardSpec::default();
    s.master = LaneSpec {
        blocks,
        ..Default::default()
    };
    s
}

pub fn factory() -> Vec<NamedPreset> {
    let mut wide = PedalboardSpec::default();
    // Pull the hard-panned chips in a little so 2SID tunes are easier on
    // headphones, then widen the mono content.
    wide.lanes[0].pan = Some(-0.6);
    wide.lanes[1].pan = Some(0.6);
    wide.lanes[0].blocks = vec![block(1, BlockKind::Chorus, &[(2, 0.35)])];
    wide.master.blocks = vec![block(2, BlockKind::Widener, &[(0, 1.4)])];

    vec![
        NamedPreset {
            name: "Clean".into(),
            board: PedalboardSpec::default(),
        },
        NamedPreset {
            name: "Hall".into(),
            board: master_only(vec![
                block(1, BlockKind::Reverb, &[(0, 18.0), (1, 3.5), (3, 0.3)]),
                block(2, BlockKind::Limiter, &[]),
            ]),
        },
        NamedPreset {
            name: "Wide Chip".into(),
            board: wide,
        },
        NamedPreset {
            name: "Lo-Fi".into(),
            board: master_only(vec![
                block(1, BlockKind::Bitcrusher, &[(0, 10.0), (1, 3.0), (2, 0.6)]),
                block(2, BlockKind::TapeWobble, &[(1, 0.8)]),
                block(3, BlockKind::Filter, &[(1, 7000.0)]),
            ]),
        },
        NamedPreset {
            name: "Dub Delay".into(),
            board: master_only(vec![
                block(
                    1,
                    BlockKind::Delay,
                    &[(0, 428.0), (1, 0.55), (2, 0.35), (4, 2500.0)],
                ),
                block(2, BlockKind::Reverb, &[(3, 0.15)]),
                block(3, BlockKind::Limiter, &[]),
            ]),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips() {
        let mut st = PedalboardStore::default();
        st.active = factory()[1].board.clone();
        st.active_name = Some("Hall".into());
        st.upsert("Mine", factory()[4].board.clone());
        let json = serde_json::to_string(&st).unwrap();
        let back: PedalboardStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn factory_presets_are_valid() {
        for p in factory() {
            assert_eq!(p.board.clone().normalised(), p.board, "{}", p.name);
        }
    }

    #[test]
    fn unknown_fields_and_missing_fields_tolerated() {
        let st: PedalboardStore = serde_json::from_str(
            r#"{"future_field": 1, "active": {"lanes": [{},{},{},{}], "master": {}}}"#,
        )
        .unwrap();
        assert!(st.active.is_neutral());
    }
}

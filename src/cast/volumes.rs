//! Remembered AirPlay speaker volumes (`cast_volumes.json`).
//!
//! AirPlay has no "keep the speaker's own volume": the sender always sets
//! it, and without one receivers play at 0 dB (full). So Phosphor starts
//! each AirPlay device at the level last used with it, or [`DEFAULT`].

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Starting level for an AirPlay device we haven't cast to before.
pub const DEFAULT: f32 = 0.20;

#[derive(Default)]
pub struct VolumeMemory {
    levels: BTreeMap<String, f32>,
}

fn path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("cast_volumes.json"))
}

impl VolumeMemory {
    pub fn load() -> Self {
        let levels = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<BTreeMap<String, f32>>(&s).ok())
            .unwrap_or_default();
        Self { levels }
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.levels) {
            let _ = std::fs::write(p, s);
        }
    }

    pub fn get(&self, device: &str) -> f32 {
        self.levels
            .get(device)
            .copied()
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(DEFAULT)
    }

    pub fn set(&mut self, device: &str, level: f32) {
        self.levels
            .insert(device.to_string(), level.clamp(0.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_device_gets_safe_default() {
        let mut m = VolumeMemory::default();
        assert_eq!(m.get("tv"), DEFAULT);
        m.set("tv", 1.7);
        assert_eq!(m.get("tv"), 1.0);
        m.set("tv", 0.42);
        assert_eq!(m.get("tv"), 0.42);
    }
}

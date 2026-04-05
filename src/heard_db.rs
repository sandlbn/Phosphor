//  persistent set of HVSC MD5 hashes that have ever been played.
//

use std::collections::HashSet;
use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
//  HeardDb
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct HeardDb {
    /// Set of lowercase hex MD5 strings that have been heard at least once.
    pub heard: HashSet<String>,
    /// Set dirty when a new MD5 was added (so we only save when needed).
    dirty: bool,
}

impl HeardDb {
    /// Load from the config directory, or return an empty DB.
    pub fn load() -> Self {
        match db_path() {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                let heard: HashSet<String> = text
                    .lines()
                    .map(|l| l.trim().to_lowercase())
                    .filter(|l| l.len() == 32) // valid MD5 length
                    .collect();
                let count = heard.len();
                eprintln!("[phosphor] HeardDb: loaded {} unique tracks heard", count);
                Self {
                    heard,
                    dirty: false,
                }
            }
            _ => {
                eprintln!("[phosphor] HeardDb: starting fresh");
                Self::default()
            }
        }
    }

    /// Persist to disk only if the set has changed since last save.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(p) = db_path() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut lines: Vec<&str> = self.heard.iter().map(|s| s.as_str()).collect();
            lines.sort_unstable(); // deterministic output, friendly for diff
            let content = lines.join("\n");
            if std::fs::write(&p, content).is_ok() {
                self.dirty = false;
            }
        }
    }

    /// Record a played MD5. Returns `true` if this was a new entry (first time
    /// hearing this track), `false` if already in the set.
    pub fn record(&mut self, md5: &str) -> bool {
        let key = md5.trim().to_lowercase();
        if key.len() != 32 || key.is_empty() {
            return false;
        }
        if self.heard.insert(key) {
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Format the completion as a human-readable string.
    /// e.g. "1337 / 50127 SIDs heard (2.67%)"
    pub fn format_completion(&self, hvsc_total: usize) -> String {
        let heard = self.heard.len();
        if hvsc_total == 0 || heard == 0 {
            // DB not loaded yet or nothing heard — show nothing so the bar stays clean
            return String::new();
        }
        let pct = heard as f64 / hvsc_total as f64 * 100.0;
        if pct < 0.01 {
            format!("{} of {} HVSC SIDs heard ({:.4}%)", heard, hvsc_total, pct)
        } else if pct < 1.0 {
            format!("{} of {} HVSC SIDs heard ({:.2}%)", heard, hvsc_total, pct)
        } else {
            format!("{} of {} HVSC SIDs heard ({:.1}%)", heard, hvsc_total, pct)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Path helper
// ─────────────────────────────────────────────────────────────────────────────

fn db_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("heard.txt"))
}

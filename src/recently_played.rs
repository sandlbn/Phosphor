// Recently played history — persisted ring-buffer of the last 100 unique tracks.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 100;

// ─────────────────────────────────────────────────────────────────────────────
//  Entry
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    /// HVSC MD5 — used as the deduplication key.
    pub md5: String,
    pub title: String,
    pub author: String,
    pub released: String,
    pub path: PathBuf,
    /// Unix timestamp (seconds) of the last time this track was played.
    pub played_at: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Database
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecentlyPlayed {
    /// Ordered newest-first.
    pub entries: VecDeque<RecentEntry>,
}

impl RecentlyPlayed {
    /// Load from the config directory, or return an empty DB.
    pub fn load() -> Self {
        match db_path() {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                serde_json::from_str(&text).unwrap_or_default()
            }
            _ => Self::default(),
        }
    }

    /// Persist to disk.
    pub fn save(&self) {
        if let Some(p) = db_path() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(&p, json);
            }
        }
    }

    /// Record a play. If the MD5 is already in the list, move it to the
    /// front and update the timestamp. Otherwise prepend a new entry.
    /// Trims to MAX_ENTRIES.
    pub fn record(
        &mut self,
        md5: &str,
        title: &str,
        author: &str,
        released: &str,
        path: &std::path::Path,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Remove existing entry with the same MD5 (deduplication)
        self.entries.retain(|e| e.md5 != md5);

        // Prepend the fresh entry
        self.entries.push_front(RecentEntry {
            md5: md5.to_string(),
            title: title.to_string(),
            author: author.to_string(),
            released: released.to_string(),
            path: path.to_path_buf(),
            played_at: now,
        });

        // Cap at MAX_ENTRIES
        self.entries.truncate(MAX_ENTRIES);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn db_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("recently_played.json"))
}

/// Format a Unix timestamp as a human-readable relative string.
/// e.g. "just now", "5 min ago", "2 h ago", "3 days ago", "2025-01-15"
pub fn format_played_at(played_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let age = now.saturating_sub(played_at);

    if age < 60 {
        "just now".to_string()
    } else if age < 3_600 {
        format!("{} min ago", age / 60)
    } else if age < 86_400 {
        format!("{} h ago", age / 3_600)
    } else if age < 7 * 86_400 {
        let days = age / 86_400;
        format!("{} day{} ago", days, if days == 1 { "" } else { "s" })
    } else {
        // Older than a week: show date as YYYY-MM-DD
        let secs = played_at as i64;
        let days_since_epoch = secs / 86_400;
        // Simple Gregorian calendar conversion (good enough for display)
        let (y, m, d) = days_to_ymd(days_since_epoch);
        format!("{:04}-{:02}-{:02}", y, m, d)
    }
}

/// Minimal days-since-epoch → (year, month, day) conversion.
/// Accurate for dates from 1970 onward.
fn days_to_ymd(mut days: i64) -> (i64, i64, i64) {
    // Algorithm: civil calendar from Howard Hinnant
    days += 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

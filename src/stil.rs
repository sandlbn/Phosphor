// stil.rs — HVSC STIL.txt parser and lookup.
//
// STIL (SID Tune Information List) is a flat text database distributed with
// the High Voltage SID Collection. It maps HVSC-relative paths like
// "/MUSICIANS/H/Hubbard_Rob/Commando.sid" to rich metadata: the original
// song titles the SID covers, artist credits, and curator comments.
//
// Format summary (from STIL.faq):
//
//   ### Section header (composer/dir name) #######
//
//   /MUSICIANS/H/Hubbard_Rob/Commando.sid
//    COMMENT: Global file-level comment.
//    (#1)
//    TITLE: Eye of the Tiger [from …]
//    ARTIST: Survivor
//    (#2)
//    NAME: subtune name
//    AUTHOR: correct composer
//    TITLE: Another cover [from …] (0:45)
//    ARTIST: Another artist
//    COMMENT: Subtune-specific comment.
//
// Rules:
//   • Lines starting with "###" are section headers (ignored by lookup).
//   • A line starting at column 0 with "/" is a file key (HVSC-relative path).
//   • A line starting at column 0 with "  " is a STIL section comment on a dir.
//   • Field lines are indented by exactly one space: " FIELD: value".
//   • Subtune blocks are introduced by " (#N)" at column 1.
//   • Multi-line field values are indented by more than one space.
//   • The file is Latin-1 / ISO-8859-1 encoded (not UTF-8).
//
// Lookup strategy:
//   Because the user's SID files may not be in an HVSC tree, we match by
//   filename only (case-insensitive) as a fallback when the full path is
//   not available.  A full HVSC-relative path match always takes priority.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────────
//  Public data types
// ─────────────────────────────────────────────────────────────────────────────

/// One cover / tune block within a STIL entry.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct StilTuneEntry {
    /// Subtune number this block applies to, or 0 for "all tunes" (global).
    pub subtune: u8,
    /// The `NAME:` field — original name of the subtune.
    pub name: Option<String>,
    /// The `AUTHOR:` field — composer who wrote the *original* piece covered.
    pub author: Option<String>,
    /// The `TITLE:` field — title of the original piece being covered.
    pub title: Option<String>,
    /// The `ARTIST:` field — performer of the original piece.
    pub artist: Option<String>,
    /// The `COMMENT:` field.
    pub comment: Option<String>,
}

impl StilTuneEntry {
    /// True if this entry has any displayable content.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.author.is_none()
            && self.title.is_none()
            && self.artist.is_none()
            && self.comment.is_none()
    }
}

/// All STIL information for one SID file.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct StilEntry {
    /// HVSC-relative path, e.g. "/MUSICIANS/H/Hubbard_Rob/Commando.sid".
    pub hvsc_path: String,
    /// Global (file-level) comment that applies to all subtunes.
    pub global_comment: Option<String>,
    /// Per-subtune entries.  Subtune 0 = applies to whole SID.
    pub tunes: Vec<StilTuneEntry>,
}

impl StilEntry {
    /// Return the best displayable text block for the given 1-based subtune
    /// (or subtune 0 / None for the global view).
    pub fn for_subtune(&self, subtune: u16) -> Vec<&StilTuneEntry> {
        let mut result: Vec<&StilTuneEntry> = self
            .tunes
            .iter()
            .filter(|t| t.subtune == 0 || t.subtune == subtune as u8)
            .collect();
        // Sort: global (0) first, then specific subtune.
        result.sort_by_key(|t| t.subtune);
        result
    }

    /// Flat formatted string for display, combining global comment + subtune.
    pub fn format_for_display(&self, subtune: u16) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(ref gc) = self.global_comment {
            parts.push(gc.clone());
        }

        for tune in self.for_subtune(subtune) {
            if tune.subtune > 0 {
                parts.push(format!("— Subtune {} —", tune.subtune));
            }
            if let Some(ref v) = tune.title {
                parts.push(format!("Title:   {v}"));
            }
            if let Some(ref v) = tune.artist {
                parts.push(format!("Artist:  {v}"));
            }
            if let Some(ref v) = tune.name {
                parts.push(format!("Name:    {v}"));
            }
            if let Some(ref v) = tune.author {
                parts.push(format!("Author:  {v}"));
            }
            if let Some(ref v) = tune.comment {
                parts.push(v.clone());
            }
        }

        parts.join("\n")
    }

    pub fn has_content(&self) -> bool {
        self.global_comment.is_some() || !self.tunes.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  StilDb — loaded database
// ─────────────────────────────────────────────────────────────────────────────

/// The parsed STIL database, indexed two ways for flexible lookup.
pub struct StilDb {
    /// Primary index: lowercase HVSC-relative path → entry.
    by_path: HashMap<String, StilEntry>,
    /// Secondary index: lowercase filename (no path, no ext) → list of paths.
    /// Used when the file's HVSC path is not known.
    by_filename: HashMap<String, Vec<String>>,
    /// Number of entries loaded.
    pub count: usize,
}

#[allow(dead_code)]
impl StilDb {
    /// Parse a STIL.txt file from disk.
    ///
    /// The file is ISO-8859-1 encoded; we read raw bytes and lossily convert.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read(path).map_err(|e| format!("Cannot read STIL file: {e}"))?;

        // ISO-8859-1 → String: each byte maps 1:1 to the same Unicode codepoint.
        let content: String = raw.iter().map(|&b| b as char).collect();

        let mut db = StilDb {
            by_path: HashMap::new(),
            by_filename: HashMap::new(),
            count: 0,
        };

        db.parse(&content);
        db.count = db.by_path.len();

        eprintln!(
            "[phosphor] STIL: loaded {} entries from {}",
            db.count,
            path.display()
        );
        Ok(db)
    }

    /// Look up a SID file.
    ///
    /// `sid_path` is the absolute path to the file on disk.
    /// `hvsc_root` is an optional hint about where the HVSC tree is rooted;
    /// if provided and `sid_path` is inside it, we compute the HVSC-relative
    /// path for a precise match.
    pub fn lookup(&self, sid_path: &Path, hvsc_root: Option<&Path>) -> Option<&StilEntry> {
        // 1. Try precise HVSC-relative path match.
        if let Some(root) = hvsc_root {
            if let Ok(rel) = sid_path.strip_prefix(root) {
                let hvsc_key =
                    format!("/{}", rel.to_string_lossy().replace('\\', "/")).to_lowercase();
                if let Some(e) = self.by_path.get(&hvsc_key) {
                    return Some(e);
                }
            }
        }

        // 2. Fallback: match by filename stem (case-insensitive).
        //    If there's exactly one STIL entry with that filename we use it;
        //    if there are multiple we pick the first (ambiguous, but better
        //    than nothing).
        let stem = sid_path.file_name()?.to_string_lossy().to_lowercase();
        let stem_no_ext = stem.trim_end_matches(".sid");

        if let Some(paths) = self.by_filename.get(stem_no_ext) {
            if let Some(best_path) = paths.first() {
                return self.by_path.get(best_path.as_str());
            }
        }

        None
    }

    /// Look up by HVSC-relative path string directly (case-insensitive).
    pub fn lookup_by_hvsc_path(&self, hvsc_path: &str) -> Option<&StilEntry> {
        self.by_path.get(&hvsc_path.to_lowercase())
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Parser
    // ─────────────────────────────────────────────────────────────────────────

    fn parse(&mut self, content: &str) {
        let mut current_entry: Option<StilEntry> = None;
        let mut current_tune: Option<StilTuneEntry> = None;
        // Which field is currently being accumulated (for multi-line values).
        let mut current_field: CurrentField = CurrentField::None;

        let flush_tune = |entry: &mut StilEntry, tune: &mut Option<StilTuneEntry>| {
            if let Some(t) = tune.take() {
                if !t.is_empty() {
                    entry.tunes.push(t);
                }
            }
        };

        let flush_entry =
            |db: &mut StilDb, entry: &mut Option<StilEntry>, tune: &mut Option<StilTuneEntry>| {
                if let Some(mut e) = entry.take() {
                    if let Some(t) = tune.take() {
                        if !t.is_empty() {
                            e.tunes.push(t);
                        }
                    }
                    if e.has_content() {
                        let key = e.hvsc_path.to_lowercase();
                        // Register in by_filename index.
                        let stem = PathBuf::from(&e.hvsc_path)
                            .file_name()
                            .map(|f| f.to_string_lossy().to_lowercase())
                            .unwrap_or_default();
                        let stem_no_ext = stem.trim_end_matches(".sid").to_string();
                        db.by_filename
                            .entry(stem_no_ext)
                            .or_default()
                            .push(key.clone());
                        db.by_path.insert(key, e);
                    }
                }
            };

        for line in content.lines() {
            // ── Section headers ────────────────────────────────────────────
            if line.starts_with("###") || line.starts_with('#') {
                // Section comment line or header — skip but don't disturb state.
                current_field = CurrentField::None;
                continue;
            }

            // ── New file entry (line starts with '/') ──────────────────────
            if line.starts_with('/') && !line.trim().is_empty() {
                // Flush previous entry.
                if let Some(ref mut e) = current_entry {
                    flush_tune(e, &mut current_tune);
                }
                flush_entry(self, &mut current_entry, &mut current_tune);

                let path = line.trim().to_string();
                current_entry = Some(StilEntry {
                    hvsc_path: path,
                    global_comment: None,
                    tunes: Vec::new(),
                });
                current_tune = None;
                current_field = CurrentField::None;
                continue;
            }

            // Everything else requires an active entry.
            let entry = match current_entry.as_mut() {
                Some(e) => e,
                None => continue,
            };

            let trimmed = line.trim();

            // ── Subtune header: " (#N)" ────────────────────────────────────
            if trimmed.starts_with("(#") && trimmed.ends_with(')') {
                // Flush previous tune block.
                flush_tune(entry, &mut current_tune);
                let n_str = trimmed.trim_start_matches("(#").trim_end_matches(')');
                let subtune = n_str.parse::<u8>().unwrap_or(0);
                current_tune = Some(StilTuneEntry {
                    subtune,
                    ..Default::default()
                });
                current_field = CurrentField::None;
                continue;
            }

            // ── Field lines: " FIELD: value" (leading space, field at col 1) ─
            // We detect them by checking for known field names at the start
            // of the trimmed line.
            if let Some((field, value)) = parse_field(trimmed) {
                current_field = field;
                let value = value.trim().to_string();

                // If no subtune block is active, treat as a global field.
                // Per STIL spec, COMMENT before any (#N) is "global comment".
                match field {
                    CurrentField::Comment => {
                        if current_tune.is_none() {
                            // Global comment.
                            append_or_set(&mut entry.global_comment, &value);
                        } else {
                            let tune = current_tune.get_or_insert_with(Default::default);
                            append_or_set(&mut tune.comment, &value);
                        }
                    }
                    CurrentField::Name => {
                        let tune = current_tune.get_or_insert_with(Default::default);
                        append_or_set(&mut tune.name, &value);
                    }
                    CurrentField::Author => {
                        let tune = current_tune.get_or_insert_with(Default::default);
                        append_or_set(&mut tune.author, &value);
                    }
                    CurrentField::Title => {
                        let tune = current_tune.get_or_insert_with(Default::default);
                        append_or_set(&mut tune.title, &value);
                    }
                    CurrentField::Artist => {
                        let tune = current_tune.get_or_insert_with(Default::default);
                        append_or_set(&mut tune.artist, &value);
                    }
                    CurrentField::None => {}
                }
                continue;
            }

            // ── Continuation line (more indented, no field prefix) ─────────
            // Multi-line field values have extra indentation.
            if !trimmed.is_empty() && current_field != CurrentField::None {
                let continuation = format!(" {trimmed}");
                match current_field {
                    CurrentField::Comment => {
                        if current_tune.is_none() {
                            append_continuation(&mut entry.global_comment, &continuation);
                        } else if let Some(ref mut tune) = current_tune {
                            append_continuation(&mut tune.comment, &continuation);
                        }
                    }
                    CurrentField::Name => {
                        if let Some(ref mut tune) = current_tune {
                            append_continuation(&mut tune.name, &continuation);
                        }
                    }
                    CurrentField::Author => {
                        if let Some(ref mut tune) = current_tune {
                            append_continuation(&mut tune.author, &continuation);
                        }
                    }
                    CurrentField::Title => {
                        if let Some(ref mut tune) = current_tune {
                            append_continuation(&mut tune.title, &continuation);
                        }
                    }
                    CurrentField::Artist => {
                        if let Some(ref mut tune) = current_tune {
                            append_continuation(&mut tune.artist, &continuation);
                        }
                    }
                    CurrentField::None => {}
                }
                continue;
            }

            // ── Blank line: reset continuation but keep active entry/tune ──
            if trimmed.is_empty() {
                current_field = CurrentField::None;
            }
        }

        // Flush the last entry.
        if let Some(ref mut e) = current_entry {
            flush_tune(e, &mut current_tune);
        }
        flush_entry(self, &mut current_entry, &mut current_tune);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Parser helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum CurrentField {
    None,
    Name,
    Author,
    Title,
    Artist,
    Comment,
}

/// Try to parse a field from a trimmed line.
/// Returns (field_type, value_after_colon) or None.
fn parse_field(trimmed: &str) -> Option<(CurrentField, &str)> {
    // STIL fields always start with the keyword followed by ": ".
    let candidates = [
        ("NAME:", CurrentField::Name),
        ("AUTHOR:", CurrentField::Author),
        ("TITLE:", CurrentField::Title),
        ("ARTIST:", CurrentField::Artist),
        ("COMMENT:", CurrentField::Comment),
    ];
    for (prefix, field) in &candidates {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((*field, rest));
        }
    }
    None
}

/// Set a field for the first time, or append a newline + continuation.
fn append_or_set(target: &mut Option<String>, value: &str) {
    match target {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(value);
        }
        None => {
            *target = Some(value.to_string());
        }
    }
}

/// Append a continuation line to an existing field value.
fn append_continuation(target: &mut Option<String>, cont: &str) {
    if let Some(existing) = target {
        existing.push('\n');
        existing.push_str(cont.trim_start());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Download helper (mirrors download_songlength in config.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Download STIL.txt from `url` and save it to our config directory.
/// Returns the path to the saved file on success.
pub async fn download_stil(url: String) -> Result<PathBuf, String> {
    let dest = crate::config::config_dir()
        .ok_or_else(|| "Cannot determine config directory".to_string())?
        .join("STIL.txt");

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create directory: {e}"))?;
    }

    eprintln!("[phosphor] Downloading STIL.txt from {url}...");

    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "120", // STIL.txt is several MB, allow more time
            "-o",
            &dest.to_string_lossy(),
            &url,
        ])
        .output()
        .map_err(|e| format!("Failed to run curl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Download failed: {stderr}"));
    }

    let meta = std::fs::metadata(&dest).map_err(|e| format!("Downloaded file not found: {e}"))?;

    eprintln!(
        "[phosphor] STIL.txt saved to {} ({} bytes)",
        dest.display(),
        meta.len(),
    );
    Ok(dest)
}

/// Default path for STIL.txt in the config directory.
pub fn stil_db_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("STIL.txt"))
}

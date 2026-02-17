// Playlist management: track list, shuffle, repeat modes, Songlength DB.

use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::player::sid_file;

// ─────────────────────────────────────────────────────────────────────────────
//  Playlist entry
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    pub title: String,
    pub author: String,
    pub released: String,
    pub songs: u16,
    /// Which sub-tune to play (1-based).
    pub selected_song: u16,
    pub is_pal: bool,
    pub num_sids: usize,
    /// True if RSID, false if PSID.
    pub is_rsid: bool,
    /// HVSC MD5 (computed lazily).
    pub md5: Option<String>,
    /// Duration from Songlength DB, if available (seconds).
    pub duration_secs: Option<u32>,
}

impl PlaylistEntry {
    /// Try to create an entry by reading and parsing a .sid file header.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let data =
            std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let sid = sid_file::load_sid(&data)?;
        let h = &sid.header;

        let md5 = sid_file::compute_hvsc_md5(&sid);
        eprintln!(
            "[phosphor] {} → MD5: {}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            md5,
        );

        Ok(Self {
            path: path.to_path_buf(),
            title: if h.name.is_empty() {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                h.name.clone()
            },
            author: h.author.clone(),
            released: h.released.clone(),
            songs: h.songs,
            selected_song: h.start_song,
            is_pal: h.is_pal,
            num_sids: h.num_sids(),
            is_rsid: h.is_rsid,
            md5: Some(md5),
            duration_secs: None,
        })
    }

    pub fn format_duration(&self) -> String {
        match self.duration_secs {
            Some(s) => format!("{}:{:02}", s / 60, s % 60),
            None => "—:——".to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Repeat / shuffle modes
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RepeatMode {
    Off,
    All,
    Single,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::Single,
            Self::Single => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "⮔ Off",
            Self::All => "🔁 All",
            Self::Single => "🔂 One",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Playlist
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Playlist {
    pub entries: Vec<PlaylistEntry>,
    /// Current playing index (into `entries`).
    pub current: Option<usize>,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Shuffle order (indices into `entries`).
    shuffle_order: Vec<usize>,
    /// Position within shuffle_order.
    shuffle_pos: usize,
}

impl Playlist {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            repeat: RepeatMode::Off,
            shuffle: false,
            shuffle_order: Vec::new(),
            shuffle_pos: 0,
        }
    }

    /// Add a single .sid file.
    pub fn add_file(&mut self, path: &Path) -> Result<(), String> {
        let entry = PlaylistEntry::from_path(path)?;
        self.entries.push(entry);
        self.rebuild_shuffle();
        Ok(())
    }

    /// Recursively add all .sid files from a directory.
    pub fn add_directory(&mut self, dir: &Path) -> usize {
        let mut count = 0;
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.extension().map(|e| e.to_ascii_lowercase()) == Some("sid".into()) {
                if self.add_file(p).is_ok() {
                    count += 1;
                }
            }
        }
        self.rebuild_shuffle();
        count
    }

    /// Bulk-add pre-parsed entries (used by background loading tasks).
    pub fn add_entries(&mut self, entries: Vec<PlaylistEntry>) {
        self.entries.extend(entries);
        self.rebuild_shuffle();
    }

    /// Remove entry at index.
    pub fn remove(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries.remove(idx);
            // Adjust current index
            if let Some(ref mut cur) = self.current {
                if idx < *cur {
                    *cur -= 1;
                } else if idx == *cur {
                    self.current = None;
                }
            }
            self.rebuild_shuffle();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.current = None;
        self.shuffle_order.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ── M3U playlist save / load ─────────────────────────────────────────

    /// Save the playlist as an extended M3U file.
    ///
    /// Format:
    /// ```text
    /// #EXTM3U
    /// #EXTINF:123,Artist - Title
    /// #PHOSPHOR:song=2
    /// /absolute/path/to/file.sid
    /// ```
    ///
    /// The `#PHOSPHOR:` line is optional metadata that preserves the
    /// selected sub-tune. Standard M3U players ignore unknown `#` lines.
    /// Duration from `#EXTINF` is restored on load, so the Songlength DB
    /// doesn't need to be re-scanned for known tunes.
    #[allow(dead_code)]
    pub fn save_m3u(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;

        let mut f = std::fs::File::create(path)
            .map_err(|e| format!("Cannot create {}: {e}", path.display()))?;

        writeln!(f, "#EXTM3U").map_err(|e| format!("Write error: {e}"))?;

        for entry in &self.entries {
            let duration = entry.duration_secs.unwrap_or(0) as i64;
            let display = if entry.author.is_empty() {
                entry.title.clone()
            } else {
                format!("{} - {}", entry.author, entry.title)
            };
            writeln!(f, "#EXTINF:{},{}", duration, display)
                .map_err(|e| format!("Write error: {e}"))?;
            // Persist selected sub-tune so it survives reload
            if entry.selected_song != 1 || entry.songs > 1 {
                writeln!(f, "#PHOSPHOR:song={}", entry.selected_song)
                    .map_err(|e| format!("Write error: {e}"))?;
            }
            writeln!(f, "{}", entry.path.display()).map_err(|e| format!("Write error: {e}"))?;
        }

        eprintln!(
            "[phosphor] Saved {} tracks to {}",
            self.entries.len(),
            path.display()
        );
        Ok(())
    }

    /// Load tracks from an M3U or PLS playlist file.
    /// Supports: plain M3U, extended M3U (#EXTM3U), and basic PLS.
    /// Durations from #EXTINF and sub-tune selections from #PHOSPHOR
    /// are restored, avoiding a Songlength DB re-scan for known tunes.
    /// Returns the number of tracks successfully loaded.
    pub fn load_playlist_file(&mut self, path: &Path) -> Result<usize, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

        let playlist_dir = path.parent().unwrap_or(Path::new("."));

        let ext = path
            .extension()
            .map(|e| e.to_ascii_lowercase().to_string_lossy().to_string())
            .unwrap_or_default();

        let mut loaded = 0;

        if ext == "pls" {
            let paths = parse_pls(&content, playlist_dir);
            for p in &paths {
                if p.is_dir() {
                    loaded += self.add_directory(p);
                } else if self.add_file(p).is_ok() {
                    loaded += 1;
                } else {
                    eprintln!(
                        "[phosphor] Playlist: skipping {} (not a valid SID)",
                        p.display()
                    );
                }
            }
        } else {
            let items = parse_m3u(&content, playlist_dir);
            for item in items {
                if item.path.is_dir() {
                    loaded += self.add_directory(&item.path);
                } else if self.add_file(&item.path).is_ok() {
                    // Apply saved metadata to the entry we just added
                    if let Some(entry) = self.entries.last_mut() {
                        if let Some(dur) = item.duration_secs {
                            entry.duration_secs = Some(dur);
                        }
                        if let Some(song) = item.selected_song {
                            if song >= 1 && song <= entry.songs {
                                entry.selected_song = song;
                            }
                        }
                    }
                    loaded += 1;
                } else {
                    eprintln!(
                        "[phosphor] Playlist: skipping {} (not a valid SID)",
                        item.path.display()
                    );
                }
            }
        }

        self.rebuild_shuffle();

        eprintln!("[phosphor] Loaded {loaded} tracks from {}", path.display());
        Ok(loaded)
    }

    /// Get the next track index according to repeat/shuffle settings.
    pub fn next(&mut self) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }

        match self.repeat {
            RepeatMode::Single => {
                // Keep playing the same track
                self.current
            }
            _ => {
                let idx = if self.shuffle {
                    self.shuffle_pos += 1;
                    if self.shuffle_pos >= self.shuffle_order.len() {
                        if self.repeat == RepeatMode::All {
                            self.reshuffle();
                            self.shuffle_pos = 0;
                        } else {
                            return None; // End of shuffled playlist
                        }
                    }
                    self.shuffle_order.get(self.shuffle_pos).copied()
                } else {
                    let next = match self.current {
                        Some(cur) => cur + 1,
                        None => 0,
                    };
                    if next >= self.entries.len() {
                        if self.repeat == RepeatMode::All {
                            Some(0)
                        } else {
                            None
                        }
                    } else {
                        Some(next)
                    }
                };

                if let Some(i) = idx {
                    self.current = Some(i);
                }
                idx
            }
        }
    }

    /// Get the previous track index.
    pub fn prev(&mut self) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }

        let idx = if self.shuffle {
            if self.shuffle_pos > 0 {
                self.shuffle_pos -= 1;
                self.shuffle_order.get(self.shuffle_pos).copied()
            } else {
                self.shuffle_order.first().copied()
            }
        } else {
            match self.current {
                Some(0) => {
                    if self.repeat == RepeatMode::All {
                        Some(self.entries.len() - 1)
                    } else {
                        Some(0)
                    }
                }
                Some(cur) => Some(cur - 1),
                None => Some(0),
            }
        };

        if let Some(i) = idx {
            self.current = Some(i);
        }
        idx
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.reshuffle();
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.cycle();
    }

    fn rebuild_shuffle(&mut self) {
        self.shuffle_order = (0..self.entries.len()).collect();
        if self.shuffle {
            self.reshuffle();
        }
    }

    fn reshuffle(&mut self) {
        let mut rng = rand::thread_rng();
        self.shuffle_order.shuffle(&mut rng);
        self.shuffle_pos = 0;
    }

    /// Current entry reference.
    pub fn current_entry(&self) -> Option<&PlaylistEntry> {
        self.current.and_then(|i| self.entries.get(i))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  HVSC Songlength database
// ─────────────────────────────────────────────────────────────────────────────

/// Parsed Songlength.md5 database: MD5 → Vec<duration_seconds> (one per sub-tune).
#[derive(Debug, Clone)]
pub struct SonglengthDb {
    pub entries: HashMap<String, Vec<u32>>,
}

impl SonglengthDb {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Try to auto-load from the standard config directory.
    /// Looks in: <config_dir>/ultimate64-manager/Songlengths.md5
    /// (Same path as the ultimate64-manager player)
    pub fn auto_load() -> Option<Self> {
        let config_dir = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))?;
        let db_path = config_dir
            .join("ultimate64-manager")
            .join("Songlengths.md5");

        if !db_path.exists() {
            eprintln!("[phosphor] No Songlengths.md5 at {}", db_path.display());
            return None;
        }

        eprintln!("[phosphor] Found Songlengths.md5 at {}", db_path.display());
        match Self::load(&db_path) {
            Ok(db) => {
                eprintln!(
                    "[phosphor] Auto-loaded {} songlength entries",
                    db.entries.len()
                );
                Some(db)
            }
            Err(e) => {
                eprintln!("[phosphor] Failed to load Songlengths.md5: {e}");
                None
            }
        }
    }

    /// Load from an HVSC Songlength.md5 file.
    ///
    /// Format:
    ///   ; comment
    ///   # comment
    ///   [Database]
    ///   [/path/to/file.sid]
    ///   MD5=mm:ss mm:ss mm:ss ...
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

        let mut db = Self::new();

        for line in content.lines() {
            let line = line.trim();
            // Skip empty lines, comments, and section headers
            // (matches the ultimate64-manager parser exactly)
            if line.is_empty()
                || line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with('[')
            {
                continue;
            }

            // Expected: MD5=time time time ...
            if let Some(eq_pos) = line.find('=') {
                let md5_str = &line[..eq_pos];
                let times_str = &line[eq_pos + 1..];

                // MD5 must be exactly 32 hex chars
                if md5_str.len() != 32 {
                    continue;
                }

                let md5 = md5_str.trim().to_lowercase();
                let durations: Vec<u32> = times_str
                    .split_whitespace()
                    .filter_map(|t| parse_songlength_time(t))
                    .map(|d| d + 1) // +1 second, same as ultimate64-manager
                    .collect();

                if !durations.is_empty() {
                    db.entries.insert(md5, durations);
                }
            }
        }

        Ok(db)
    }

    /// Look up duration for a specific MD5 and sub-tune (0-based).
    pub fn lookup(&self, md5: &str, subtune: usize) -> Option<u32> {
        self.entries
            .get(&md5.to_lowercase())
            .and_then(|v| v.get(subtune).copied())
    }

    /// Look up all subtune durations for a given MD5.
    #[allow(dead_code)]
    pub fn lookup_all(&self, md5: &str) -> Option<&Vec<u32>> {
        self.entries.get(&md5.to_lowercase())
    }

    /// Apply durations to all playlist entries that have MD5s.
    /// Entries that already have a duration (e.g. restored from an M3U file)
    /// are left untouched.
    pub fn apply_to_playlist(&self, playlist: &mut Playlist) {
        let mut applied = 0;
        let mut skipped = 0;
        for entry in &mut playlist.entries {
            // Don't overwrite durations already loaded from the playlist file
            if entry.duration_secs.is_some() {
                skipped += 1;
                continue;
            }
            if let Some(ref md5) = entry.md5 {
                let subtune = entry.selected_song.saturating_sub(1) as usize;
                if let Some(dur) = self.lookup(md5, subtune) {
                    entry.duration_secs = Some(dur);
                    applied += 1;
                } else {
                    eprintln!(
                        "[phosphor] Songlength MISS: \"{}\" md5={} subtune={}",
                        entry.title, md5, subtune,
                    );
                }
            }
        }
        if applied > 0 || skipped > 0 {
            eprintln!(
                "[phosphor] Songlengths: applied={applied}, already_known={skipped}, total={}",
                playlist.entries.len()
            );
        }
    }
}

/// Parse "mm:ss", "mm:ss.xxx", or "mm:ss(G)" into whole seconds.
fn parse_songlength_time(s: &str) -> Option<u32> {
    let s = s.trim();
    // Strip optional attribute suffixes like (G), (M), (Z), (B)
    let s = s.split('(').next().unwrap_or(s);
    let (min_str, sec_part) = s.split_once(':')?;
    // Strip optional fractional part (e.g. "45.123" → "45")
    let sec_str = sec_part.split('.').next().unwrap_or(sec_part);
    let min: u32 = min_str.parse().ok()?;
    let sec: u32 = sec_str.parse().ok()?;
    Some(min * 60 + sec)
}

// ─────────────────────────────────────────────────────────────────────────────
//  M3U / PLS parsers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse an M3U/M3U8 playlist. Handles both plain and extended (#EXTM3U).
/// Relative paths are resolved against `base_dir`.
/// Metadata parsed from M3U comment lines preceding a file path.
struct M3uMeta {
    path: PathBuf,
    duration_secs: Option<u32>,
    selected_song: Option<u16>,
}

fn parse_m3u(content: &str, base_dir: &Path) -> Vec<M3uMeta> {
    let mut results = Vec::new();
    let mut pending_duration: Option<u32> = None;
    let mut pending_song: Option<u16> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse #EXTINF:duration,title
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            if let Some((dur_str, _title)) = rest.split_once(',') {
                if let Ok(dur) = dur_str.trim().parse::<i64>() {
                    if dur > 0 {
                        pending_duration = Some(dur as u32);
                    }
                }
            }
            continue;
        }

        // Parse #PHOSPHOR:song=N
        if let Some(rest) = line.strip_prefix("#PHOSPHOR:") {
            for part in rest.split(',') {
                let part = part.trim();
                if let Some(val) = part.strip_prefix("song=") {
                    if let Ok(s) = val.parse::<u16>() {
                        pending_song = Some(s);
                    }
                }
            }
            continue;
        }

        // Skip other comments
        if line.starts_with('#') {
            continue;
        }

        // This is a file path — consume pending metadata
        let p = PathBuf::from(line);
        let resolved = if p.is_absolute() { p } else { base_dir.join(p) };

        results.push(M3uMeta {
            path: resolved,
            duration_secs: pending_duration.take(),
            selected_song: pending_song.take(),
        });
    }

    results
}

/// Parse a PLS playlist file.
/// Format:
/// ```text
/// [playlist]
/// File1=/path/to/file.sid
/// File2=/path/to/other.sid
/// NumberOfEntries=2
/// ```
fn parse_pls(content: &str, base_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        // Look for FileN= lines (case-insensitive)
        if let Some(rest) = line
            .strip_prefix("File")
            .or_else(|| line.strip_prefix("file"))
        {
            // Skip the number and '='
            if let Some((_num, path_str)) = rest.split_once('=') {
                let path_str = path_str.trim();
                if path_str.is_empty() {
                    continue;
                }
                let p = PathBuf::from(path_str);
                let resolved = if p.is_absolute() { p } else { base_dir.join(p) };
                paths.push(resolved);
            }
        }
    }

    paths
}

// ─────────────────────────────────────────────────────────────────────────────
//  Background parsing helpers (for use in async tasks, off the UI thread)
// ─────────────────────────────────────────────────────────────────────────────

/// Shared progress string for background loading tasks.
/// Updated by the background thread, read by the UI on each tick.
pub type LoadingProgress = std::sync::Arc<std::sync::Mutex<String>>;

/// Parse a list of SID file paths into playlist entries (blocking I/O).
/// Designed to be called from a background thread via `Task::perform`.
pub fn parse_files(paths: Vec<PathBuf>, progress: LoadingProgress) -> Vec<PlaylistEntry> {
    let total = paths.len();
    let mut entries = Vec::with_capacity(total);
    for (i, p) in paths.iter().enumerate() {
        if let Ok(mut pg) = progress.lock() {
            *pg = format!("⏳ Adding files: {} / {}", i + 1, total);
        }
        if let Ok(e) = PlaylistEntry::from_path(p) {
            entries.push(e);
        }
    }
    // Don't clear progress here — the main thread handler will clear it
    // after post-processing (add_entries, songlengths, filter) is done.
    entries
}

/// Recursively walk a directory and parse all .sid files (blocking I/O).
/// Designed to be called from a background thread via `Task::perform`.
pub fn parse_directory(dir: PathBuf, progress: LoadingProgress) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    let mut count = 0usize;
    for entry in WalkDir::new(&dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().map(|e| e.to_ascii_lowercase()) == Some("sid".into()) {
            count += 1;
            if let Ok(mut pg) = progress.lock() {
                *pg = format!("⏳ Scanning folder: {} files found", count);
            }
            if let Ok(e) = PlaylistEntry::from_path(p) {
                entries.push(e);
            }
        }
    }
    // Don't clear — main thread handler clears after post-processing.
    entries
}

/// Parse a playlist file (M3U/PLS) and load all referenced SID files (blocking I/O).
/// Designed to be called from a background thread via `Task::perform`.
/// For M3U files, saved durations and sub-tune selections are restored.
pub fn parse_playlist_file(
    path: PathBuf,
    progress: LoadingProgress,
) -> Result<Vec<PlaylistEntry>, String> {
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

    let playlist_dir = path.parent().unwrap_or(Path::new("."));

    let ext = path
        .extension()
        .map(|e| e.to_ascii_lowercase().to_string_lossy().to_string())
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut count = 0usize;

    if ext == "pls" {
        let paths = parse_pls(&content, playlist_dir);
        let total = paths.len();
        for p in &paths {
            if p.is_dir() {
                entries.extend(parse_directory(p.clone(), progress.clone()));
            } else {
                count += 1;
                if let Ok(mut pg) = progress.lock() {
                    *pg = format!("⏳ Loading playlist: {} / {}", count, total);
                }
                if let Ok(e) = PlaylistEntry::from_path(p) {
                    entries.push(e);
                } else {
                    eprintln!(
                        "[phosphor] Playlist: skipping {} (not a valid SID)",
                        p.display()
                    );
                }
            }
        }
    } else {
        let items = parse_m3u(&content, playlist_dir);
        let total = items.len();
        for item in items {
            if item.path.is_dir() {
                entries.extend(parse_directory(item.path, progress.clone()));
            } else {
                count += 1;
                if let Ok(mut pg) = progress.lock() {
                    *pg = format!("⏳ Loading playlist: {} / {}", count, total);
                }
                if let Ok(mut e) = PlaylistEntry::from_path(&item.path) {
                    // Restore saved metadata from the M3U
                    if let Some(dur) = item.duration_secs {
                        e.duration_secs = Some(dur);
                    }
                    if let Some(song) = item.selected_song {
                        if song >= 1 && song <= e.songs {
                            e.selected_song = song;
                        }
                    }
                    entries.push(e);
                } else {
                    eprintln!(
                        "[phosphor] Playlist: skipping {} (not a valid SID)",
                        item.path.display()
                    );
                }
            }
        }
    }

    eprintln!(
        "[phosphor] Loaded {} entries from {}",
        entries.len(),
        path.display()
    );
    // Don't clear — main thread handler clears after post-processing.
    Ok(entries)
}

// HVSC browser — lazy two-column author/tune walker.
//
// Pure data model + std::fs walking. The iced UI lives in `ui/mod.rs`;
// this module just answers "what authors exist under MUSICIANS/?", "what
// tunes are in MUSICIANS/H/Hubbard_Rob/?", and applies optional
// songlength durations + STIL ✓ markers when those DBs are available.
//
// Design constraints (per the approved plan):
//   - Lazy: no upfront scan of the full ~75k file tree. Author list is
//     two shallow readdirs; tune list is one walkdir per selected author.
//   - Reuses PlaylistEntry::from_path so add-to-playlist is identical to
//     the existing add-folder flow.
//   - No async. Each user click is one synchronous filesystem walk that
//     completes in tens of milliseconds for a typical author folder.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::playlist::{PlaylistEntry, SonglengthDb};
use crate::stil::StilDb;

/// Browser source — picks which sub-view the Browse panel renders.
/// "Local HVSC" reads from the synced HVSC tree on disk; "Assembly64"
/// queries the remote A64 HTTP API. Persisted to config so the toggle
/// position survives restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSource {
    LocalHvsc,
    Assembly64,
}

impl Default for BrowserSource {
    fn default() -> Self {
        BrowserSource::LocalHvsc
    }
}

impl BrowserSource {
    pub fn label(self) -> &'static str {
        match self {
            BrowserSource::LocalHvsc => "Local HVSC",
            BrowserSource::Assembly64 => "Assembly64",
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            BrowserSource::LocalHvsc => "local",
            BrowserSource::Assembly64 => "a64",
        }
    }

    pub fn from_config_str(s: &str) -> Self {
        match s {
            "a64" => BrowserSource::Assembly64,
            _ => BrowserSource::LocalHvsc,
        }
    }
}

/// HVSC top-level category. DOCUMENTS/ is intentionally not browsable —
/// it's text files (Songlengths.md5, STIL.txt) not tunes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HvscCategory {
    Musicians,
    Demos,
    Games,
}

impl HvscCategory {
    pub fn dir_name(self) -> &'static str {
        match self {
            HvscCategory::Musicians => "MUSICIANS",
            HvscCategory::Demos => "DEMOS",
            HvscCategory::Games => "GAMES",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HvscCategory::Musicians => "Musicians",
            HvscCategory::Demos => "Demos",
            HvscCategory::Games => "Games",
        }
    }
}

/// One author folder under `<root>/<CATEGORY>/<letter>/`.
#[derive(Debug, Clone)]
pub struct HvscAuthor {
    /// Folder name as it appears on disk, e.g. `Hubbard_Rob`.
    pub raw_name: String,
    /// Display form derived from `raw_name`. `Hubbard_Rob` → `Hubbard, Rob`;
    /// `Robotron_4000` → `Robotron 4000` (no comma when it doesn't look like
    /// LastName_FirstName).
    pub display_name: String,
    /// First character of `raw_name`, uppercased — used for the
    /// alphabetical sticky-header in the UI.
    pub letter: char,
    /// Absolute path of the author folder.
    pub path: PathBuf,
}

/// One tune row in the right column.
#[derive(Debug, Clone)]
pub struct HvscTune {
    pub entry: PlaylistEntry,
    /// True if `StilDb::lookup_by_hvsc_path` finds an entry for this file.
    /// Used to render a ✓ in the STIL column.
    pub has_stil: bool,
}

/// Flat-index row for global search. Built once per category — one entry
/// per `.sid`/`.mus` file. No SID header parsing here; we only need the
/// filename + author dir name to drive search. When the user clicks a
/// search hit we lazy-load the full `PlaylistEntry` for that one path.
#[derive(Debug, Clone)]
pub struct HvscIndexEntry {
    pub path: PathBuf,
    /// File stem as displayed (e.g. `Commando`).
    pub stem: String,
    /// Author / section folder name as it appears on disk
    /// (`Hubbard_Rob` for MUSICIANS, `0-9` for DEMOS/GAMES).
    pub author_raw: String,
    /// Lowercased copies for case-insensitive search.
    stem_lower: String,
    author_lower: String,
}

#[derive(Debug, Default)]
pub struct HvscBrowser {
    root: Option<PathBuf>,
    category: HvscCategory,
    authors: Vec<HvscAuthor>,
    /// True if `authors` reflects the current `(root, category)` tuple.
    /// Cleared by `set_root` / `set_category`; refilled by
    /// `load_authors_if_needed`.
    authors_loaded: bool,
    selected_author: Option<usize>,
    tunes: Vec<HvscTune>,
    search: String,
    /// Flat tune index for global search. Lazily populated the first
    /// time the user types into the search box. Reset whenever
    /// `(root, category)` changes.
    flat_index: Vec<HvscIndexEntry>,
    flat_index_loaded: bool,
}

impl Default for HvscCategory {
    fn default() -> Self {
        HvscCategory::Musicians
    }
}

impl HvscBrowser {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Default::default()
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn category(&self) -> HvscCategory {
        self.category
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn authors(&self) -> &[HvscAuthor] {
        &self.authors
    }

    pub fn tunes(&self) -> &[HvscTune] {
        &self.tunes
    }

    pub fn selected_author(&self) -> Option<&HvscAuthor> {
        self.selected_author.and_then(|i| self.authors.get(i))
    }

    pub fn selected_author_idx(&self) -> Option<usize> {
        self.selected_author
    }

    /// Update the root (typically after a successful HVSC sync, or when
    /// `config.hvsc_root` changes in Settings). Invalidates caches.
    pub fn set_root(&mut self, root: Option<PathBuf>) {
        if self.root != root {
            self.root = root;
            self.authors.clear();
            self.tunes.clear();
            self.selected_author = None;
            self.authors_loaded = false;
            self.flat_index.clear();
            self.flat_index_loaded = false;
        }
    }

    pub fn set_category(&mut self, category: HvscCategory) {
        if self.category != category {
            self.category = category;
            self.authors.clear();
            self.tunes.clear();
            self.selected_author = None;
            self.authors_loaded = false;
            self.flat_index.clear();
            self.flat_index_loaded = false;
        }
    }

    pub fn set_search(&mut self, query: String) {
        self.search = query;
    }

    pub fn flat_index(&self) -> &[HvscIndexEntry] {
        &self.flat_index
    }

    pub fn flat_index_loaded(&self) -> bool {
        self.flat_index_loaded
    }

    /// Walk every `.sid`/`.mus` file under the current category and record
    /// (path, stem, author/section dir) per file. No SID header parse —
    /// just `walkdir` + string. Cheap (~50 ms for ~10k files on SSD).
    ///
    /// Called lazily the first time the user types something into the
    /// search box, so the cost doesn't hit users who only browse by
    /// author. Returns the size of the built index.
    pub fn build_flat_index_if_needed(&mut self) -> usize {
        if self.flat_index_loaded {
            return self.flat_index.len();
        }
        self.flat_index.clear();
        let root = match &self.root {
            Some(r) => r.clone(),
            None => {
                self.flat_index_loaded = true;
                return 0;
            }
        };
        let category_dir = root.join(self.category.dir_name());
        if !category_dir.is_dir() {
            self.flat_index_loaded = true;
            return 0;
        }
        for dirent in WalkDir::new(&category_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            let p = dirent.path();
            if !p.is_file() || !is_sid_or_mus(p) {
                continue;
            }
            let stem = match p.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Author / section folder = direct parent of the file's
            // grandparent for MUSICIANS (.../<letter>/<Author>/file.sid),
            // direct parent for DEMOS/GAMES (.../<range>/file.sid).
            // We pull whichever parent sits directly under the category
            // dir — works for both layouts because we already iterate
            // category_dir as the walk root.
            let author_raw = parent_under_category(p, &category_dir)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let stem_lower = stem.to_ascii_lowercase();
            let author_lower = author_raw.to_ascii_lowercase();
            self.flat_index.push(HvscIndexEntry {
                path: p.to_path_buf(),
                stem,
                author_raw,
                stem_lower,
                author_lower,
            });
        }
        self.flat_index
            .sort_by(|a, b| a.stem_lower.cmp(&b.stem_lower));
        self.flat_index_loaded = true;
        self.flat_index.len()
    }

    /// Indices into `flat_index` matching the current search query against
    /// either the file stem or the author/section folder name. Capped at
    /// 500 hits so the UI doesn't render an unbounded list while the user
    /// types one letter at a time.
    pub fn filtered_flat(&self) -> Vec<usize> {
        if self.search.trim().is_empty() {
            return Vec::new();
        }
        let needle = self.search.to_ascii_lowercase();
        let mut out = Vec::new();
        for (i, e) in self.flat_index.iter().enumerate() {
            if e.stem_lower.contains(&needle) || e.author_lower.contains(&needle) {
                out.push(i);
                if out.len() >= 500 {
                    break;
                }
            }
        }
        out
    }

    /// Lazy-load a single `PlaylistEntry` for a flat-index hit (used when
    /// the user clicks Play/Add on a global search result). Applies the
    /// songlength DB inline; STIL ✓ is determined by the caller via
    /// `lookup_by_hvsc_path` if it cares.
    pub fn realise_flat(
        &self,
        idx: usize,
        songlength: Option<&SonglengthDb>,
    ) -> Option<PlaylistEntry> {
        let path = &self.flat_index.get(idx)?.path;
        let entry = PlaylistEntry::from_path(path).ok()?;
        Some(apply_songlength(entry, songlength))
    }

    /// True when no `hvsc_root` is configured — the UI shows the empty
    /// state with a "Sync HVSC first" hint.
    pub fn is_empty_state(&self) -> bool {
        self.root.is_none()
    }

    /// Lazily populate `authors` for the current `(root, category)`.
    /// No-op if already loaded. Returns an error string the UI can show
    /// if the category folder doesn't exist (e.g. sync was partial).
    ///
    /// HVSC has two on-disk layouts:
    ///   - **MUSICIANS** (two levels): `<root>/MUSICIANS/<letter>/<Author>/...`
    ///     → each `<Author>` directory becomes one entry in `authors`.
    ///   - **DEMOS / GAMES** (one level): `<root>/<CAT>/<range>/*.sid`
    ///     → each `<range>` directory becomes one entry (no per-author
    ///     subfolder exists). Ranges are labels like `0-9`, `A-F`,
    ///     `Commodore`, etc.
    /// The right-column tune walk in `select_author` handles both shapes
    /// uniformly via `walkdir`.
    pub fn load_authors_if_needed(&mut self) -> Result<(), String> {
        if self.authors_loaded {
            return Ok(());
        }
        self.authors.clear();
        let root = match &self.root {
            Some(r) => r.clone(),
            None => {
                self.authors_loaded = true;
                return Ok(());
            }
        };
        let category_dir = root.join(self.category.dir_name());
        if !category_dir.is_dir() {
            self.authors_loaded = true;
            return Err(format!(
                "{}/ not found under {} — re-sync HVSC?",
                self.category.dir_name(),
                root.display()
            ));
        }
        let top_iter = match std::fs::read_dir(&category_dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.authors_loaded = true;
                return Err(format!("cannot read {}: {e}", category_dir.display()));
            }
        };
        let mut top_dirs: Vec<PathBuf> = top_iter
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        top_dirs.sort();

        match self.category {
            HvscCategory::Musicians => {
                // Two-level walk: <letter>/<Author>/
                for letter_path in top_dirs {
                    let letter = first_letter(&letter_path);
                    let inner = match std::fs::read_dir(&letter_path) {
                        Ok(rd) => rd,
                        Err(_) => continue,
                    };
                    let mut author_paths: Vec<PathBuf> = inner
                        .filter_map(|r| r.ok())
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect();
                    author_paths.sort();
                    for author_path in author_paths {
                        let raw_name = match author_path.file_name().and_then(|s| s.to_str()) {
                            Some(n) => n.to_string(),
                            None => continue,
                        };
                        let display_name = derive_display_name(&raw_name);
                        self.authors.push(HvscAuthor {
                            raw_name,
                            display_name,
                            letter,
                            path: author_path,
                        });
                    }
                }
            }
            HvscCategory::Demos | HvscCategory::Games => {
                // One-level walk: each top-level dir IS the browsable unit.
                // Range labels (0-9, A-F, Commodore, ...) are already
                // display-ready — no name swap.
                for range_path in top_dirs {
                    let raw_name = match range_path.file_name().and_then(|s| s.to_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    let letter = first_letter(&range_path);
                    self.authors.push(HvscAuthor {
                        display_name: raw_name.clone(),
                        letter,
                        raw_name,
                        path: range_path,
                    });
                }
            }
        }
        self.authors_loaded = true;
        Ok(())
    }

    /// Walk the selected author's folder, build a `HvscTune` per `.sid`/
    /// `.mus` file. Applies songlength durations and STIL ✓ markers from
    /// the provided DBs (both optional). Typically completes in tens of ms.
    pub fn select_author(
        &mut self,
        idx: usize,
        stil: Option<&StilDb>,
        songlength: Option<&SonglengthDb>,
    ) {
        self.selected_author = Some(idx);
        self.tunes.clear();
        let author = match self.authors.get(idx) {
            Some(a) => a.clone(),
            None => return,
        };
        for dirent in WalkDir::new(&author.path)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            let p = dirent.path();
            if !p.is_file() {
                continue;
            }
            if !is_sid_or_mus(p) {
                continue;
            }
            let entry = match PlaylistEntry::from_path(p) {
                Ok(e) => e,
                Err(_) => continue,
            };
            // Apply songlength duration if available (subtune 0 = song 1).
            let entry = apply_songlength(entry, songlength);
            let has_stil = stil_has_entry(&author.path, p, stil, self.root.as_deref());
            self.tunes.push(HvscTune { entry, has_stil });
        }
        // Stable, predictable order: by file path.
        self.tunes.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    }

    /// Indices into `authors` matching the search query (case-insensitive
    /// substring against both raw and display name).
    pub fn filtered_authors(&self) -> Vec<usize> {
        if self.search.trim().is_empty() {
            return (0..self.authors.len()).collect();
        }
        let needle = self.search.to_ascii_lowercase();
        self.authors
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.raw_name.to_ascii_lowercase().contains(&needle)
                    || a.display_name.to_ascii_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices into `tunes` matching the search query — title, author,
    /// released, or filename stem.
    pub fn filtered_tunes(&self) -> Vec<usize> {
        if self.search.trim().is_empty() {
            return (0..self.tunes.len()).collect();
        }
        let needle = self.search.to_ascii_lowercase();
        self.tunes
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let e = &t.entry;
                e.title.to_ascii_lowercase().contains(&needle)
                    || e.author.to_ascii_lowercase().contains(&needle)
                    || e.released.to_ascii_lowercase().contains(&needle)
                    || e.path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_ascii_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// Name of the immediate parent directory of `file`. For HVSC:
/// MUSICIANS/H/Hubbard_Rob/Commando.sid → "Hubbard_Rob"
/// DEMOS/0-9/12345.sid                  → "0-9"
/// Used as the "author / section" attribution in the flat search index.
/// `_category_dir` is unused but kept for future per-category logic.
fn parent_under_category<'a>(file: &'a Path, _category_dir: &Path) -> Option<&'a str> {
    file.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
}

fn first_letter(path: &Path) -> char {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?')
}

fn is_sid_or_mus(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "sid" | "psid" | "rsid" | "mus"
            )
        })
        .unwrap_or(false)
}

fn apply_songlength(mut entry: PlaylistEntry, db: Option<&SonglengthDb>) -> PlaylistEntry {
    if entry.duration_secs.is_some() {
        return entry;
    }
    let db = match db {
        Some(d) => d,
        None => return entry,
    };
    let md5 = match &entry.md5 {
        Some(m) => m.clone(),
        None => return entry,
    };
    let song0 = entry.selected_song.saturating_sub(1) as usize;
    if let Some(secs) = db.lookup(&md5, song0) {
        entry.duration_secs = Some(secs);
    }
    entry
}

fn stil_has_entry(
    author_dir: &Path,
    tune_path: &Path,
    stil: Option<&StilDb>,
    hvsc_root: Option<&Path>,
) -> bool {
    let stil = match stil {
        Some(s) => s,
        None => return false,
    };
    // Build the HVSC-relative path: strip hvsc_root prefix if known,
    // otherwise fall back to the author-dir-relative form prefixed
    // with the discovered category/letter chain.
    let hvsc_rel = match hvsc_root.and_then(|r| tune_path.strip_prefix(r).ok()) {
        Some(rel) => format!("/{}", rel.to_string_lossy()),
        None => {
            // No root → can't form an HVSC path. Use author_dir as a hint.
            let parent = author_dir.parent().unwrap_or(author_dir);
            let stripped = tune_path
                .strip_prefix(parent)
                .unwrap_or(tune_path)
                .to_string_lossy()
                .into_owned();
            format!("/{stripped}")
        }
    };
    stil.lookup_by_hvsc_path(&hvsc_rel).is_some()
}

/// `Hubbard_Rob` → `Hubbard, Rob`. `Robotron_4000` → `Robotron 4000`.
/// Heuristic: split on `_`; if exactly two segments and the second segment
/// starts with an uppercase ASCII letter, treat as LastName_FirstName.
fn derive_display_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('_').collect();
    if parts.len() == 2
        && parts[1]
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase())
            .unwrap_or(false)
    {
        format!("{}, {}", parts[0], parts[1])
    } else {
        raw.replace('_', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_swaps_last_first_on_underscore() {
        assert_eq!(derive_display_name("Hubbard_Rob"), "Hubbard, Rob");
        assert_eq!(derive_display_name("Hannula_Antti"), "Hannula, Antti");
    }

    #[test]
    fn display_name_keeps_plain_underscores() {
        // Second segment doesn't start with uppercase → not a name swap.
        assert_eq!(derive_display_name("Robotron_4000"), "Robotron 4000");
        assert_eq!(
            derive_display_name("Some_band_collective"),
            "Some band collective"
        );
    }

    #[test]
    fn display_name_passes_through_single_word() {
        assert_eq!(derive_display_name("Zyron"), "Zyron");
    }
}

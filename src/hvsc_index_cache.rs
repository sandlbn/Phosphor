// On-disk cache for the HVSC global-search index.
//
// Building the index walks every .sid/.mus under a category and reads each
// file to parse its SID header, so a cold build costs seconds and was paid
// again on every launch. This stores the result and validates it cheaply on
// load.
//
// The bias throughout is toward discarding: serving a stale title silently is
// worse than rebuilding. Anything unexpected returns `None` and the caller
// falls back to the walk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hvsc_browser::{HvscCategory, HvscIndexEntry};

/// Bump on ANY change to the stored shape. An older cache is then discarded
/// rather than misread.
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct CacheHeader {
    pub schema_version: u32,
    pub root: String,
    pub category: String,
    /// HVSC release the tree was on when this was built. Songlength and STIL
    /// data move with the release, so this covers the derived fields too.
    pub hvsc_version: Option<u32>,
    /// Number of directories under the category, and the newest directory
    /// mtime. Both POSIX and NTFS bump a directory's mtime when a file inside
    /// is added, removed or renamed, so this catches structural edits while
    /// stat-ing ~2k directories instead of ~60k files.
    pub dir_count: u64,
    pub dir_mtime_max: u64,
    pub entry_count: usize,
}

/// Path relative to `<root>/<CATEGORY>/`, so the cache survives a moved HVSC
/// root. Stored as a string rather than `PathBuf` because serde's `PathBuf`
/// impl errors on non-UTF-8 paths.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    rel: String,
    title: String,
    released: String,
    author: String,
    songs: u16,
    duration_secs: Option<u32>,
    has_stil: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    header: CacheHeader,
    entries: Vec<CacheEntry>,
}

/// Does a stored header still describe the tree we are about to search?
pub fn cache_is_valid(stored: &CacheHeader, expected: &CacheHeader) -> bool {
    stored == expected
}

fn cache_path(category: HvscCategory) -> Option<PathBuf> {
    crate::config::config_dir().map(|d| {
        d.join("hvsc_index")
            .join(format!("{}.json", category.dir_name()))
    })
}

/// Count directories under `<root>/<category>` and take the newest mtime.
/// Directories only — see `CacheHeader::dir_count`.
fn fingerprint(root: &Path, category: HvscCategory) -> (u64, u64) {
    let base = root.join(category.dir_name());
    let mut count = 0u64;
    let mut newest = 0u64;
    for entry in walkdir::WalkDir::new(&base)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir())
    {
        count += 1;
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                    newest = newest.max(dur.as_secs());
                }
            }
        }
    }
    (count, newest)
}

fn build_header(
    root: &Path,
    category: HvscCategory,
    hvsc_version: Option<u32>,
    entry_count: usize,
) -> CacheHeader {
    let (dir_count, dir_mtime_max) = fingerprint(root, category);
    CacheHeader {
        schema_version: SCHEMA_VERSION,
        root: root.to_string_lossy().into_owned(),
        category: category.dir_name().to_string(),
        hvsc_version,
        dir_count,
        dir_mtime_max,
        entry_count,
    }
}

/// Load a cached index, or `None` if there isn't a usable one.
pub fn load(
    root: &Path,
    category: HvscCategory,
    hvsc_version: Option<u32>,
) -> Option<Vec<HvscIndexEntry>> {
    let path = cache_path(category)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let file: CacheFile = serde_json::from_str(&text).ok()?;

    let expected = build_header(root, category, hvsc_version, file.entries.len());
    if !cache_is_valid(&file.header, &expected) {
        eprintln!(
            "[hvsc-index] cache for {} is stale — rebuilding",
            category.dir_name()
        );
        return None;
    }

    let base = root.join(category.dir_name());
    let entries: Vec<HvscIndexEntry> = file
        .entries
        .into_iter()
        .map(|e| {
            let path = base.join(&e.rel);
            HvscIndexEntry::rehydrate(
                path,
                e.title,
                e.released,
                e.author,
                e.songs,
                e.duration_secs,
                e.has_stil,
            )
        })
        .collect();

    // Cheap sample check: a handful of paths that no longer exist means the
    // tree moved on in a way the directory fingerprint missed.
    let step = (entries.len() / 8).max(1);
    if entries
        .iter()
        .step_by(step)
        .take(8)
        .any(|e| !e.path.exists())
    {
        eprintln!("[hvsc-index] cached paths no longer resolve — rebuilding");
        return None;
    }

    eprintln!(
        "[hvsc-index] loaded {} entries for {} from cache",
        entries.len(),
        category.dir_name()
    );
    Some(entries)
}

/// Persist a freshly built index. Call only after a *complete* walk — a
/// partial index written here would look valid on the next load.
pub fn save(
    root: &Path,
    category: HvscCategory,
    hvsc_version: Option<u32>,
    entries: &[HvscIndexEntry],
) {
    let Some(path) = cache_path(category) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let base = root.join(category.dir_name());
    let file = CacheFile {
        header: build_header(root, category, hvsc_version, entries.len()),
        entries: entries
            .iter()
            .map(|e| CacheEntry {
                rel: e
                    .path
                    .strip_prefix(&base)
                    .unwrap_or(&e.path)
                    .to_string_lossy()
                    .replace('\\', "/"),
                title: e.title.clone(),
                released: e.released.clone(),
                author: e.author_raw.clone(),
                songs: e.songs,
                duration_secs: e.duration_secs,
                has_stil: e.has_stil,
            })
            .collect(),
    };
    let Ok(text) = serde_json::to_string(&file) else {
        return;
    };
    // Write-then-rename: an interrupted write must not leave a truncated file
    // that still parses as a shorter, valid index.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Drop every cached category. Used after a sync, when the tree has changed
/// underneath us wholesale.
pub fn clear_all() {
    if let Some(dir) = crate::config::config_dir().map(|d| d.join("hvsc_index")) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> CacheHeader {
        CacheHeader {
            schema_version: SCHEMA_VERSION,
            root: "/hvsc".into(),
            category: "MUSICIANS".into(),
            hvsc_version: Some(85),
            dir_count: 2000,
            dir_mtime_max: 1_700_000_000,
            entry_count: 60_000,
        }
    }

    #[test]
    fn identical_headers_are_valid() {
        assert!(cache_is_valid(&header(), &header()));
    }

    #[test]
    fn every_field_invalidates() {
        // Each of these is a way the tree can change under a cached index;
        // all of them must force a rebuild rather than serve stale titles.
        let cases: Vec<(&str, Box<dyn Fn(&mut CacheHeader)>)> = vec![
            (
                "schema",
                Box::new(|h: &mut CacheHeader| h.schema_version += 1),
            ),
            (
                "root",
                Box::new(|h: &mut CacheHeader| h.root = "/other".into()),
            ),
            (
                "category",
                Box::new(|h: &mut CacheHeader| h.category = "DEMOS".into()),
            ),
            (
                "hvsc_version",
                Box::new(|h: &mut CacheHeader| h.hvsc_version = Some(86)),
            ),
            (
                "dir_count",
                Box::new(|h: &mut CacheHeader| h.dir_count += 1),
            ),
            (
                "dir_mtime",
                Box::new(|h: &mut CacheHeader| h.dir_mtime_max += 1),
            ),
            (
                "entry_count",
                Box::new(|h: &mut CacheHeader| h.entry_count += 1),
            ),
        ];
        for (name, mutate) in cases {
            let mut stored = header();
            mutate(&mut stored);
            assert!(
                !cache_is_valid(&stored, &header()),
                "{name} change must invalidate the cache"
            );
        }
    }

    #[test]
    fn unsynced_tree_with_no_version_still_matches_itself() {
        // No STIL.txt to read a version from is normal for a hand-assembled
        // tree; it must not mean "never cacheable".
        let mut h = header();
        h.hvsc_version = None;
        assert!(cache_is_valid(&h, &h));
    }

    #[test]
    fn round_trips_through_json() {
        let file = CacheFile {
            header: header(),
            entries: vec![CacheEntry {
                rel: "H/Hubbard_Rob/Commando.sid".into(),
                title: "Commando".into(),
                released: "1985 Elite".into(),
                author: "Hubbard_Rob".into(),
                songs: 3,
                duration_secs: Some(90),
                has_stil: true,
            }],
        };
        let text = serde_json::to_string(&file).unwrap();
        let back: CacheFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.header, file.header);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].rel, "H/Hubbard_Rob/Commando.sid");
        assert_eq!(back.entries[0].duration_secs, Some(90));
    }

    #[test]
    fn truncated_json_is_an_error_not_a_short_index() {
        let text = serde_json::to_string(&CacheFile {
            header: header(),
            entries: vec![],
        })
        .unwrap();
        let truncated = &text[..text.len() / 2];
        assert!(serde_json::from_str::<CacheFile>(truncated).is_err());
    }
}

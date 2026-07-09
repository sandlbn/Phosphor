// Favourites (♥ hearted tracks) — persisted as JSON at `<config_dir>/
// favorites.json` with enough metadata to resolve back to a playable
// file across sessions, HVSC-root moves, and file relocations.
//
// See docs/plan for the design; short version:
//
//   * `Vec<FavoriteEntry>` with `{md5, path, title, author, released,
//     added_at}` — like `recently_played.rs`, so removing a track from
//     the current playlist doesn't lose the favourite.
//   * `HashSet<String>` derived index for O(1) `is_favorite(md5)`
//     queries. Kept in sync with `entries` after every mutation.
//   * Migration from the legacy MD5-only `favorites.txt` runs the
//     first time the new module loads; the old file is renamed to
//     `favorites.txt.bak` so we don't re-migrate.
//   * `resolve()` fallback chain: stored path → HVSC md5→path lookup
//     via `SonglengthDb` → give up. On success via the fallback, the
//     healed path is written back so subsequent loads are fast.
//   * M3U export / import for share-with-a-friend workflows.
//
// Public API mirrors the old `config::FavoritesDb` for backward
// compatibility with the ~8 call sites in main.rs — the `.hashes`
// field is still present (as the derived index), and `is_favorite` /
// `count` keep their signatures.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::playlist::{PlaylistEntry, SonglengthDb};

// ─────────────────────────────────────────────────────────────────────
//  Entry
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoriteEntry {
    /// Lowercase hex MD5 (32 chars). Primary key.
    pub md5: String,
    /// SID header title. May be empty for legacy migrated entries.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub released: String,
    /// Last-known absolute path. `None` for legacy entries migrated
    /// from `favorites.txt`. The `resolve()` fallback chain heals this
    /// on the first successful play or Load-Liked.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Unix timestamp (seconds) when the user first hearted this
    /// track. 0 for legacy migrated entries.
    #[serde(default)]
    pub added_at: u64,
}

impl FavoriteEntry {
    fn from_playlist_entry(e: &PlaylistEntry) -> Option<Self> {
        let md5 = e.md5.as_ref()?.to_lowercase();
        Some(Self {
            md5,
            title: e.title.clone(),
            author: e.author.clone(),
            released: e.released.clone(),
            path: Some(e.path.clone()),
            added_at: now_secs(),
        })
    }

    /// True when the entry has no metadata at all — usually a leftover
    /// from the legacy `favorites.txt` migration where the user
    /// hearted a track long ago but never re-played it, so we never
    /// captured title / author / path. Ghost entries don't count
    /// toward the visible ❤ badge and are the first candidates for
    /// automatic pruning during `Load Liked`.
    pub fn is_ghost(&self) -> bool {
        self.path.is_none() && self.title.is_empty() && self.author.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Database
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FavoritesDb {
    /// Ordered newest-first (add-time).
    pub entries: Vec<FavoriteEntry>,
    /// Derived MD5 index for O(1) `is_favorite`. Rebuilt on load and
    /// after every mutation. Public so a handful of legacy call sites
    /// (which read `favorites.hashes.contains(...)` directly) keep
    /// working without a big refactor.
    #[serde(skip)]
    pub hashes: HashSet<String>,
}

impl FavoritesDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `favorites.json` if it exists. Migrate from the
    /// legacy `favorites.txt` if only the old file is present.
    pub fn load() -> Self {
        let json = json_path();
        let txt = txt_path();

        // 1) Preferred: JSON format.
        if let Some(ref p) = json {
            if p.exists() {
                match fs::read_to_string(p) {
                    Ok(text) => match serde_json::from_str::<Self>(&text) {
                        Ok(mut db) => {
                            db.rebuild_index();
                            eprintln!("[phosphor] Loaded {} favorites", db.count());
                            return db;
                        }
                        Err(e) => eprintln!("[phosphor] favorites.json parse: {e}"),
                    },
                    Err(e) => eprintln!("[phosphor] favorites.json read: {e}"),
                }
            }
        }

        // 2) Legacy: MD5-only text file. Migrate + rename in one shot.
        if let Some(ref p) = txt {
            if p.exists() {
                match fs::read_to_string(p) {
                    Ok(content) => {
                        let mut db = Self::new();
                        for line in content.lines() {
                            let md5 = line.trim().to_lowercase();
                            if md5.len() == 32 && !db.hashes.contains(&md5) {
                                db.entries.push(FavoriteEntry {
                                    md5: md5.clone(),
                                    title: String::new(),
                                    author: String::new(),
                                    released: String::new(),
                                    path: None,
                                    added_at: 0,
                                });
                                db.hashes.insert(md5);
                            }
                        }
                        eprintln!(
                            "[phosphor] Migrated {} favorites from legacy favorites.txt",
                            db.count()
                        );
                        db.save();
                        // Rename the .txt so we don't re-migrate on
                        // next launch. Best-effort — a failure here
                        // just means we'll re-migrate next time.
                        let bak = p.with_extension("txt.bak");
                        let _ = fs::rename(p, &bak);
                        return db;
                    }
                    Err(e) => eprintln!("[phosphor] favorites.txt read: {e}"),
                }
            }
        }

        Self::new()
    }

    /// Persist as pretty JSON.
    pub fn save(&self) {
        let path = match json_path() {
            Some(p) => p,
            None => return,
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = fs::write(&path, json) {
                    eprintln!("[phosphor] favorites.json write: {e}");
                }
            }
            Err(e) => eprintln!("[phosphor] favorites.json serialize: {e}"),
        }
    }

    /// Toggle a track by playlist entry. Returns true if it's now a
    /// favourite (was added), false if it was removed.
    ///
    /// This is the ergonomic entry point — call sites that have a
    /// `PlaylistEntry` in hand (which is all of them) pass it here.
    pub fn toggle(&mut self, entry: &PlaylistEntry) -> bool {
        let md5 = match entry.md5.as_ref() {
            Some(m) => m.to_lowercase(),
            None => return false,
        };
        if self.hashes.contains(&md5) {
            self.entries.retain(|e| e.md5 != md5);
            self.hashes.remove(&md5);
            false
        } else if let Some(fav) = FavoriteEntry::from_playlist_entry(entry) {
            self.hashes.insert(fav.md5.clone());
            self.entries.insert(0, fav); // newest-first
            true
        } else {
            false
        }
    }

    /// Remove by MD5. Returns whether the entry existed. Useful for
    /// remote paths that only have the hash.
    pub fn remove(&mut self, md5: &str) -> bool {
        let key = md5.to_lowercase();
        if self.hashes.remove(&key) {
            self.entries.retain(|e| e.md5 != key);
            true
        } else {
            false
        }
    }

    /// Add or overwrite an entry with full metadata. Used by import
    /// and by the "enrich on play" flow that heals path=None entries
    /// as they're re-encountered.
    pub fn upsert(&mut self, entry: &PlaylistEntry) {
        let fav = match FavoriteEntry::from_playlist_entry(entry) {
            Some(f) => f,
            None => return,
        };
        if let Some(existing) = self.entries.iter_mut().find(|e| e.md5 == fav.md5) {
            // Preserve added_at from the original; refresh everything
            // else so metadata drift (retagged files) heals silently.
            existing.title = fav.title;
            existing.author = fav.author;
            existing.released = fav.released;
            existing.path = fav.path;
        } else {
            self.hashes.insert(fav.md5.clone());
            self.entries.insert(0, fav);
        }
    }

    pub fn is_favorite(&self, md5: &str) -> bool {
        self.hashes.contains(&md5.to_lowercase())
    }

    /// User-visible favourite count — excludes ghost entries left
    /// over from the legacy migration. This is what the ❤ badge in
    /// the search bar should show; use `total_len` for internal
    /// bookkeeping when you need every entry.
    pub fn count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_ghost()).count()
    }

    /// Raw entry count including ghosts. Used by iteration paths that
    /// need to visit every persisted row (Load Liked, M3U export).
    pub fn total_len(&self) -> usize {
        self.entries.len()
    }

    /// Drop entries at the given indices in one pass. Used by Load
    /// Liked to silently prune ghost entries that failed to resolve —
    /// the user recognises the ♥ 2 count as "my 2 tracks" and doesn't
    /// need to see the invisible leftover row.
    pub fn remove_indices(&mut self, mut indices: Vec<usize>) {
        // Sort descending so removes don't shift later indices.
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        for i in indices {
            if i < self.entries.len() {
                let removed = self.entries.remove(i);
                self.hashes.remove(&removed.md5);
            }
        }
    }

    /// Resolve one favourite to a playable path. Two-step fallback:
    /// stored path if it still exists → HVSC MD5→path lookup via the
    /// songlength DB. On a successful fallback the healed path is
    /// written back so the caller can persist.
    ///
    /// Returns the resolved path or `None` if both steps fail.
    pub fn resolve(
        &mut self,
        entry_idx: usize,
        songlength_db: Option<&SonglengthDb>,
        hvsc_root: Option<&Path>,
    ) -> Option<PathBuf> {
        let entry = self.entries.get(entry_idx)?;

        // 1) Stored path still valid?
        if let Some(ref p) = entry.path {
            if p.exists() {
                return Some(p.clone());
            }
        }

        // 2) HVSC songlength lookup.
        let md5 = entry.md5.clone();
        if let (Some(db), Some(root)) = (songlength_db, hvsc_root) {
            if let Some(rel) = db.md5_to_path.get(&md5) {
                let candidate = root.join(rel.trim_start_matches('/'));
                if candidate.exists() {
                    // Heal the stored path so next time step 1 wins.
                    if let Some(e) = self.entries.get_mut(entry_idx) {
                        e.path = Some(candidate.clone());
                    }
                    return Some(candidate);
                }
            }
        }

        None
    }

    /// Blank the stored path on any entry whose file no longer exists
    /// or whose cached path lives under a stale HVSC root. Called
    /// from `Message::RerootFavourites` on Settings change.
    ///
    /// Returns the number of entries touched.
    pub fn reroot(&mut self, previous_hvsc_root: Option<&Path>) -> usize {
        let mut touched = 0usize;
        for e in &mut self.entries {
            let stale = match &e.path {
                Some(p) => {
                    let inside_old = previous_hvsc_root
                        .map(|r| p.starts_with(r))
                        .unwrap_or(false);
                    !p.exists() || inside_old
                }
                None => false,
            };
            if stale {
                e.path = None;
                touched += 1;
            }
        }
        touched
    }

    /// Render the current favourites as an M3U playlist. Entries that
    /// don't currently resolve are still written with their last-known
    /// path (their recipient's HVSC install might heal them).
    pub fn export_m3u(&self) -> String {
        let mut out = String::from("#EXTM3U\n");
        for e in &self.entries {
            let display = if !e.author.is_empty() && !e.title.is_empty() {
                format!("{} - {}", e.author, e.title)
            } else if !e.title.is_empty() {
                e.title.clone()
            } else {
                e.md5.clone()
            };
            out.push_str(&format!("#EXTINF:-1,{display}\n"));
            if let Some(ref p) = e.path {
                out.push_str(&format!("{}\n", p.display()));
            } else {
                // Unresolved entry — emit a placeholder path comment
                // so the recipient can hand-edit if needed.
                out.push_str(&format!("# md5-only:{}\n", e.md5));
            }
        }
        out
    }

    /// Import an M3U into the favourites DB. Parses each track path
    /// via `PlaylistEntry::from_path` to fill in MD5 + metadata.
    /// Returns `(new, existing, missing)` counts for the status bar.
    pub fn import_m3u(&mut self, m3u: &str) -> (usize, usize, usize) {
        let mut new_count = 0usize;
        let mut existing = 0usize;
        let mut missing = 0usize;
        for line in m3u.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let p = PathBuf::from(line);
            let entry = match PlaylistEntry::from_path(&p) {
                Ok(e) => e,
                Err(_) => {
                    missing += 1;
                    continue;
                }
            };
            match entry.md5.as_ref() {
                Some(md5) if self.hashes.contains(&md5.to_lowercase()) => existing += 1,
                Some(_) => {
                    self.upsert(&entry);
                    new_count += 1;
                }
                None => missing += 1,
            }
        }
        (new_count, existing, missing)
    }

    /// Rebuild the derived `hashes` index from `entries`. Called on
    /// load and after direct mutations.
    fn rebuild_index(&mut self) {
        self.hashes = self.entries.iter().map(|e| e.md5.clone()).collect();
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────

fn json_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("favorites.json"))
}

fn txt_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("favorites.txt"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Per-test temp dir. Tests can run in parallel, so plain
    // `phosphor-fav-test-<now>` collides on same-second launches;
    // include a tag + process id + timestamp for uniqueness.
    fn unique_tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "phosphor-fav-test-{}-{}-{}",
            tag,
            std::process::id(),
            now_secs()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn playlist_entry(md5: &str, path: &str) -> PlaylistEntry {
        PlaylistEntry {
            path: PathBuf::from(path),
            title: format!("Title-{md5}"),
            author: format!("Author-{md5}"),
            released: "1985 Test".to_string(),
            songs: 1,
            selected_song: 1,
            is_pal: true,
            num_sids: 1,
            is_rsid: false,
            md5: Some(md5.to_string()),
            duration_secs: None,
            has_wds: false,
        }
    }

    fn hvsc_db(md5: &str, hvsc_rel: &str) -> SonglengthDb {
        let mut db = SonglengthDb::new();
        db.entries.insert(md5.to_string(), vec![120]);
        db.md5_to_path.insert(md5.to_string(), hvsc_rel.to_string());
        db
    }

    #[test]
    fn migration_from_txt_preserves_all_md5s() {
        // Simulate the migration path in-memory: build a legacy
        // hashset (what the old load() produced), wrap each into a
        // FavoriteEntry, verify the new DB has the same set.
        let legacy: Vec<&str> = vec![
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "cccccccccccccccccccccccccccccccc",
            "dddddddddddddddddddddddddddddddd",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        ];
        let mut db = FavoritesDb::new();
        for md5 in &legacy {
            db.entries.push(FavoriteEntry {
                md5: md5.to_string(),
                title: String::new(),
                author: String::new(),
                released: String::new(),
                path: None,
                added_at: 0,
            });
        }
        db.rebuild_index();
        // Migrated rows have no metadata → they're ghosts and don't
        // count toward the user-visible `count()`; use `total_len()`
        // for the "everything persisted" assertion.
        assert_eq!(db.total_len(), 5);
        assert_eq!(db.count(), 0);
        for md5 in &legacy {
            assert!(db.is_favorite(md5));
            assert!(db
                .entries
                .iter()
                .find(|e| e.md5 == *md5)
                .unwrap()
                .is_ghost());
        }
    }

    #[test]
    fn resolve_prefers_stored_path_when_valid() {
        let tmp = unique_tmp("stored");
        let file = tmp.join("valid.sid");
        fs::write(&file, b"PSID\0\0").unwrap();
        let mut db = FavoritesDb::new();
        db.entries.push(FavoriteEntry {
            md5: "1".repeat(32),
            title: String::new(),
            author: String::new(),
            released: String::new(),
            path: Some(file.clone()),
            added_at: 0,
        });
        db.rebuild_index();
        let resolved = db.resolve(0, None, None);
        assert_eq!(resolved, Some(file));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_falls_back_to_hvsc_when_stored_missing() {
        let tmp = unique_tmp("fallback");
        let hvsc_dir = tmp.join("MUSICIANS").join("H").join("Foo");
        let _ = fs::create_dir_all(&hvsc_dir);
        let target = hvsc_dir.join("Bar.sid");
        fs::write(&target, b"PSID\0\0").unwrap();

        let md5 = "2".repeat(32);
        let mut db = FavoritesDb::new();
        db.entries.push(FavoriteEntry {
            md5: md5.clone(),
            title: String::new(),
            author: String::new(),
            released: String::new(),
            // Deliberately point at something that doesn't exist so
            // step 1 fails and step 2 kicks in.
            path: Some(PathBuf::from("/does/not/exist.sid")),
            added_at: 0,
        });
        db.rebuild_index();

        let hvsc = hvsc_db(&md5, "MUSICIANS/H/Foo/Bar.sid");
        let resolved = db.resolve(0, Some(&hvsc), Some(&tmp));
        assert_eq!(resolved, Some(target));
        // The healed path should have been written back so the next
        // resolve() takes step 1.
        assert!(db.entries[0].path.as_ref().unwrap().exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_returns_none_when_all_paths_fail() {
        let md5 = "3".repeat(32);
        let mut db = FavoritesDb::new();
        db.entries.push(FavoriteEntry {
            md5: md5.clone(),
            title: String::new(),
            author: String::new(),
            released: String::new(),
            path: Some(PathBuf::from("/does/not/exist.sid")),
            added_at: 0,
        });
        db.rebuild_index();
        // No songlength DB, no HVSC root — nothing to fall back to.
        assert_eq!(db.resolve(0, None, None), None);
    }

    #[test]
    fn m3u_export_import_round_trip() {
        // Round-trip needs real files on disk because import calls
        // `PlaylistEntry::from_path` which parses the SID header.
        // Use a real SID-like buffer with a synth PSID header.
        let tmp = unique_tmp("m3u");

        // Minimum-valid PSID for `load_sid` — header + a load-address
        // (2 bytes) + payload byte after the header, and `load_address`
        // in the header set to 0 so the loader reads it from the data
        // area. Anything shorter trips the "data_offset past end of
        // file" check in `sid_file::load_sid`.
        let synth_psid = |name: &str| -> Vec<u8> {
            let mut buf = vec![0u8; 0x7C + 3];
            buf[0..4].copy_from_slice(b"PSID");
            buf[5] = 0x02;
            buf[7] = 0x7C;
            let name_bytes = name.as_bytes();
            let n = name_bytes.len().min(31);
            buf[0x16..0x16 + n].copy_from_slice(&name_bytes[..n]);
            buf[0x7C] = 0x00; // load addr lo
            buf[0x7D] = 0x10; // load addr hi (= $1000)
            buf[0x7E] = 0xEA; // one byte of "code"
            buf
        };
        let a = tmp.join("A.sid");
        let b = tmp.join("B.sid");
        fs::write(&a, synth_psid("Track A")).unwrap();
        fs::write(&b, synth_psid("Track B")).unwrap();

        // Build initial DB via the import path.
        let m3u_input = format!("#EXTM3U\n{}\n{}\n", a.display(), b.display());
        let mut db = FavoritesDb::new();
        let (new_count, _existing, _missing) = db.import_m3u(&m3u_input);
        assert_eq!(new_count, 2);
        assert_eq!(db.count(), 2);
        let original_md5s: HashSet<String> = db.entries.iter().map(|e| e.md5.clone()).collect();

        // Round-trip: export → clear → import.
        let m3u_output = db.export_m3u();
        let mut db2 = FavoritesDb::new();
        let (new2, _, _) = db2.import_m3u(&m3u_output);
        assert_eq!(new2, 2);
        let after_md5s: HashSet<String> = db2.entries.iter().map(|e| e.md5.clone()).collect();
        assert_eq!(original_md5s, after_md5s);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reroot_blanks_paths_outside_new_root() {
        let old_root = PathBuf::from("/old/hvsc");
        let mut db = FavoritesDb::new();
        // One entry rooted under the old HVSC root — must be blanked.
        db.entries.push(FavoriteEntry {
            md5: "1".repeat(32),
            title: String::new(),
            author: String::new(),
            released: String::new(),
            path: Some(old_root.join("MUSICIANS/H/Foo.sid")),
            added_at: 0,
        });
        // One entry outside old root pointing at a non-existent file —
        // reroot should also blank this (file gone).
        db.entries.push(FavoriteEntry {
            md5: "2".repeat(32),
            title: String::new(),
            author: String::new(),
            released: String::new(),
            path: Some(PathBuf::from("/somewhere/else/gone.sid")),
            added_at: 0,
        });
        // Silence the unused-var lint.
        let _ = &old_root;
        db.rebuild_index();
        let touched = db.reroot(Some(&old_root));
        assert_eq!(touched, 2);
        assert!(db.entries.iter().all(|e| e.path.is_none()));
    }

    #[test]
    fn toggle_add_then_remove() {
        let entry = playlist_entry(&"a".repeat(32), "/tmp/a.sid");
        let mut db = FavoritesDb::new();
        assert!(db.toggle(&entry));
        assert!(db.is_favorite(&entry.md5.as_ref().unwrap()));
        assert!(!db.toggle(&entry));
        assert!(!db.is_favorite(&entry.md5.as_ref().unwrap()));
    }
}

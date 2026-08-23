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

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

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
    PublishedPlaylists,
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
            BrowserSource::PublishedPlaylists => "Playlists",
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            BrowserSource::LocalHvsc => "local",
            BrowserSource::Assembly64 => "a64",
            BrowserSource::PublishedPlaylists => "published",
        }
    }

    pub fn from_config_str(s: &str) -> Self {
        match s {
            "a64" => BrowserSource::Assembly64,
            "published" => BrowserSource::PublishedPlaylists,
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

impl HvscIndexEntry {
    /// Rebuild an entry from cached fields, recomputing the lowercase copies
    /// the sort comparators use rather than storing them on disk.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate(
        path: PathBuf,
        title: String,
        released: String,
        author_raw: String,
        songs: u16,
        duration_secs: Option<u32>,
        has_stil: bool,
    ) -> Self {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        Self {
            title_lower: title.to_ascii_lowercase(),
            author_lower: author_raw.to_ascii_lowercase(),
            path,
            stem,
            author_raw,
            title,
            released,
            songs,
            duration_secs,
            has_stil,
        }
    }
}

/// Sortable columns in the tune table. One enum covers both list modes;
/// `Secondary` is "Author / section" in global results and "Released" per
/// author, which is why it is named for its position rather than its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HvscSortColumn {
    Title,
    Secondary,
    Subs,
    Len,
}

/// Flat-index row for global search. Built once per category — one entry
/// per `.sid`/`.mus` file. Enriched with SID-header + songlength + STIL
/// metadata so the global-hit list can show the same columns as the
/// per-author view (title / released / #songs / duration / STIL ✓).
/// Building is off-thread — see `build_flat_index_worker`.
#[derive(Debug, Clone)]
pub struct HvscIndexEntry {
    pub path: PathBuf,
    /// File stem as displayed (e.g. `Commando`).
    pub stem: String,
    /// Author / section folder name as it appears on disk
    /// (`Hubbard_Rob` for MUSICIANS, `0-9` for DEMOS/GAMES).
    pub author_raw: String,
    /// SID header title. Falls back to the stem when the header is empty.
    pub title: String,
    pub released: String,
    pub songs: u16,
    pub duration_secs: Option<u32>,
    pub has_stil: bool,
    /// Lowercased copies for case-insensitive search.
    author_lower: String,
    title_lower: String,
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
    /// True while a background `build_flat_index_worker` is in flight.
    /// UI shows an "Indexing tunes…" placeholder in the right pane.
    flat_index_building: bool,
    /// Bumped on every `(root, category)` invalidation. The background
    /// worker's completion carries the version it was started with, so a
    /// stale result (from a category the user has since switched away
    /// from) can be discarded without polluting the current view.
    flat_index_version: u64,
    /// When true and an author is selected, the search box filters
    /// within that author's tunes instead of falling into the global
    /// flat-index view. Per-session; not persisted.
    search_scope_this_author: bool,
    /// Filters the author column only. Separate from `search` so typing a
    /// tune name no longer empties the author list.
    author_filter: String,
    /// Selected tune, keyed by path rather than index: sorting reorders
    /// indices, so an index would silently point at a different tune.
    selected_tune: Option<PathBuf>,
    /// Row under the cursor. `mouse_area` reports enter/exit but does no
    /// styling of its own, so hover is tracked here and drawn by the row.
    hovered_row: Option<usize>,
    /// Active sort, or `None` to keep relevance order for search results
    /// and path order for an author's tunes.
    sort: Option<(HvscSortColumn, crate::ui::SortDirection)>,
    /// Cached global-search result, recomputed only when the query, sort or
    /// index changes — `view()` runs ~30x/second and must not rescan 60k
    /// entries each time.
    cache: SearchCache,
    /// Same treatment for the author column: it was re-filtering all ~1850
    /// authors every frame, two `to_lowercase` allocations each.
    author_cache: Option<(String, usize, Vec<usize>)>,
}

/// Memoised global-search result. `key` is everything the result depends on.
#[derive(Debug, Default)]
struct SearchCache {
    key: Option<(
        String,
        Option<(HvscSortColumn, crate::ui::SortDirection)>,
        u64,
    )>,
    results: Vec<usize>,
    /// True number of matches, which may exceed `results.len()` once the
    /// display cap kicks in. Reported to the user verbatim.
    total_matches: usize,
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
            self.flat_index_building = false;
            self.flat_index_version = self.flat_index_version.wrapping_add(1);
            self.selected_tune = None;
            self.hovered_row = None;
            self.cache = SearchCache::default();
            self.author_cache = None;
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
            self.flat_index_building = false;
            self.flat_index_version = self.flat_index_version.wrapping_add(1);
            self.selected_tune = None;
            self.hovered_row = None;
            self.cache = SearchCache::default();
            self.author_cache = None;
        }
    }

    /// Drop the index so the next build starts from scratch. Backs the
    /// manual "rebuild" affordance for when the collection changed on disk.
    pub fn forget_flat_index(&mut self) {
        self.flat_index.clear();
        self.flat_index_loaded = false;
        self.flat_index_building = false;
        self.flat_index_version = self.flat_index_version.wrapping_add(1);
        self.cache = SearchCache::default();
    }

    pub fn flat_index_version(&self) -> u64 {
        self.flat_index_version
    }

    pub fn flat_index_building(&self) -> bool {
        self.flat_index_building
    }

    pub fn search_scope_this_author(&self) -> bool {
        self.search_scope_this_author
    }

    pub fn set_search_scope_this_author(&mut self, on: bool) {
        self.search_scope_this_author = on;
    }

    pub fn set_search(&mut self, query: String) {
        self.search = query;
    }

    pub fn author_filter(&self) -> &str {
        &self.author_filter
    }

    pub fn set_author_filter(&mut self, query: String) {
        self.author_filter = query;
    }

    pub fn selected_tune(&self) -> Option<&Path> {
        self.selected_tune.as_deref()
    }

    pub fn set_selected_tune(&mut self, path: Option<PathBuf>) {
        self.selected_tune = path;
    }

    pub fn hovered_row(&self) -> Option<usize> {
        self.hovered_row
    }

    pub fn set_hovered_row(&mut self, row: Option<usize>) {
        self.hovered_row = row;
    }

    pub fn sort(&self) -> Option<(HvscSortColumn, crate::ui::SortDirection)> {
        self.sort
    }

    /// Click a header: same column flips direction, a new column starts
    /// ascending, and a third click on the same column clears the sort and
    /// returns to relevance order.
    pub fn toggle_sort(&mut self, col: HvscSortColumn) {
        self.sort = match self.sort {
            Some((c, crate::ui::SortDirection::Ascending)) if c == col => {
                Some((col, crate::ui::SortDirection::Descending))
            }
            Some((c, crate::ui::SortDirection::Descending)) if c == col => None,
            _ => Some((col, crate::ui::SortDirection::Ascending)),
        };
    }

    pub fn flat_index(&self) -> &[HvscIndexEntry] {
        &self.flat_index
    }

    pub fn flat_index_loaded(&self) -> bool {
        self.flat_index_loaded
    }

    /// If the flat index is empty and no build is in flight, mark a
    /// build as pending and return a `(root, category, version)` handle
    /// the caller can hand off to a background task. `None` means the
    /// index is already loaded, already building, or there's no root
    /// configured — no work needed.
    ///
    /// The caller is expected to `Task::perform` `build_flat_index_worker`
    /// with the returned tuple + snapshots of the STIL and songlength
    /// DBs, then dispatch a `HvscFlatIndexReady` message that calls
    /// `install_flat_index` with the produced vec + the same version.
    pub fn begin_flat_index_build(&mut self) -> Option<(PathBuf, HvscCategory, u64)> {
        if self.flat_index_loaded || self.flat_index_building {
            return None;
        }
        let root = self.root.as_ref()?.clone();
        self.flat_index_building = true;
        Some((root, self.category, self.flat_index_version))
    }

    /// Install a completed flat index. Rejects the result if the version
    /// stamp doesn't match the current one (user changed category /
    /// root while the walk was in flight).
    pub fn install_flat_index(&mut self, version: u64, index: Vec<HvscIndexEntry>) {
        self.flat_index_building = false;
        if version != self.flat_index_version {
            // Stale — drop it and stay unloaded so the next keystroke
            // triggers a fresh build for the current (root, category).
            return;
        }
        self.flat_index = index;
        self.flat_index_loaded = true;
    }

    /// Cached global-search hits. Recomputed by `recompute_search` only when
    /// the query, sort or index changes — never from `view()`.
    pub fn flat_results(&self) -> &[usize] {
        &self.cache.results
    }

    /// True match count, which may exceed `flat_results().len()` once the
    /// display cap applies.
    pub fn flat_total_matches(&self) -> usize {
        self.cache.total_matches
    }

    /// Recompute the global-search cache if anything it depends on changed.
    /// Cheap to call every update tick; the key comparison is the fast path.
    pub fn recompute_search(&mut self) {
        let key = (
            self.search.trim().to_string(),
            self.sort,
            self.flat_index_version,
        );
        if self.cache.key.as_ref() == Some(&key) {
            return;
        }
        let query = key.0.clone();
        if query.is_empty() {
            self.cache = SearchCache {
                key: Some(key),
                results: Vec::new(),
                total_matches: 0,
            };
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = HvscQuery::parse(&query);
        let mut buf = Vec::new();

        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (i, e) in self.flat_index.iter().enumerate() {
            if let Some(score) = score_entry(&pattern, e, &mut matcher, &mut buf) {
                scored.push((score, i));
            }
        }
        let total = scored.len();

        match self.sort {
            // Explicit sort wins over relevance, but ranking still decides
            // which hits survive the cap, so a good match is never dropped
            // in favour of a weak one that merely sorts earlier.
            Some((col, dir)) => {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
                scored.truncate(MAX_FLAT_RESULTS);
                let index = &self.flat_index;
                scored.sort_by(|a, b| hvsc_sort_cmp(col, dir, &index[a.1], &index[b.1]));
            }
            None => {
                // Best score first; ties fall back to title for stability.
                let index = &self.flat_index;
                scored.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| index[a.1].title_lower.cmp(&index[b.1].title_lower))
                });
                scored.truncate(MAX_FLAT_RESULTS);
            }
        }

        self.cache = SearchCache {
            key: Some(key),
            results: scored.into_iter().map(|(_, i)| i).collect(),
            total_matches: total,
        };
    }
}

/// Most rows shown for a global search. Ranking runs over the whole index
/// first, so this caps what is *drawn*, not what is considered.
pub const MAX_FLAT_RESULTS: usize = 500;

/// A query compiled once per search: a contiguous-substring form and a
/// typo-tolerant fuzzy form.
///
/// Fuzzy matching alone is far too loose on a 20k-row index. Searching
/// "antti" subsequence-matches "F-a-n-t-as-t-i-c_Zool" and dozens like it,
/// which buried the actual `Hannula_Antti`. Scoring both ways and boosting
/// the substring hit keeps typo tolerance without letting it dominate.
pub struct HvscQuery {
    substring: Pattern,
    fuzzy: Pattern,
}

impl HvscQuery {
    pub fn parse(query: &str) -> Self {
        Self {
            substring: Pattern::new(
                query,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Substring,
            ),
            fuzzy: Pattern::new(
                query,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
            ),
        }
    }
}

/// How far a contiguous match outranks a merely-subsequence one. Large
/// enough that no amount of fuzzy score can lift noise above a real hit.
const SUBSTRING_BOOST: u32 = 10_000;

/// Relevance score for one index entry, or `None` if it does not match.
///
/// Fields are weighted so a title hit outranks an incidental match in
/// `released`, and a contiguous match anywhere beats a scattered one.
pub fn score_entry(
    query: &HvscQuery,
    entry: &HvscIndexEntry,
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
) -> Option<u32> {
    let mut best = None;
    for (text, weight) in [
        (entry.title.as_str(), 3u32),
        (entry.author_raw.as_str(), 3),
        (entry.stem.as_str(), 2),
        (entry.released.as_str(), 1),
    ] {
        if text.is_empty() {
            continue;
        }
        let haystack = Utf32Str::new(text, buf);
        let field = match query.substring.score(haystack, matcher) {
            Some(sub) => SUBSTRING_BOOST + sub,
            None => match query.fuzzy.score(haystack, matcher) {
                Some(f) => f,
                None => continue,
            },
        };
        let weighted = field * weight;
        best = Some(best.map_or(weighted, |b: u32| b.max(weighted)));
    }
    best
}

/// Same ordering as `hvsc_sort_cmp`, for the per-author list, which holds
/// `PlaylistEntry` rather than index rows. `Secondary` is "Released" here.
pub fn tune_sort_cmp(
    col: HvscSortColumn,
    dir: crate::ui::SortDirection,
    a: &crate::playlist::PlaylistEntry,
    b: &crate::playlist::PlaylistEntry,
) -> std::cmp::Ordering {
    let ord = match col {
        HvscSortColumn::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        HvscSortColumn::Secondary => a.released.to_lowercase().cmp(&b.released.to_lowercase()),
        HvscSortColumn::Subs => a.songs.cmp(&b.songs),
        HvscSortColumn::Len => a.duration_secs.cmp(&b.duration_secs),
    }
    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    match dir {
        crate::ui::SortDirection::Ascending => ord,
        crate::ui::SortDirection::Descending => ord.reverse(),
    }
}

/// Comparator for the tune table's sortable columns.
pub fn hvsc_sort_cmp(
    col: HvscSortColumn,
    dir: crate::ui::SortDirection,
    a: &HvscIndexEntry,
    b: &HvscIndexEntry,
) -> std::cmp::Ordering {
    let ord = match col {
        HvscSortColumn::Title => a.title_lower.cmp(&b.title_lower),
        HvscSortColumn::Secondary => a.author_lower.cmp(&b.author_lower),
        HvscSortColumn::Subs => a.songs.cmp(&b.songs),
        HvscSortColumn::Len => a.duration_secs.cmp(&b.duration_secs),
    }
    // Ties resolve by title so repeated sorts don't shuffle equal rows.
    .then_with(|| a.title_lower.cmp(&b.title_lower));
    match dir {
        crate::ui::SortDirection::Ascending => ord,
        crate::ui::SortDirection::Descending => ord.reverse(),
    }
}

/// Walk one author's folder and parse every tune. Blocking — run off-thread.
pub fn load_author_tunes(
    author: HvscAuthor,
    root: Option<PathBuf>,
    stil: Option<StilDb>,
    songlength: Option<SonglengthDb>,
) -> Vec<HvscTune> {
    let mut tunes = Vec::new();
    for dirent in WalkDir::new(&author.path)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        let p = dirent.path();
        if !p.is_file() || !is_sid_or_mus(p) {
            continue;
        }
        let entry = match PlaylistEntry::from_path(p) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry = apply_songlength(entry, songlength.as_ref());
        let has_stil = stil_has_entry(&author.path, p, stil.as_ref(), root.as_deref());
        tunes.push(HvscTune { entry, has_stil });
    }
    tunes.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    tunes
}

/// Off-thread flat-index builder. Walks every `.sid`/`.mus` file under
/// `<root>/<category>/`, parses each SID header (via
/// `PlaylistEntry::from_path`), applies the optional songlength lookup
/// and the STIL ✓ marker, and returns the enriched rows sorted by title.
/// Typical cost: ~5-10 s cold / ~1 s warm for ~10k files on SSD. Meant
/// to run inside `iced::Task::perform`; the caller passes the result
/// through `HvscBrowser::install_flat_index` with the version stamp
/// returned by `begin_flat_index_build`.
pub fn build_flat_index_worker(
    root: PathBuf,
    category: HvscCategory,
    stil: Option<StilDb>,
    songlength: Option<SonglengthDb>,
) -> Vec<HvscIndexEntry> {
    let category_dir = root.join(category.dir_name());
    if !category_dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
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
        let author_raw = parent_under_category(p, &category_dir)
            .map(|s| s.to_string())
            .unwrap_or_default();
        // SID header + songlength enrichment. If the header can't be
        // parsed we still keep the row — the stem + author folder are
        // enough for the user to find it, and the metadata columns will
        // just be blank.
        let (title, released, songs, duration_secs) = match PlaylistEntry::from_path(p) {
            Ok(e) => {
                let e = apply_songlength(e, songlength.as_ref());
                (
                    if e.title.is_empty() {
                        stem.clone()
                    } else {
                        e.title
                    },
                    e.released,
                    e.songs,
                    e.duration_secs,
                )
            }
            Err(_) => (stem.clone(), String::new(), 1, None),
        };
        // stil_has_entry wants the author directory (for the fallback
        // path it constructs when hvsc_root is unknown). The immediate
        // parent works for both HVSC layouts here.
        let author_dir = p.parent().unwrap_or(&category_dir);
        let has_stil = stil_has_entry(author_dir, p, stil.as_ref(), Some(root.as_path()));
        let author_lower = author_raw.to_ascii_lowercase();
        let title_lower = title.to_ascii_lowercase();
        out.push(HvscIndexEntry {
            path: p.to_path_buf(),
            stem,
            author_raw,
            title,
            released,
            songs,
            duration_secs,
            has_stil,
            author_lower,
            title_lower,
        });
    }
    out.sort_by(|a, b| a.title_lower.cmp(&b.title_lower));
    out
}

/// Pick a random SID/MUS path under `<root>/<category>/` with reservoir
/// sampling (O(1) memory, one pass). This is the cold path for 🎲 Surprise
/// Me and **must be run off the UI thread** — the `is_dir` stat and
/// `WalkDir` perform filesystem I/O that blocks for a long time when the
/// HVSC root lives on a network/mapped drive or an offline OneDrive
/// placeholder. Meant to run inside `iced::Task::perform`, mirroring
/// [`build_flat_index_worker`].
pub fn random_hvsc_path_walk(root: PathBuf, category: HvscCategory) -> Option<PathBuf> {
    use rand::Rng;
    let category_dir = root.join(category.dir_name());
    // Log the target BEFORE the is_dir() stat — on a dead network drive or an
    // offline OneDrive placeholder even this stat can block; if it hangs here
    // this is the last line in the log (now on a worker thread, not the UI).
    crate::dlog!(
        "random_hvsc_path_walk: checking is_dir on {}",
        category_dir.display()
    );
    if !category_dir.is_dir() {
        crate::dlog!(
            "random_hvsc_path_walk: category dir missing / not a directory (tree not synced?) -> None"
        );
        return None;
    }
    let mut rng = rand::thread_rng();
    let mut chosen: Option<PathBuf> = None;
    let mut seen: u64 = 0;
    crate::dlog!(
        "random_hvsc_path_walk: COLD WalkDir starting over {} (follow_links=true)",
        category_dir.display()
    );
    let t0 = std::time::Instant::now();
    for dirent in WalkDir::new(&category_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        let p = dirent.path();
        if !p.is_file() || !is_sid_or_mus(p) {
            continue;
        }
        seen += 1;
        if chosen.is_none() || rng.gen_range(0..seen) == 0 {
            chosen = Some(p.to_path_buf());
        }
    }
    crate::dlog!(
        "random_hvsc_path_walk: walk done, seen={seen}, elapsed={}ms",
        t0.elapsed().as_millis()
    );
    chosen
}

impl HvscBrowser {
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

    /// Fast path for 🎲 Surprise Me: if the enriched flat index is already
    /// in memory, sample from it with zero disk I/O. Returns `None` when the
    /// index isn't warm — the caller must then run [`random_hvsc_path_walk`]
    /// on a background thread (see [`surprise_cold_target`]), because the
    /// cold `WalkDir` can block for a long time on a network/cloud-backed
    /// HVSC root and must never run on the UI thread.
    ///
    /// [`surprise_cold_target`]: HvscBrowser::surprise_cold_target
    pub fn random_hvsc_warm(&self) -> Option<PathBuf> {
        if self.flat_index_loaded && !self.flat_index.is_empty() {
            use rand::Rng;
            crate::dlog!("random_hvsc_warm: flat_index n={}", self.flat_index.len());
            let i = rand::thread_rng().gen_range(0..self.flat_index.len());
            return Some(self.flat_index[i].path.clone());
        }
        None
    }

    /// Owned `(root, category)` for an off-thread Surprise pick, or `None`
    /// when no HVSC root is configured. Cheap to clone; hand the result to
    /// [`random_hvsc_path_walk`] inside a `Task::perform` so the blocking
    /// walk stays off the UI thread.
    pub fn surprise_cold_target(&self) -> Option<(PathBuf, HvscCategory)> {
        match self.root.as_ref() {
            Some(root) => Some((root.clone(), self.category)),
            None => {
                crate::dlog!("surprise_cold_target: no HVSC root set (tree not synced) -> None");
                None
            }
        }
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
        // Seed the filter cache here so `view()` never reads an empty slice
        // before the first user interaction.
        self.recompute_authors();
        Ok(())
    }

    /// Walk the selected author's folder, build a `HvscTune` per `.sid`/
    /// `.mus` file. Applies songlength durations and STIL ✓ markers from
    /// the provided DBs (both optional). Typically completes in tens of ms.
    /// Mark an author selected and return the work needed to load its tunes,
    /// or `None` if there is nothing to load. The caller runs
    /// `load_author_tunes` off the UI thread and hands the result back to
    /// `install_author_tunes`.
    ///
    /// Reading and MD5-ing a big author's files takes ~21 ms warm on a local
    /// disk and far longer on a cold or network-backed one, which is a visible
    /// stall when it happens inside `update`.
    pub fn begin_select_author(&mut self, idx: usize) -> Option<(HvscAuthor, u64)> {
        self.selected_author = Some(idx);
        self.tunes.clear();
        self.selected_tune = None;
        self.hovered_row = None;
        self.authors
            .get(idx)
            .cloned()
            .map(|a| (a, self.flat_index_version))
    }

    /// Install tunes produced by `load_author_tunes`. Dropped if the user has
    /// moved on (different root/category) since the load started.
    pub fn install_author_tunes(&mut self, version: u64, tunes: Vec<HvscTune>) {
        if version != self.flat_index_version {
            return;
        }
        self.tunes = tunes;
    }

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

    /// Indices into `authors` matching the *author* filter box. Deliberately
    /// independent of `search`: sharing one string meant typing a tune name
    /// emptied the author column.
    /// Cached author-column filter. Call `recompute_authors` first; this is a
    /// read for `view()`.
    pub fn filtered_authors(&self) -> &[usize] {
        self.author_cache
            .as_ref()
            .map(|(_, _, v)| v.as_slice())
            .unwrap_or(&[])
    }

    /// Refresh the author filter if the query or the author list changed.
    /// Cheap to call every update; the key comparison is the fast path.
    pub fn recompute_authors(&mut self) {
        let needle = self.author_filter.trim().to_lowercase();
        if let Some((q, n, _)) = &self.author_cache {
            if q == &needle && *n == self.authors.len() {
                return;
            }
        }
        let out: Vec<usize> = if needle.is_empty() {
            (0..self.authors.len()).collect()
        } else {
            self.authors
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    a.raw_name.to_lowercase().contains(&needle)
                        || a.display_name.to_lowercase().contains(&needle)
                })
                .map(|(i, _)| i)
                .collect()
        };
        self.author_cache = Some((needle, self.authors.len(), out));
    }

    /// Indices into `tunes` matching the search query — title, author,
    /// released, or filename stem.
    pub fn filtered_tunes(&self) -> Vec<usize> {
        let mut out = self.filtered_tunes_unsorted();
        if let Some((col, dir)) = self.sort {
            let tunes = &self.tunes;
            out.sort_by(|&a, &b| tune_sort_cmp(col, dir, &tunes[a].entry, &tunes[b].entry));
        }
        out
    }

    fn filtered_tunes_unsorted(&self) -> Vec<usize> {
        // `trim` matters: the old code lowercased the raw string, so a single
        // trailing space from a paste matched nothing at all.
        let needle = self.search.trim().to_lowercase();
        if needle.is_empty() {
            return (0..self.tunes.len()).collect();
        }
        self.tunes
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let e = &t.entry;
                e.title.to_lowercase().contains(&needle)
                    || e.author.to_lowercase().contains(&needle)
                    || e.released.to_lowercase().contains(&needle)
                    || e.path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_lowercase().contains(&needle))
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

    fn entry(
        title: &str,
        author: &str,
        released: &str,
        songs: u16,
        secs: Option<u32>,
    ) -> HvscIndexEntry {
        HvscIndexEntry {
            path: PathBuf::from(format!("/hvsc/{author}/{title}.sid")),
            stem: title.to_string(),
            author_raw: author.to_string(),
            title: title.to_string(),
            released: released.to_string(),
            songs,
            duration_secs: secs,
            has_stil: false,
            title_lower: title.to_ascii_lowercase(),
            author_lower: author.to_ascii_lowercase(),
        }
    }

    fn tune(title: &str) -> HvscTune {
        HvscTune {
            entry: crate::playlist::PlaylistEntry {
                path: PathBuf::from(format!("/hvsc/{title}.sid")),
                title: title.to_string(),
                author: String::new(),
                released: String::new(),
                songs: 1,
                selected_song: 1,
                is_pal: true,
                num_sids: 1,
                is_rsid: false,
                md5: None,
                duration_secs: None,
                has_wds: false,
            },
            has_stil: false,
        }
    }

    fn score(query: &str, e: &HvscIndexEntry) -> Option<u32> {
        let mut m = Matcher::new(Config::DEFAULT);
        let pat = HvscQuery::parse(query);
        let mut buf = Vec::new();
        score_entry(&pat, e, &mut m, &mut buf)
    }

    #[test]
    fn fuzzy_tolerates_typos() {
        let commando = entry("Commando", "Hubbard_Rob", "1985 Elite", 1, Some(90));
        // The point of the whole fuzzy change: a dropped letter still matches.
        assert!(score("commndo", &commando).is_some());
        assert!(score("Commando", &commando).is_some());
        // Matching the author works too, since it is one of the scored fields.
        assert!(score("hubbard", &commando).is_some());
        assert!(score("zzzzqqq", &commando).is_none());
    }

    #[test]
    fn title_outranks_incidental_released_match() {
        // "elite" is this one's actual title...
        let by_title = entry("Elite", "Braben_David", "1984 Acornsoft", 1, Some(60));
        // ...and only the publisher of this one. The title hit must win, which
        // the old first-500-alphabetically behaviour could not express.
        let by_released = entry("Zoom", "Someone_Else", "1985 Elite", 1, Some(60));
        let a = score("elite", &by_title).expect("title should match");
        let b = score("elite", &by_released).expect("released should match");
        assert!(a > b, "title hit {a} should outrank released hit {b}");
    }

    #[test]
    fn real_author_beats_subsequence_noise() {
        // Reported case: searching "antti" ranked "Fantastic_Zool" and
        // "Blanchette_Francois" above the actual Antti authors, because
        // a-n-t-t-i appears in order inside both. A contiguous hit must win.
        let real = entry("Some Tune", "Hannula_Antti", "1994", 1, Some(60));
        let noise = entry("Melody Rulez", "Fantastic_Zool", "1995", 1, Some(60));
        let noise2 = entry("Creepspread", "Blanchette_Francois", "1996", 1, Some(60));

        let r = score("antti", &real).expect("real author must match");
        for (name, n) in [("Fantastic_Zool", &noise), ("Blanchette_Francois", &noise2)] {
            if let Some(ns) = score("antti", n) {
                assert!(r > ns, "Hannula_Antti ({r}) must outrank {name} ({ns})");
            }
        }
    }

    #[test]
    fn substring_beats_fuzzy_even_across_fields() {
        // A contiguous match in the lowest-weighted field still beats a
        // scattered match in the highest-weighted one.
        let contiguous = entry("Zzz", "Nobody", "antti 1994", 1, Some(60));
        let scattered = entry("Fantastic Adventure", "Nobody", "1995", 1, Some(60));
        let c = score("antti", &contiguous).expect("substring in released");
        if let Some(sc) = score("antti", &scattered) {
            assert!(c > sc, "contiguous {c} must beat scattered {sc}");
        }
    }

    #[test]
    fn sort_columns_order_both_ways() {
        use crate::ui::SortDirection::{Ascending, Descending};
        let a = entry("Alpha", "AAA", "1985", 1, Some(10));
        let b = entry("Beta", "BBB", "1986", 9, Some(99));

        for (col, expect_a_first) in [
            (HvscSortColumn::Title, true),
            (HvscSortColumn::Secondary, true),
            (HvscSortColumn::Subs, true),
            (HvscSortColumn::Len, true),
        ] {
            let asc = hvsc_sort_cmp(col, Ascending, &a, &b);
            let desc = hvsc_sort_cmp(col, Descending, &a, &b);
            assert_eq!(asc.is_lt(), expect_a_first, "ascending {col:?}");
            assert_eq!(
                desc,
                asc.reverse(),
                "descending {col:?} must mirror ascending"
            );
        }
    }

    #[test]
    fn sort_is_stable_on_ties() {
        use crate::ui::SortDirection::Ascending;
        // Equal on the sorted column — the title tie-break keeps the order
        // fixed so repeated sorts don't shuffle rows around.
        let a = entry("Alpha", "AAA", "1985", 5, Some(10));
        let b = entry("Beta", "BBB", "1985", 5, Some(10));
        assert!(hvsc_sort_cmp(HvscSortColumn::Subs, Ascending, &a, &b).is_lt());
        assert!(hvsc_sort_cmp(HvscSortColumn::Subs, Ascending, &b, &a).is_gt());
    }

    #[test]
    fn toggle_sort_cycles_asc_desc_then_off() {
        use crate::ui::SortDirection::{Ascending, Descending};
        let mut b = HvscBrowser::default();
        assert_eq!(b.sort(), None);
        b.toggle_sort(HvscSortColumn::Title);
        assert_eq!(b.sort(), Some((HvscSortColumn::Title, Ascending)));
        b.toggle_sort(HvscSortColumn::Title);
        assert_eq!(b.sort(), Some((HvscSortColumn::Title, Descending)));
        // Third click clears it, returning results to relevance order.
        b.toggle_sort(HvscSortColumn::Title);
        assert_eq!(b.sort(), None);
        // A different column always starts ascending.
        b.toggle_sort(HvscSortColumn::Len);
        assert_eq!(b.sort(), Some((HvscSortColumn::Len, Ascending)));
    }

    #[test]
    fn author_filter_is_independent_of_tune_search() {
        let mut b = HvscBrowser::default();
        b.authors = vec![
            HvscAuthor {
                raw_name: "Hubbard_Rob".into(),
                display_name: "Hubbard, Rob".into(),
                letter: 'H',
                path: PathBuf::from("/hvsc/H/Hubbard_Rob"),
            },
            HvscAuthor {
                raw_name: "Galway_Martin".into(),
                display_name: "Galway, Martin".into(),
                letter: 'G',
                path: PathBuf::from("/hvsc/G/Galway_Martin"),
            },
        ];
        // `load_authors_if_needed` seeds this in production; the test builds
        // `authors` directly, so it has to prime the cache itself.
        b.recompute_authors();

        // Searching for a tune must not empty the author column — the whole
        // point of splitting the two boxes.
        b.set_search("commando".into());
        b.recompute_authors();
        assert_eq!(b.filtered_authors().len(), 2);
        // The author box still filters on its own terms.
        b.set_author_filter("hubbard".into());
        b.recompute_authors();
        assert_eq!(b.filtered_authors(), vec![0]);
    }

    #[test]
    fn author_filter_is_cached_and_invalidates() {
        let mut b = HvscBrowser::default();
        b.authors = vec![
            HvscAuthor {
                raw_name: "Hubbard_Rob".into(),
                display_name: "Hubbard, Rob".into(),
                letter: 'H',
                path: PathBuf::from("/hvsc/H/Hubbard_Rob"),
            },
            HvscAuthor {
                raw_name: "Galway_Martin".into(),
                display_name: "Galway, Martin".into(),
                letter: 'G',
                path: PathBuf::from("/hvsc/G/Galway_Martin"),
            },
        ];
        b.recompute_authors();
        assert_eq!(b.filtered_authors().len(), 2);

        // Mutating behind the cache is invisible until something in the key
        // changes — proving view() reads a cache rather than re-filtering
        // ~1850 authors every frame.
        b.authors.push(HvscAuthor {
            raw_name: "Daglish_Ben".into(),
            display_name: "Daglish, Ben".into(),
            letter: 'D',
            path: PathBuf::from("/hvsc/D/Daglish_Ben"),
        });
        // Length is part of the key, so this one *does* invalidate.
        b.recompute_authors();
        assert_eq!(b.filtered_authors().len(), 3);

        b.set_author_filter("hubbard".into());
        b.recompute_authors();
        assert_eq!(b.filtered_authors(), vec![0]);
    }

    #[test]
    fn stale_author_load_is_discarded() {
        // Clicking author A then switching category before A's files finish
        // loading must not dump A's tunes into the new category's view.
        let mut b = HvscBrowser::default();
        b.authors = vec![HvscAuthor {
            raw_name: "A".into(),
            display_name: "A".into(),
            letter: 'A',
            path: PathBuf::from("/hvsc/A"),
        }];
        let (_, version) = b.begin_select_author(0).expect("author exists");
        b.set_category(HvscCategory::Demos); // bumps the version
        b.install_author_tunes(version, vec![tune("Stale")]);
        assert!(b.tunes().is_empty(), "stale load must be dropped");

        // A load stamped with the current version still installs.
        let current = b.flat_index_version();
        b.install_author_tunes(current, vec![tune("Fresh")]);
        assert_eq!(b.tunes().len(), 1);
    }

    #[test]
    fn search_cache_reports_true_total_when_capped() {
        let mut b = HvscBrowser::default();
        // More matches than the display cap.
        b.flat_index = (0..MAX_FLAT_RESULTS + 120)
            .map(|i| entry(&format!("Commando {i}"), "AAA", "1985", 1, Some(10)))
            .collect();
        b.flat_index_loaded = true;
        b.set_search("commando".into());
        b.recompute_search();
        assert_eq!(b.flat_results().len(), MAX_FLAT_RESULTS, "list is capped");
        assert_eq!(
            b.flat_total_matches(),
            MAX_FLAT_RESULTS + 120,
            "but the true match count is reported, not the index size"
        );
    }

    #[test]
    fn search_cache_recomputes_only_when_inputs_change() {
        let mut b = HvscBrowser::default();
        b.flat_index = vec![entry("Commando", "Hubbard_Rob", "1985", 1, Some(10))];
        b.flat_index_loaded = true;
        b.set_search("commando".into());
        b.recompute_search();
        assert_eq!(b.flat_results().len(), 1);

        // Mutating the index behind the cache's back is invisible until the
        // version changes — proving view() reads a cache and does not rescan.
        b.flat_index.clear();
        b.recompute_search();
        assert_eq!(
            b.flat_results().len(),
            1,
            "cache still serves the old result"
        );

        // A sort change is part of the key, so it does force a recompute.
        b.toggle_sort(HvscSortColumn::Title);
        b.recompute_search();
        assert_eq!(b.flat_results().len(), 0);
    }

    #[test]
    fn trailing_space_still_matches() {
        let mut b = HvscBrowser::default();
        b.tunes = vec![HvscTune {
            entry: crate::playlist::PlaylistEntry {
                path: PathBuf::from("/hvsc/Commando.sid"),
                title: "Commando".into(),
                author: String::new(),
                released: String::new(),
                songs: 1,
                selected_song: 1,
                is_pal: true,
                num_sids: 1,
                is_rsid: false,
                md5: None,
                duration_secs: None,
                has_wds: false,
            },
            has_stil: false,
        }];
        // The old code lowercased the raw string, so one pasted trailing
        // space matched nothing at all.
        b.set_search("commando ".into());
        assert_eq!(b.filtered_tunes(), vec![0]);
    }
}

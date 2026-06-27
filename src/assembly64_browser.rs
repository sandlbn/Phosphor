// Assembly64 browser — pure UI-facing state machine.
//
// HTTP work happens in `Task::perform` futures spawned from message
// handlers in main.rs; this module owns the search state, results
// accumulator, inline expansion map, and pagination cursor.
//
// Source toggle (Local HVSC vs Assembly64) and on-disk cache live in
// main.rs / config.rs respectively — this struct is pure data.

use std::collections::{HashMap, HashSet};

use crate::assembly64::{AsmEntry, AsmFile};

pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// State of one entry's inline file-list expansion.
#[derive(Debug, Clone)]
pub enum ExpansionState {
    /// list_files request is in flight.
    Loading,
    /// Files loaded; filtered to `.sid` items by the UI.
    Loaded(Vec<AsmFile>),
    /// list_files failed; we keep the message for inline display.
    Failed(String),
}

/// State for the Assembly64 browser. Cheap to construct; default = empty.
#[derive(Debug, Default)]
pub struct Assembly64Browser {
    /// Current text in the search input.
    query: String,
    /// The query the current `results` were fetched with. Differs from
    /// `query` while the user is mid-edit before pressing ENTER.
    results_query: String,
    /// Accumulated results across "Load more" clicks.
    results: Vec<AsmEntry>,
    /// item_id → expansion state.
    expanded: HashMap<String, ExpansionState>,
    /// Pagination cursor: byte offset into the result set for the NEXT
    /// "Load more" page (i.e. `results.len()` after each successful page).
    offset: u32,
    /// Page size used for both the initial fetch and "Load more" pages.
    page_size: u32,
    /// True when the last fetch returned >= page_size — there may be more.
    more_available: bool,
    /// search() request in flight (drives the "Searching…" status line).
    search_in_flight: bool,
    /// Last error from any A64 call (search/list_files/download).
    /// Surfaced inline above the results list.
    last_error: Option<String>,
    /// Cache of `list_files` responses, populated by both the
    /// background prefetch (fires once per search hit) and by
    /// manual expansion. Expand-clicks read this first to skip
    /// the network round-trip when the prefetch already returned.
    file_cache: HashMap<String, Vec<AsmFile>>,
    /// item_ids whose prefetch confirmed 0 playable `.sid` files.
    /// UI skips these rows so the user only sees releases that
    /// actually contain SIDs.
    hidden: HashSet<String>,
    /// Count of search hits whose prefetch is still in flight —
    /// used to render a "Checking N releases…" line so the user
    /// understands why entries are appearing and disappearing.
    prefetch_pending: u32,
}

impl Assembly64Browser {
    pub fn new() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            ..Default::default()
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, q: String) {
        self.query = q;
    }

    pub fn results(&self) -> &[AsmEntry] {
        &self.results
    }

    pub fn expansion(&self, item_id: &str) -> Option<&ExpansionState> {
        self.expanded.get(item_id)
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn more_available(&self) -> bool {
        self.more_available
    }

    pub fn search_in_flight(&self) -> bool {
        self.search_in_flight
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn results_query(&self) -> &str {
        &self.results_query
    }

    pub fn is_hidden(&self, item_id: &str) -> bool {
        self.hidden.contains(item_id)
    }

    pub fn prefetched_files(&self, item_id: &str) -> Option<&[AsmFile]> {
        self.file_cache.get(item_id).map(|v| v.as_slice())
    }

    pub fn prefetch_pending(&self) -> u32 {
        self.prefetch_pending
    }

    /// Called once per search hit BEFORE we fire its prefetch task,
    /// so the UI can count how many are still being verified.
    pub fn note_prefetch_started(&mut self, n: u32) {
        self.prefetch_pending = self.prefetch_pending.saturating_add(n);
    }

    /// Record a prefetched file listing. Marks the entry hidden if
    /// it contains no playable SIDs.
    pub fn record_prefetch(&mut self, item_id: String, files: Vec<AsmFile>) {
        self.prefetch_pending = self.prefetch_pending.saturating_sub(1);
        let has_sid = files.iter().any(|f| f.is_sid());
        if !has_sid {
            self.hidden.insert(item_id.clone());
        }
        self.file_cache.insert(item_id, files);
    }

    /// Called when a prefetch fails (network etc.). We can't tell
    /// whether the entry is empty — leave it visible and let the
    /// user expand it manually.
    pub fn record_prefetch_failure(&mut self) {
        self.prefetch_pending = self.prefetch_pending.saturating_sub(1);
    }

    /// Begin a fresh search. Caller fires the async search request after
    /// this returns; we just transition state.
    pub fn begin_search(&mut self) {
        self.search_in_flight = true;
        self.results.clear();
        self.expanded.clear();
        self.file_cache.clear();
        self.hidden.clear();
        self.prefetch_pending = 0;
        self.offset = 0;
        self.more_available = false;
        self.last_error = None;
        self.results_query = self.query.clone();
    }

    /// Begin loading the NEXT page (appending). Caller fires the async
    /// request afterwards.
    pub fn begin_load_more(&mut self) {
        self.search_in_flight = true;
        self.last_error = None;
    }

    /// Apply a successful search result page. `replace_or_append == true`
    /// means "this is the first page" (replace) — false means append.
    pub fn apply_results(&mut self, page: Vec<AsmEntry>, replace: bool) {
        self.search_in_flight = false;
        let page_full = page.len() as u32 >= self.page_size;
        if replace {
            self.results = page;
            self.offset = self.results.len() as u32;
        } else {
            self.offset = self.offset.saturating_add(page.len() as u32);
            self.results.extend(page);
        }
        self.more_available = page_full;
    }

    /// Apply a search error (terminal for the current page; user can retry).
    pub fn set_search_error(&mut self, msg: String) {
        self.search_in_flight = false;
        self.last_error = Some(msg);
    }

    pub fn set_expanded_loading(&mut self, item_id: String) {
        self.expanded.insert(item_id, ExpansionState::Loading);
    }

    pub fn set_expanded_loaded(&mut self, item_id: String, files: Vec<AsmFile>) {
        // Cache so collapse/expand cycles don't re-fetch. We DON'T mark
        // hidden here — manual expand is an explicit user signal; if the
        // entry has no SIDs they should still see "No .sid files".
        self.file_cache.insert(item_id.clone(), files.clone());
        self.expanded.insert(item_id, ExpansionState::Loaded(files));
    }

    pub fn set_expanded_failed(&mut self, item_id: String, msg: String) {
        self.expanded.insert(item_id, ExpansionState::Failed(msg));
    }

    pub fn collapse(&mut self, item_id: &str) {
        self.expanded.remove(item_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> AsmEntry {
        AsmEntry {
            id: id.into(),
            name: format!("Entry {id}"),
            category: 4,
            group: String::new(),
            handle: String::new(),
            year: 0,
            released: String::new(),
            rating: 0,
            updated: String::new(),
        }
    }

    #[test]
    fn begin_search_resets_state() {
        let mut b = Assembly64Browser::new();
        b.set_query("commando".into());
        b.apply_results(vec![entry("1"), entry("2")], true);
        b.set_expanded_loading("1".into());
        b.begin_search();
        assert!(b.search_in_flight());
        assert!(b.results().is_empty());
        assert!(b.expansion("1").is_none());
        assert_eq!(b.offset(), 0);
    }

    #[test]
    fn apply_results_tracks_more_available() {
        let mut b = Assembly64Browser::new();
        b.page_size = 5;
        let full_page: Vec<_> = (0..5).map(|i| entry(&i.to_string())).collect();
        b.apply_results(full_page, true);
        assert!(b.more_available());
        assert_eq!(b.offset(), 5);

        let partial: Vec<_> = (5..8).map(|i| entry(&i.to_string())).collect();
        b.apply_results(partial, false);
        assert!(!b.more_available());
        assert_eq!(b.offset(), 8);
        assert_eq!(b.results().len(), 8);
    }

    #[test]
    fn expansion_state_round_trip() {
        let mut b = Assembly64Browser::new();
        b.set_expanded_loading("42".into());
        assert!(matches!(b.expansion("42"), Some(ExpansionState::Loading)));
        b.set_expanded_loaded(
            "42".into(),
            vec![AsmFile {
                id: 0,
                path: "x.sid".into(),
                size: 0,
            }],
        );
        match b.expansion("42") {
            Some(ExpansionState::Loaded(files)) => assert_eq!(files.len(), 1),
            _ => panic!("expected Loaded"),
        }
        b.collapse("42");
        assert!(b.expansion("42").is_none());
    }
}

// Published Playlists browser — pure UI-facing state machine.
//
// HTTP work happens in `Task::perform` futures spawned from message
// handlers in main.rs; this module owns the manifest, the active-file
// indicator, sync flags, and per-row preview state.
//
// The "default playlist is never overwritten" contract lives on the
// `App::session_mode` field in main.rs — this struct just knows which
// published file is currently loaded so the UI can render the banner
// and offer the "Restore my playlist" button.

use std::collections::HashMap;

use crate::playlist::PreviewTrack;
use crate::published_playlists::{Manifest, PublishedPlaylistMeta};

/// Per-row preview state for the inline ▾ track list.
#[derive(Debug, Clone)]
pub enum PreviewState {
    /// The M3U isn't on disk yet (sync in flight) — wait for the
    /// FileDone hook to fire a re-parse.
    Loading,
    /// Parsed, ready to render.
    Ready(Vec<PreviewTrack>),
    /// Parsing failed (read or parse error). Surface inline.
    Failed(String),
}

#[derive(Debug, Default)]
pub struct PublishedPlaylistsBrowser {
    manifest: Option<Manifest>,
    sync_in_flight: bool,
    /// Count of background per-playlist downloads still running after
    /// the manifest came back. Drives "Updating 3 of 12…".
    download_pending: u32,
    last_error: Option<String>,
    last_synced_unix: Option<i64>,
    /// The currently-loaded published file (None = default mode).
    active_file: Option<String>,
    expanded: HashMap<String, PreviewState>,
}

impl PublishedPlaylistsBrowser {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Getters ────────────────────────────────────────────────────
    pub fn playlists(&self) -> &[PublishedPlaylistMeta] {
        self.manifest
            .as_ref()
            .map(|m| m.playlists.as_slice())
            .unwrap_or(&[])
    }

    pub fn sync_in_flight(&self) -> bool {
        self.sync_in_flight
    }

    pub fn download_pending(&self) -> u32 {
        self.download_pending
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn last_synced_unix(&self) -> Option<i64> {
        self.last_synced_unix
    }

    pub fn preview(&self, file: &str) -> Option<&PreviewState> {
        self.expanded.get(file)
    }

    pub fn is_expanded(&self, file: &str) -> bool {
        self.expanded.contains_key(file)
    }

    pub fn meta_for(&self, file: &str) -> Option<&PublishedPlaylistMeta> {
        self.playlists().iter().find(|p| p.file == file)
    }

    // ── Sync state machine ────────────────────────────────────────
    pub fn begin_sync(&mut self) {
        self.sync_in_flight = true;
        self.last_error = None;
    }

    pub fn apply_manifest(&mut self, m: Manifest, unix_now: i64) {
        self.sync_in_flight = false;
        self.last_synced_unix = Some(unix_now);
        self.manifest = Some(m);
    }

    pub fn set_error(&mut self, msg: String) {
        self.sync_in_flight = false;
        self.last_error = Some(msg);
    }

    pub fn note_download_started(&mut self, n: u32) {
        self.download_pending = self.download_pending.saturating_add(n);
    }

    pub fn note_download_finished(&mut self) {
        self.download_pending = self.download_pending.saturating_sub(1);
    }

    // ── Preview state ─────────────────────────────────────────────
    pub fn set_preview_loading(&mut self, file: String) {
        self.expanded.insert(file, PreviewState::Loading);
    }

    pub fn set_preview_ready(&mut self, file: String, tracks: Vec<PreviewTrack>) {
        self.expanded.insert(file, PreviewState::Ready(tracks));
    }

    pub fn set_preview_failed(&mut self, file: String, msg: String) {
        self.expanded.insert(file, PreviewState::Failed(msg));
    }

    pub fn collapse(&mut self, file: &str) {
        self.expanded.remove(file);
    }

    // ── Active-file indicator ─────────────────────────────────────
    pub fn set_active(&mut self, file: String) {
        self.active_file = Some(file);
    }

    pub fn clear_active(&mut self) {
        self.active_file = None;
    }

    pub fn restore_last_synced(&mut self, unix: Option<i64>) {
        self.last_synced_unix = unix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::published_playlists::{Manifest, PublishedPlaylistMeta};

    fn manifest_with(files: &[&str]) -> Manifest {
        Manifest {
            version: 1,
            playlists: files
                .iter()
                .map(|f| PublishedPlaylistMeta {
                    file: (*f).to_string(),
                    name: (*f).to_string(),
                    description: String::new(),
                    tracks: 0,
                    sha256: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn sync_state_transitions() {
        let mut b = PublishedPlaylistsBrowser::new();
        assert!(!b.sync_in_flight());
        b.begin_sync();
        assert!(b.sync_in_flight());
        b.apply_manifest(manifest_with(&["a.m3u"]), 1_700_000_000);
        assert!(!b.sync_in_flight());
        assert_eq!(b.playlists().len(), 1);
        assert_eq!(b.last_synced_unix(), Some(1_700_000_000));
    }

    #[test]
    fn preview_round_trip() {
        let mut b = PublishedPlaylistsBrowser::new();
        b.set_preview_loading("a.m3u".into());
        assert!(matches!(b.preview("a.m3u"), Some(PreviewState::Loading)));
        b.set_preview_ready("a.m3u".into(), Vec::new());
        assert!(matches!(b.preview("a.m3u"), Some(PreviewState::Ready(_))));
        b.collapse("a.m3u");
        assert!(b.preview("a.m3u").is_none());
    }

    #[test]
    fn download_counter_saturating() {
        let mut b = PublishedPlaylistsBrowser::new();
        b.note_download_finished(); // underflow guard
        assert_eq!(b.download_pending(), 0);
        b.note_download_started(3);
        b.note_download_finished();
        assert_eq!(b.download_pending(), 2);
    }
}

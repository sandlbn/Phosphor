#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
mod audio_volume;
mod c64_emu;
mod config;
mod device_config;
mod heard_db;
mod petscii;
mod player;
mod playlist;
mod recently_played;
mod sid_device;
mod stil;
mod ui;
mod version_check;

#[cfg(all(feature = "usb", target_os = "macos"))]
mod usb_bridge;

#[cfg(all(feature = "usb", target_os = "macos"))]
mod daemon_installer;

// Direct libusb path. On macOS this is the alternative to the bridge daemon
// (selected via Settings → macOS USB mode → Direct); on Linux/Windows it's
// the only USB path.
#[cfg(feature = "usb")]
mod sid_direct;

mod assembly64;
mod assembly64_browser;
mod hvsc_browser;
mod hvsc_sync;
mod published_playlists;
mod published_playlists_browser;
mod remote;
mod sid_emulated;
mod sid_sidlite;
mod sid_u64;

/// Windows-only high-resolution timer guard.
///
/// Windows' default scheduler tick is ~15.6 ms, so `thread::sleep()` rounds
/// up to the next 15.6 ms boundary unless someone has called
/// `timeBeginPeriod(1)` — which since Windows 10 21H2 is per-process. Our
/// player thread paces PAL frames at ~19.95 ms, so without 1 ms timer
/// resolution it sleeps to ~31 ms and misses every frame whenever Phosphor
/// runs in the background. RAII guard: bumps resolution at construction,
/// restores at drop.
#[cfg(windows)]
mod windows_timer {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(uPeriod: u32) -> u32;
        fn timeEndPeriod(uPeriod: u32) -> u32;
    }

    pub struct HiResTimerGuard;

    impl HiResTimerGuard {
        pub fn raise() -> Self {
            unsafe {
                timeBeginPeriod(1);
            }
            Self
        }
    }

    impl Drop for HiResTimerGuard {
        fn drop(&mut self) {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

use std::path::PathBuf;
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────────
//  Opt-in per-frame timing profiler
// ─────────────────────────────────────────────────────────────────────────────
//
// Enable with `PHOSPHOR_PROFILE_UPDATE=1`. When set, each `update()` and
// `view()` call is timed via an RAII guard; every 30 samples (roughly one
// second at the 30 Hz tick rate) we print the per-frame averages to
// stderr:
//
//   [perf] update=0.31ms/frame  view=3.14ms/frame
//
// Zero cost when the env var is unset (single `OnceLock` bool check per
// frame + no accumulator writes).

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::OnceLock;

static PROFILE_ENABLED: OnceLock<bool> = OnceLock::new();
static UPDATE_ACCUM_US: AtomicU64 = AtomicU64::new(0);
static VIEW_ACCUM_US: AtomicU64 = AtomicU64::new(0);
static SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);

fn profile_enabled() -> bool {
    *PROFILE_ENABLED.get_or_init(|| std::env::var("PHOSPHOR_PROFILE_UPDATE").is_ok())
}

enum ProfilerKind {
    Update,
    View,
}

struct ProfilerGuard {
    kind: ProfilerKind,
    start: Instant,
}

impl ProfilerGuard {
    #[inline]
    fn update() -> Option<Self> {
        if profile_enabled() {
            Some(Self {
                kind: ProfilerKind::Update,
                start: Instant::now(),
            })
        } else {
            None
        }
    }

    #[inline]
    fn view() -> Option<Self> {
        if profile_enabled() {
            Some(Self {
                kind: ProfilerKind::View,
                start: Instant::now(),
            })
        } else {
            None
        }
    }
}

impl Drop for ProfilerGuard {
    fn drop(&mut self) {
        let us = self.start.elapsed().as_micros() as u64;
        match self.kind {
            ProfilerKind::Update => {
                UPDATE_ACCUM_US.fetch_add(us, AtomicOrdering::Relaxed);
            }
            ProfilerKind::View => {
                VIEW_ACCUM_US.fetch_add(us, AtomicOrdering::Relaxed);
                let n = SAMPLE_COUNT.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                if n >= 30 {
                    let u = UPDATE_ACCUM_US.swap(0, AtomicOrdering::Relaxed);
                    let v = VIEW_ACCUM_US.swap(0, AtomicOrdering::Relaxed);
                    SAMPLE_COUNT.store(0, AtomicOrdering::Relaxed);
                    eprintln!(
                        "[perf] update={:.2}ms/frame  view={:.2}ms/frame  (last {n} frames)",
                        u as f64 / (n as f64) / 1000.0,
                        v as f64 / (n as f64) / 1000.0,
                    );
                }
            }
        }
    }
}

use crossbeam_channel::{self, Receiver, Sender};
use iced::widget::{column, container, mouse_area, rule, Space};
use iced::{event, time, Color, Element, Length, Subscription, Task, Theme};

use config::{Config, FavoritesDb};
use heard_db::HeardDb;
use player::{PlayState, PlayerCmd, PlayerStatus};
use playlist::{Playlist, SonglengthDb};
use recently_played::RecentlyPlayed;
use ui::sid_panel::{TrackerHistory, TrackerView};
use ui::visualizer::{TrackerRef, Visualizer};
use ui::{Message, SortColumn, SortDirection};

// ─────────────────────────────────────────────────────────────────────────────
//  Context menu state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ContextMenu {
    /// The playlist index of the right-clicked row.
    track_idx: usize,
    /// Absolute pixel position where the menu should appear.
    x: f32,
    y: f32,
}

/// Tracks whether the in-memory playlist represents the user's own
/// default (saved to session_playlist.m3u on exit) or a read-only
/// published playlist (which must NEVER overwrite the user's default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMode {
    Default,
    PublishedReadOnly { file: String },
}

impl Default for SessionMode {
    fn default() -> Self {
        SessionMode::Default
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Application state
// ─────────────────────────────────────────────────────────────────────────────

struct App {
    /// Channel to send commands to the player thread.
    cmd_tx: Sender<PlayerCmd>,
    /// Channel to receive status updates from the player thread.
    status_rx: Receiver<PlayerStatus>,
    /// Last known player status (state, elapsed time, voice levels, …).
    status: PlayerStatus,

    /// Playlist model — entries, current index, shuffle/repeat state.
    playlist: Playlist,
    /// Row highlighted by keyboard / single-click (not necessarily playing).
    selected: Option<usize>,
    /// Oscilloscope / bar visualiser driven by voice-level data.
    visualizer: Visualizer,
    /// HVSC Songlength database, loaded on demand.
    songlength_db: Option<SonglengthDb>,

    /// Current text in the search box.
    search_text: String,
    /// Indices into `playlist.entries` that pass the current search + sort.
    filtered_indices: Vec<usize>,

    /// Which column the playlist is currently sorted by.
    sort_column: SortColumn,
    /// Ascending or descending sort direction.
    sort_direction: SortDirection,

    /// Persistent application configuration (saved to disk on every change).
    config: Config,
    /// Whether the settings panel is currently visible.
    show_settings: bool,
    /// Whether the USBSID-Pico Device Config panel is currently visible.
    show_device_config: bool,
    /// Cached state for the Device Config panel. `None` means we haven't
    /// successfully read the device yet.
    device_cfg: Option<ui::DeviceConfigSnapshot>,
    /// One-line status banner shown above the panel (e.g. "Reading…",
    /// "Saved!", "Error: …").
    device_cfg_status: String,
    /// Channel the player thread sends DeviceConfigEvent results on.
    device_cfg_rx: crossbeam_channel::Receiver<player::DeviceConfigEvent>,
    /// Have we already auto-fetched USB device info for the engine-label
    /// suffix? One-shot — fires on the first USB playback per app session.
    usb_info_fetched: bool,
    /// Raw text from the default-song-length input field (may be mid-edit).
    default_length_text: String,
    /// Raw text from the base-font-size input field. Holds intermediate
    /// keystrokes (e.g. just `"1"` while typing `"14"`) so the field stays
    /// editable; only successful parses commit to config.
    base_font_size_text: String,
    /// Live-edited HTTP proxy URL — only persisted to config.proxy_url
    /// when the user clicks Apply (avoids touching live HTTP clients
    /// on every keystroke).
    proxy_url_text: String,
    /// First-run welcome card. Shown on cold launch when
    /// `!config.has_seen_welcome`; hidden as soon as the user picks any
    /// action or clicks Skip.
    show_welcome: bool,
    /// Sleep timer — Instant at which playback should auto-stop.
    /// None → timer disabled. Session-only (not persisted).
    sleep_deadline: Option<Instant>,
    /// Configured duration for the current sleep timer, so the UI can
    /// render "Sleep in 15 min" even after the deadline drifts.
    sleep_selected_mins: Option<u32>,
    /// Status message shown below the Songlength download button.
    download_status: String,
    /// Combined status for auto-downloads shown in the search bar.
    auto_download_status: String,
    /// Number of auto-downloads still in flight (0, 1 or 2).
    pending_auto_downloads: u8,

    /// Shared progress string updated by background file-loading tasks.
    loading_progress: playlist::LoadingProgress,
    /// Entries waiting to be processed (chained message pipeline).
    pending_entries: Option<Vec<playlist::PlaylistEntry>>,

    /// Scroll playlist to current track on next tick (set when playback starts).
    scroll_to_current: bool,

    /// Available update info (if any), shown as a badge in the controls bar.
    new_version: Option<version_check::NewVersionInfo>,

    /// Favorites database (MD5 hashes), persisted to favorites.txt.
    favorites: FavoritesDb,
    /// Whether to show only favorite tunes in the playlist view.
    favorites_only: bool,
    /// Current window width — used for responsive compact/normal layout.
    window_width: f32,
    /// Current window height — used for context-menu flip-to-fit logic.
    window_height: f32,

    /// Recently played history (last 100 unique tracks).
    recently_played: RecentlyPlayed,
    heard_db: HeardDb,
    /// Pre-formatted HVSC completion string for the status bar.
    heard_text: String,
    /// Remote HVSC version reported by the boot-time `check_hvsc_update`
    /// probe. None until the probe finishes (or if it fails).
    hvsc_remote_version: Option<u32>,
    /// Pre-formatted "HVSC v84 ✓" / "HVSC v72 → v84 ⚠" string for the
    /// status bar. Recomputed by `refresh_hvsc_status()` whenever the
    /// local STIL DB loads or the remote check returns.
    hvsc_status_text: String,
    /// True when the remote HVSC is newer than the local copy. Drives the
    /// status bar's amber colour for the update marker.
    hvsc_update_available: bool,
    /// Whether the recently played panel is visible instead of the playlist.
    show_recently_played: bool,
    /// Whether the SID register info panel is visible instead of the playlist.
    show_sid_panel: bool,
    /// Whether the Browse panel (HVSC + Assembly64) is visible instead
    /// of the playlist. Mutually exclusive with the panels above.
    show_hvsc_browser: bool,
    /// Lazy walker over the synced HVSC tree (authors + per-author tunes).
    hvsc_browser: hvsc_browser::HvscBrowser,
    /// Browser source toggle (Local HVSC vs Assembly64). Persisted.
    browser_source: hvsc_browser::BrowserSource,
    /// Assembly64 search state machine.
    assembly64_browser: assembly64_browser::Assembly64Browser,
    /// Shared HTTP client for the Assembly64 API. Cheap to clone (Arc inside).
    assembly64_client: assembly64::Assembly64Client,
    /// Published Playlists (manifest + previews + active-file indicator).
    published_playlists_browser: published_playlists_browser::PublishedPlaylistsBrowser,
    /// Shared HTTP client for the Published Playlists CDN.
    published_playlists_client: published_playlists::PublishedPlaylistsClient,
    /// `Default` → session_playlist.m3u is written on exit as usual.
    /// `PublishedReadOnly` → the user's saved default is preserved
    /// untouched; the in-memory playlist is discarded on quit.
    session_mode: SessionMode,

    /// Absolute Y scroll offset of the playlist in pixels (updated by on_scroll).
    /// Used by the virtual list to compute which rows are in the viewport.
    playlist_scroll_offset_y: f32,
    /// Height of the playlist viewport in pixels (updated by on_scroll).
    /// Falls back to `window_height` as an estimate before the first scroll event.
    playlist_viewport_height: f32,
    /// Absolute Y position of the scrollable viewport within the window (logical px).
    playlist_viewport_y: f32,
    /// Physical-to-logical pixel ratio (1.0 on standard displays, 2.0 on Retina).
    /// Derived from the first right-click by comparing raw cursor coords to known
    /// logical row geometry.  Starts at 1.0 until calibrated.
    pixel_ratio: f32,

    /// Some(_) when the right-click context menu is visible.
    context_menu: Option<ContextMenu>,
    /// Consecutive frames with zero SID writes — used to detect end-of-song
    /// silence for MUS files that don't have songlength DB entries.
    silence_frames: u32,
    /// When the last auto-advance (subtune or track) was issued. Used to
    /// debounce a single auto-advance per 500 ms regardless of any stale-
    /// status race we haven't located. Reset to `None` on every user-
    /// initiated playback change (Play/Stop/Pause-resume) so the debounce
    /// can't fight an intentional rapid action.
    last_advance_at: Option<Instant>,
    /// Did we already log the SUPPRESSED diagnostic for the current debounce
    /// window? Without this we'd emit one log line per Tick (~15 lines per
    /// real subtune end) on slow engines like U64.
    advance_suppress_logged: bool,

    /// Whether the visualiser is expanded to fill the whole window (overlay mode).
    /// Double-clicking the visualiser canvas toggles this.
    vis_expanded: bool,
    show_help: bool,
    mini_mode: bool,
    /// Primary window ID — captured from the first window event for resize.
    window_id: Option<iced::window::Id>,
    /// Scroll offset for the STIL demoscene ticker in the expanded tracker view.
    stil_scroll_x: f32,
    /// Cached metadata for the concert-screen overlay, kept in sync with track_info
    /// on every tick so the borrow checker is happy in `view()`.
    vis_expanded_info: Option<ui::visualizer::ExpandedInfo>,

    /// Tracker ring buffer — decoded SID register frames for the tracker view.
    tracker_history: TrackerHistory,
    /// Tracker canvas cache — owns the iced Cache for the tracker Canvas.
    tracker_view: TrackerView,

    /// HVSC STIL database (loaded on demand from STIL.txt).
    stil_db: Option<stil::StilDb>,
    /// STIL entry for the currently playing tune (updated on track change).
    stil_entry: Option<stil::StilEntry>,
    /// Whether the STIL info overlay is currently visible.
    show_stil_overlay: bool,
    /// Status text shown below the STIL download button in settings.
    stil_status: String,
    /// HVSC rsync sync state. `Some` while a sync is in progress; dropped
    /// to None on completion or cancel.
    hvsc_sync: Option<hvsc_sync::HvscSyncHandle>,
    /// Most recent status line for the HVSC sync section.
    hvsc_sync_status: String,
    /// Optional `(files_done, files_total)` shown as a progress bar in Settings.
    /// Files (not bytes) because HVSC's HTTP index doesn't expose per-file
    /// sizes in a form gosh-dl extracts, so a byte-based bar would always
    /// read 0%. File counts are accurate.
    hvsc_sync_progress: Option<(u32, u32)>,
    /// Pre-formatted STIL text for the currently playing tune + subtune.
    /// Kept in App so view() can borrow it without a local String lifetime issue.
    stil_display_text: String,
    /// MUS FLAG command timestamps for karaoke sync (seconds per WDS line).
    karaoke_flag_times: Vec<f32>,
    /// Whether the MUS file contains any FLAG commands (in any voice).
    karaoke_has_flags: bool,
    /// Logical lyric groups parsed from WDS file (each group = 1+ display rows).
    karaoke_groups: Vec<Vec<String>>,
    /// Current karaoke group index (advanced by real-time FLAG events from player).
    karaoke_line: usize,
    /// Last seen flag_count from player status (to detect new FLAG events).
    last_flag_count: u32,
    /// Set once the background session load finishes.
    /// Guards save_session in Drop against nuking the file
    /// before loading completes.
    session_loaded: bool,
    /// Monotonic frame counter — drives loading animation redraw.
    tick: u32,
    /// Shared state for the HTTP remote control server.
    remote_state: Arc<Mutex<remote::SharedRemoteState>>,
    /// Commands from the HTTP remote control server.
    remote_cmd_rx: Receiver<remote::RemoteCmd>,
    /// Sender kept to start new servers on the fly.
    remote_cmd_tx: Sender<remote::RemoteCmd>,
    /// Whether the HTTP server is currently running.
    http_remote_running: bool,
    /// Editable text for the HTTP port field in settings.
    http_port_text: String,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let mut config = Config::load();
        // Snapshot hvsc_root for the browser model — `config` moves into
        // the struct literal below, so we can't reach it from there.
        let initial_hvsc_root = config.hvsc_root.as_deref().map(std::path::PathBuf::from);
        let initial_browser_src = config.browser_source.clone();
        let initial_assembly64_query = config.assembly64_last_query.clone();
        let initial_published_last_synced = config.published_playlists_last_synced;
        // Pre-seed the HVSC sync status from the persisted timestamp, while
        // we still own `config` by reference (we'll move it into the struct
        // literal below).
        let initial_hvsc_status = config
            .hvsc_last_sync
            .as_deref()
            .map(|t| format!("Last synced: {t}"))
            .unwrap_or_default();
        // Re-seed the global font scale to match the (possibly newer) config
        // boot reads; main() also seeds before the window opens.
        crate::ui::font::set_base(config.base_font_size);
        crate::audio_volume::set(config.master_volume);
        eprintln!(
            "[phosphor] Config: skip_rsid={}, default_length={}s, engine={}",
            config.skip_rsid, config.default_song_length_secs, config.output_engine,
        );

        #[cfg(all(feature = "usb", target_os = "macos"))]
        {
            let eng = config.output_engine.as_str();
            if (eng == "usb" || eng == "auto") && daemon_installer::daemon_needs_update() {
                eprintln!("[phosphor] Daemon binary path changed — triggering update");
                if let Err(e) = daemon_installer::ensure_daemon() {
                    eprintln!("[phosphor] Daemon auto-update skipped: {e}");
                }
            }
        }

        let (cmd_tx, status_rx, device_cfg_rx) = player::spawn_player(
            config.output_engine(),
            config.u64_address.clone(),
            config.u64_password.clone(),
            config.macos_usb_mode.clone(),
        );

        let playlist = Playlist::new();

        // Collect CLI file/dir args for background loading.
        let cli_paths: Vec<PathBuf> = std::env::args()
            .skip(1)
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .collect();

        // ONLY use Phosphor's own Songlengths.md5 at <config_dir>/Songlengths.md5.
        // No fallback to `last_songlength_file` (which kept pointing at the
        // stale HVSC/DOCUMENTS/ copy after a sync) and no `auto_load` to
        // legacy paths. The auto-download writes to exactly this location,
        // so a fresh download is always picked up on next boot.
        let songlength_db = config::songlength_db_path()
            .filter(|p| p.exists())
            .and_then(|p| {
                eprintln!("[phosphor] Loading Songlengths.md5 at {}", p.display());
                SonglengthDb::load(&p).ok()
            });

        // Keep `last_songlength_file` in sync so Settings UI shows the same path.
        if let Some(p) = config::songlength_db_path() {
            let s = p.to_string_lossy().into_owned();
            if config.last_songlength_file.as_deref() != Some(s.as_str()) {
                config.last_songlength_file = Some(s);
                config.save();
            }
        }

        // Load STIL database from remembered path, then from default config path.
        let stil_db = config
            .last_stil_file
            .as_ref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.exists())
            .and_then(|p| {
                eprintln!("[phosphor] Loading remembered STIL.txt at {}", p.display());
                stil::StilDb::load(&p).ok()
            })
            .or_else(|| {
                stil::stil_db_path().filter(|p| p.exists()).and_then(|p| {
                    eprintln!("[phosphor] Found STIL.txt at {}", p.display());
                    stil::StilDb::load(&p).ok()
                })
            });
        let filtered_indices: Vec<usize> = (0..playlist.len()).collect();
        let default_length_text = if config.default_song_length_secs > 0 {
            config.default_song_length_secs.to_string()
        } else {
            String::new()
        };
        let base_font_size_text = format!("{}", config.base_font_size);
        let proxy_url_text = config.proxy_url.clone().unwrap_or_default();

        let favorites = FavoritesDb::load();
        let recently_played = RecentlyPlayed::load();
        let heard_db = HeardDb::load();
        let window_width = config.window_width_saved;
        let window_height = config.window_height_saved;

        // Remote control HTTP server.
        let remote_state = Arc::new(Mutex::new(remote::SharedRemoteState::default()));
        let (remote_cmd_tx, remote_cmd_rx) = crossbeam_channel::bounded(32);
        let http_port_text = config.http_remote_port.to_string();
        let mut http_remote_running = false;
        if config.http_remote_enabled {
            remote::start_server(
                config.http_remote_port,
                Arc::clone(&remote_state),
                remote_cmd_tx.clone(),
            );
            http_remote_running = true;
        }

        // Snapshot fields needed for auto-download before config moves into app.
        // Both Songlengths.md5 and STIL.txt are fetched as DOCUMENTS/*.* relative
        // to the single hvsc_rsync_url — one source of truth.
        let auto_hvsc_base = config.hvsc_rsync_url.clone();
        let auto_last_sl_file = config.last_songlength_file.clone();
        let auto_last_stil_file = config.last_stil_file.clone();
        let initial_show_welcome = !config.has_seen_welcome;

        let app = Self {
            cmd_tx,
            status_rx,
            status: PlayerStatus::default(),
            playlist,
            selected: None,
            visualizer: Visualizer::new(),
            songlength_db,
            search_text: String::new(),
            filtered_indices,
            sort_column: SortColumn::Index,
            sort_direction: SortDirection::Ascending,
            config,
            show_settings: false,
            show_device_config: false,
            device_cfg: None,
            device_cfg_status: String::new(),
            usb_info_fetched: false,
            device_cfg_rx,
            default_length_text,
            base_font_size_text,
            proxy_url_text,
            show_welcome: initial_show_welcome,
            sleep_deadline: None,
            sleep_selected_mins: None,
            download_status: String::new(),
            auto_download_status: String::new(),
            pending_auto_downloads: 0,
            loading_progress: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            pending_entries: None,
            scroll_to_current: false,
            new_version: None,
            favorites,
            favorites_only: false,
            window_width,
            window_height,
            recently_played,
            heard_db,
            heard_text: String::new(),
            hvsc_remote_version: None,
            hvsc_status_text: String::new(),
            hvsc_update_available: false,
            show_recently_played: false,
            show_sid_panel: false,
            show_hvsc_browser: false,
            hvsc_browser: hvsc_browser::HvscBrowser::new(initial_hvsc_root),
            browser_source: hvsc_browser::BrowserSource::from_config_str(&initial_browser_src),
            assembly64_browser: {
                let mut b = assembly64_browser::Assembly64Browser::new();
                if let Some(q) = initial_assembly64_query.clone() {
                    b.set_query(q);
                }
                b
            },
            assembly64_client: assembly64::Assembly64Client::new(),
            published_playlists_browser: {
                let mut b = published_playlists_browser::PublishedPlaylistsBrowser::new();
                b.restore_last_synced(initial_published_last_synced);
                b
            },
            published_playlists_client: published_playlists::PublishedPlaylistsClient::new(),
            session_mode: SessionMode::default(),
            playlist_scroll_offset_y: 0.0,
            // Use the saved window height as a reasonable first-frame estimate;
            // the real value arrives with the first PlaylistScrolled event.
            playlist_viewport_height: window_height,
            playlist_viewport_y: 0.0,
            pixel_ratio: 1.0,
            context_menu: None,
            silence_frames: 0,
            last_advance_at: None,
            advance_suppress_logged: false,
            vis_expanded: false,
            show_help: false,
            mini_mode: false,
            window_id: None,
            stil_scroll_x: 0.0,
            vis_expanded_info: None,
            tracker_history: TrackerHistory::new(),
            tracker_view: TrackerView::new(),
            stil_db,
            stil_entry: None,
            show_stil_overlay: false,
            stil_status: String::new(),
            hvsc_sync: None,
            hvsc_sync_status: initial_hvsc_status,
            hvsc_sync_progress: None,
            stil_display_text: String::new(),
            karaoke_flag_times: Vec::new(),
            karaoke_has_flags: false,
            karaoke_groups: Vec::new(),
            karaoke_line: 0,
            last_flag_count: 0,
            session_loaded: false,
            tick: 0,
            remote_state,
            remote_cmd_rx,
            remote_cmd_tx,
            http_remote_running,
            http_port_text,
        };

        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let version_task = Task::perform(
            async move { version_check::check_github_release(&current_version).await },
            Message::VersionCheckDone,
        );

        // Kick off HVSC version check using the HVSC base URL — internally
        // hits <base>/DOCUMENTS/STIL.txt with a 1 KB Range request.
        let hvsc_check_url = auto_hvsc_base.clone();
        let hvsc_task = Task::perform(
            async move {
                match stil::check_hvsc_update(&hvsc_check_url).await {
                    Ok(info) => Ok(info.remote_version),
                    Err(e) => Err(e),
                }
            },
            Message::HvscCheckDone,
        );

        // Auto-download missing HVSC databases on first launch (or if files are gone).
        // Both downloads run in the background — the app is fully usable immediately.
        let songlength_missing = auto_last_sl_file
            .as_ref()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(true)
            && config::songlength_db_path()
                .map(|p| !p.exists())
                .unwrap_or(true);

        let stil_missing = auto_last_stil_file
            .as_ref()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(true)
            && stil::stil_db_path().map(|p| !p.exists()).unwrap_or(true);

        // Load session playlist / CLI args in the background so the UI
        // appears immediately even with tens of thousands of entries.
        // Set progress text NOW so the first rendered frame shows the indicator.
        let has_startup_work = if !cli_paths.is_empty() {
            if let Ok(mut pg) = app.loading_progress.lock() {
                *pg = "⏳ Loading files…".to_string();
            }
            true
        } else {
            // Check if a session file exists to restore.
            let session_exists = config::config_dir()
                .map(|d| d.join("session_playlist.m3u"))
                .map(|p| p.exists())
                .unwrap_or(false);
            if session_exists {
                if let Ok(mut pg) = app.loading_progress.lock() {
                    *pg = "⏳ Restoring session…".to_string();
                }
            }
            session_exists
        };
        let startup_progress = app.loading_progress.clone();
        let session_task = if has_startup_work {
            Task::perform(
                async move { playlist::parse_startup(cli_paths, startup_progress) },
                Message::SessionLoaded,
            )
        } else {
            // Nothing to load — mark session as loaded immediately.
            Task::perform(async { Vec::new() }, Message::SessionLoaded)
        };

        // Refresh the Published Playlists manifest quietly in the background
        // so opening the panel shows up-to-date content immediately. Failures
        // are silent — the user can still hit ⟳ Sync now inside the panel.
        let published_playlists_task = Task::perform(
            async { Message::PublishedPlaylistsSyncStart },
            std::convert::identity,
        );

        let mut tasks = vec![
            version_task,
            hvsc_task,
            session_task,
            published_playlists_task,
        ];
        let mut auto_status_parts: Vec<&str> = vec![];

        if songlength_missing {
            eprintln!("[phosphor] Songlength DB not found — auto-downloading");
            let base = auto_hvsc_base.clone();
            tasks.push(Task::perform(
                config::download_songlength(base),
                Message::SonglengthDownloaded,
            ));
            auto_status_parts.push("Songlengths");
        }

        if stil_missing {
            eprintln!("[phosphor] STIL.txt not found — auto-downloading");
            let base = auto_hvsc_base.clone();
            tasks.push(Task::perform(
                stil::download_stil(base),
                Message::StilDownloaded,
            ));
            auto_status_parts.push("STIL");
        }

        let mut app = app;
        let n = auto_status_parts.len() as u8;
        app.pending_auto_downloads = n;
        if n > 0 {
            app.auto_download_status = format!("⬇ Downloading {}…", auto_status_parts.join(" & "));
        }
        // If STIL.txt was loaded from disk above, the local HVSC version is
        // already known. Seed the status-bar indicator now so it's visible
        // immediately, before the remote check returns.
        app.refresh_hvsc_status();

        (app, Task::batch(tasks))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let _perf = ProfilerGuard::update();
        match message {
            // ── Any interaction dismisses the context menu ────────────────
            // (handled per-message below where needed; explicit dismiss too)

            // ── Context menu actions ──────────────────────────────────────
            Message::ShowContextMenu(idx, x, y) => {
                // cursor.position() in a widget's update() returns physical pixels on
                // HiDPI/Retina displays, but iced's layout coordinates are logical.
                // We derive the pixel ratio by comparing the raw cursor y to the
                // known logical y of the clicked row.  Once calibrated the ratio
                // is stored and reused for x as well.
                //
                // Logical row centre y =
                //   playlist_viewport_y + display_pos*row_height() - scroll_offset_y + row_height()/2
                //
                // pixel_ratio = raw_cursor_y / logical_row_y
                // We round to the nearest common value (1.0, 1.5, 2.0, 3.0) to avoid
                // noise from the cursor not being exactly at row centre.

                let display_pos = self
                    .filtered_indices
                    .iter()
                    .position(|&i| i == idx)
                    .unwrap_or(0) as f32;

                // Logical y of the centre of the clicked row.
                let logical_row_y = self.playlist_viewport_y + display_pos * ui::row_height()
                    - self.playlist_scroll_offset_y
                    + ui::row_height() * 0.5;

                // Calibrate ratio when we have valid logical geometry.
                if logical_row_y > 0.0 && y > 0.0 {
                    let raw_ratio = y / logical_row_y;
                    // Round to nearest 0.25 to absorb sub-pixel noise, then clamp to
                    // sane values (0.5 – 4.0).
                    let snapped = ((raw_ratio * 4.0).round() / 4.0).clamp(0.5, 4.0);
                    self.pixel_ratio = snapped;
                }

                let ratio = self.pixel_ratio;
                let menu_x = (x / ratio).max(0.0);
                let menu_y = (self.playlist_viewport_y + display_pos * ui::row_height()
                    - self.playlist_scroll_offset_y
                    + ui::row_height())
                .max(0.0);

                eprintln!("[phosphor] ShowContextMenu idx={idx} raw=({x:.1},{y:.1}) ratio={ratio:.2} menu=({menu_x:.1},{menu_y:.1})");

                self.context_menu = Some(ContextMenu {
                    track_idx: idx,
                    x: menu_x,
                    y: menu_y,
                });
                self.selected = Some(idx);
            }

            Message::DismissContextMenu => {
                self.context_menu = None;
            }

            Message::ContextMenuPlay => {
                if let Some(cm) = self.context_menu.take() {
                    self.play_track(cm.track_idx);
                }
            }

            Message::ContextMenuRemove => {
                if let Some(cm) = self.context_menu.take() {
                    let idx = cm.track_idx;
                    if self.playlist.current == Some(idx) {
                        let _ = self.cmd_tx.send(PlayerCmd::Stop);
                    }
                    self.playlist.remove(idx);
                    self.selected = if self.playlist.is_empty() {
                        None
                    } else {
                        Some(idx.min(self.playlist.len() - 1))
                    };
                    self.rebuild_filter();
                }
            }

            Message::ContextMenuMoveToTop => {
                if let Some(cm) = self.context_menu.take() {
                    let idx = cm.track_idx;
                    if idx > 0 && idx < self.playlist.entries.len() {
                        let entry = self.playlist.entries.remove(idx);
                        self.playlist.entries.insert(0, entry);
                        // Fix up current index if it moved
                        if let Some(cur) = self.playlist.current {
                            self.playlist.current = Some(if cur == idx {
                                0
                            } else if cur < idx {
                                cur + 1
                            } else {
                                cur
                            });
                        }
                        self.selected = Some(0);
                        self.rebuild_filter();
                    }
                }
            }

            Message::ContextMenuToggleFavorite => {
                if let Some(cm) = self.context_menu.take() {
                    if let Some(entry) = self.playlist.entries.get(cm.track_idx) {
                        if let Some(ref md5) = entry.md5 {
                            let is_fav = self.favorites.toggle(md5);
                            self.favorites.save();
                            eprintln!(
                                "[phosphor] {} \"{}\" via context menu",
                                if is_fav {
                                    "♥ Favorited"
                                } else {
                                    "♡ Unfavorited"
                                },
                                entry.title,
                            );
                            if self.favorites_only {
                                self.rebuild_filter();
                            }
                        }
                    }
                }
            }

            Message::ContextMenuCopyTitle => {
                if let Some(cm) = self.context_menu.take() {
                    if let Some(entry) = self.playlist.entries.get(cm.track_idx) {
                        let title = entry.title.clone();
                        return iced::clipboard::write(title);
                    }
                }
            }

            // ── Transport ────────────────────────────────────────────────
            Message::PlayPause => {
                self.context_menu = None;
                if self.status.state == PlayState::Stopped {
                    let idx = self.selected.or(Some(0));
                    if let Some(i) = idx {
                        self.play_track(i);
                    }
                } else {
                    let _ = self.cmd_tx.send(PlayerCmd::TogglePause);
                    // User-initiated pause/resume — drop the debounce so the
                    // first auto-advance after resume isn't gated by a stale
                    // timestamp from before pause.
                    self.last_advance_at = None;
                }
            }

            Message::Stop => {
                self.context_menu = None;
                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                self.visualizer.reset();
                self.tracker_history.reset();
                self.tracker_view.reset();
                self.last_advance_at = None;
            }

            Message::NextTrack => {
                self.context_menu = None;
                if let Some(idx) = self.playlist.next() {
                    self.play_track(idx);
                }
            }

            Message::PrevTrack => {
                self.context_menu = None;
                if self.status.elapsed.as_secs() > 3 {
                    if let Some(idx) = self.playlist.current {
                        self.play_track(idx);
                    }
                } else if let Some(idx) = self.playlist.prev() {
                    self.play_track(idx);
                }
            }

            // ── Sub-tunes ────────────────────────────────────────────────
            Message::NextSubtune => {
                if let Some(ref info) = self.status.track_info {
                    let next = (info.current_song + 1).min(info.songs);
                    if next != info.current_song {
                        let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(next));
                        self.clear_advance_status();
                        self.update_entry_subtune(next);
                    }
                }
            }

            Message::PrevSubtune => {
                if let Some(ref info) = self.status.track_info {
                    let prev = info.current_song.saturating_sub(1).max(1);
                    if prev != info.current_song {
                        let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(prev));
                        self.clear_advance_status();
                        self.update_entry_subtune(prev);
                    }
                }
            }

            // ── Playlist interaction ─────────────────────────────────────
            Message::PlaylistSelect(idx) => {
                self.context_menu = None;
                if self.selected == Some(idx) {
                    self.play_track(idx);
                } else {
                    self.selected = Some(idx);
                }
            }

            Message::PlaylistDoubleClick(idx) => {
                self.context_menu = None;
                self.play_track(idx);
            }

            Message::AddFiles => {
                self.context_menu = None;
                let start_dir = self.config.last_sid_dir.clone();
                return Task::perform(pick_files(start_dir), Message::FilesChosen);
            }

            Message::AddFolder => {
                self.context_menu = None;
                let start_dir = self.config.last_sid_dir.clone();
                return Task::perform(pick_folder(start_dir), Message::FolderChosen);
            }

            Message::ClearPlaylist => {
                self.context_menu = None;
                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                self.playlist.clear();
                self.selected = None;
                self.visualizer.reset();
                self.rebuild_filter();
            }

            Message::RemoveSelected => {
                self.context_menu = None;
                if let Some(idx) = self.selected {
                    if self.playlist.current == Some(idx) {
                        let _ = self.cmd_tx.send(PlayerCmd::Stop);
                    }
                    self.playlist.remove(idx);
                    self.selected = if self.playlist.is_empty() {
                        None
                    } else {
                        Some(idx.min(self.playlist.len() - 1))
                    };
                    self.rebuild_filter();
                }
            }

            // ── Modes ────────────────────────────────────────────────────
            Message::ToggleShuffle => {
                self.context_menu = None;
                self.playlist.toggle_shuffle();
            }
            Message::CycleRepeat => {
                self.context_menu = None;
                self.playlist.cycle_repeat();
            }

            // ── Sub-tunes ────────────────────────────────────────────────

            // ── Songlength ───────────────────────────────────────────────
            Message::LoadSonglength => {
                self.context_menu = None;
                let start_dir = self.config.last_songlength_dir.clone();
                return Task::perform(
                    pick_songlength_file(start_dir),
                    Message::SonglengthFileChosen,
                );
            }

            // ── Playlist save / load ─────────────────────────────────────
            Message::SavePlaylist => {
                self.context_menu = None;
                if self.playlist.is_empty() {
                    return Task::none();
                }
                let entries: Vec<(PathBuf, String, String, Option<u32>)> = self
                    .playlist
                    .entries
                    .iter()
                    .map(|e| {
                        (
                            e.path.clone(),
                            e.author.clone(),
                            e.title.clone(),
                            e.duration_secs,
                        )
                    })
                    .collect();
                let start_dir = self.config.last_playlist_dir.clone();
                return Task::perform(
                    save_playlist_dialog(entries, start_dir),
                    Message::PlaylistSaved,
                );
            }

            Message::LoadPlaylist => {
                self.context_menu = None;
                let start_dir = self.config.last_playlist_dir.clone();
                return Task::perform(pick_playlist_file(start_dir), Message::PlaylistFileChosen);
            }

            // ── Async results ────────────────────────────────────────────
            Message::FilesChosen(paths) => {
                if paths.is_empty() {
                    return Task::none();
                }
                if let Some(first) = paths.first() {
                    self.config.remember_sid_dir(first);
                }
                let pg = self.loading_progress.clone();
                return Task::perform(
                    async move { playlist::parse_files(paths, pg) },
                    Message::FilesLoaded,
                );
            }

            Message::FolderChosen(Some(path)) => {
                self.config.remember_sid_dir(&path);
                let pg = self.loading_progress.clone();
                return Task::perform(
                    async move { playlist::parse_directory(path, pg) },
                    Message::FolderLoaded,
                );
            }
            Message::FolderChosen(None) => {}

            Message::FilesLoaded(entries) => {
                if entries.is_empty() {
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        pg.clear();
                    }
                } else {
                    // Extend rather than replace — multiple drops may arrive
                    // concurrently and each resolves as a separate FilesLoaded.
                    let pending = self.pending_entries.get_or_insert_with(Vec::new);
                    pending.extend(entries);
                    let n = pending.len();
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        *pg = format!("⏳ Adding {} tracks…", n);
                    }
                    return Task::perform(flush_frame(), |_| Message::ProcessPendingEntries);
                }
            }

            Message::FolderLoaded(entries) => {
                if entries.is_empty() {
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        pg.clear();
                    }
                } else {
                    let n = entries.len();
                    self.pending_entries = Some(entries);
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        *pg = format!("⏳ Adding {} tracks…", n);
                    }
                    return Task::perform(flush_frame(), |_| Message::ProcessPendingEntries);
                }
            }

            Message::SessionLoaded(entries) => {
                self.session_loaded = true;
                if entries.is_empty() {
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        pg.clear();
                    }
                } else {
                    let n = entries.len();
                    eprintln!("[phosphor] Session loaded: {n} tracks (background)");
                    self.pending_entries = Some(entries);
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        *pg = format!("⏳ Adding {} tracks…", n);
                    }
                    return Task::perform(flush_frame(), |_| Message::ProcessPendingEntries);
                }
            }

            Message::FileDropped(path) => {
                self.context_menu = None;
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                match ext.as_str() {
                    "sid" | "mus" => {
                        self.config.remember_sid_dir(&path);
                        let paths = vec![path];
                        let pg = self.loading_progress.clone();
                        return Task::perform(
                            async move { playlist::parse_files(paths, pg) },
                            Message::FilesLoaded,
                        );
                    }
                    "md5" | "txt" => {
                        self.config.remember_songlength_path(&path);
                        match SonglengthDb::load(&path) {
                            Ok(db) => {
                                let count = db.entries.len();
                                db.apply_to_playlist(
                                    &mut self.playlist,
                                    self.config.hvsc_root.as_deref().map(std::path::Path::new),
                                );
                                if self.config.default_song_length_secs > 0 {
                                    apply_default_length(
                                        &mut self.playlist,
                                        self.config.default_song_length_secs,
                                    );
                                }
                                self.songlength_db = Some(db);
                                self.download_status =
                                    format!("Loaded {} entries from dropped file", count);
                            }
                            Err(e) => eprintln!("[phosphor] Dropped file failed to load: {e}"),
                        }
                    }
                    "m3u" | "m3u8" | "pls" => {
                        self.config.remember_playlist_dir(&path);
                        let pg = self.loading_progress.clone();
                        return Task::perform(
                            async move { playlist::parse_playlist_file(path, pg) },
                            Message::PlaylistLoaded,
                        );
                    }
                    _ => {
                        if path.is_dir() {
                            self.config.remember_sid_dir(&path);
                            let pg = self.loading_progress.clone();
                            return Task::perform(
                                async move { playlist::parse_directory(path, pg) },
                                Message::FolderLoaded,
                            );
                        }
                    }
                }
            }

            Message::SonglengthFileChosen(Some(path)) => {
                self.config.remember_songlength_path(&path);
                match SonglengthDb::load(&path) {
                    Ok(db) => {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                        self.songlength_db = Some(db);
                    }
                    Err(e) => eprintln!("[phosphor] Failed to load Songlength DB: {e}"),
                }
            }
            Message::SonglengthFileChosen(None) => {}

            Message::PlaylistSaved(Ok(path)) => {
                self.config.remember_playlist_dir(&path);
                eprintln!("[phosphor] Playlist saved to {}", path.display());
            }
            Message::PlaylistSaved(Err(e)) => eprintln!("[phosphor] Save failed: {e}"),

            Message::PlaylistFileChosen(Some(path)) => {
                self.config.remember_playlist_dir(&path);
                let pg = self.loading_progress.clone();
                return Task::perform(
                    async move { playlist::parse_playlist_file(path, pg) },
                    Message::PlaylistLoaded,
                );
            }
            Message::PlaylistFileChosen(None) => {}

            Message::PlaylistLoaded(Ok(entries)) => {
                if entries.is_empty() {
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        pg.clear();
                    }
                } else {
                    let n = entries.len();
                    eprintln!("[phosphor] Loaded {} tracks from playlist", n);
                    self.pending_entries = Some(entries);
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        *pg = format!("⏳ Adding {} tracks…", n);
                    }
                    return Task::perform(flush_frame(), |_| Message::ProcessPendingEntries);
                }
            }
            Message::PlaylistLoaded(Err(e)) => {
                eprintln!("[phosphor] Failed to load playlist: {e}");
                if let Ok(mut pg) = self.loading_progress.lock() {
                    pg.clear();
                }
            }

            Message::ProcessPendingEntries => {
                if let Some(entries) = self.pending_entries.take() {
                    let n = entries.len();
                    self.playlist.add_entries(entries);
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        *pg = format!("⏳ Applying songlengths to {} tracks…", n);
                    }
                    return Task::perform(flush_frame(), |_| Message::FinalizePendingEntries);
                } else {
                    if let Ok(mut pg) = self.loading_progress.lock() {
                        pg.clear();
                    }
                }
            }
            Message::FinalizePendingEntries => {
                self.session_loaded = true;
                self.apply_songlengths();
                self.rebuild_filter();
                if let Ok(mut pg) = self.loading_progress.lock() {
                    pg.clear();
                }
            }

            // ── Search / filter ───────────────────────────────────────────
            Message::SearchChanged(query) => {
                self.context_menu = None;
                self.search_text = query;
                self.rebuild_filter();
            }

            Message::ClearSearch => {
                self.context_menu = None;
                self.search_text.clear();
                self.rebuild_filter();
            }

            // ── Sort ──────────────────────────────────────────────────────
            Message::SortBy(col) => {
                self.context_menu = None;
                if self.sort_column == col {
                    self.sort_direction = self.sort_direction.flip();
                } else {
                    self.sort_column = col;
                    self.sort_direction = SortDirection::Ascending;
                }
                self.rebuild_filter();
            }

            // ── Keyboard navigation ───────────────────────────────────────
            Message::SelectNext => {
                self.context_menu = None;
                if self.filtered_indices.is_empty() {
                    return Task::none();
                }
                let cur = self
                    .selected
                    .and_then(|s| self.filtered_indices.iter().position(|&i| i == s))
                    .unwrap_or(0);
                let next = (cur + 1).min(self.filtered_indices.len() - 1);
                self.selected = Some(self.filtered_indices[next]);
                let total = self.filtered_indices.len();
                // Update virtual scroll offset immediately so the newly selected
                // row is included in the render window before the snap_to fires.
                self.playlist_scroll_offset_y = next as f32 * ui::row_height();
                if total > 1 {
                    return iced::widget::operation::snap_to(
                        ui::playlist_scrollable_id(),
                        iced::widget::scrollable::RelativeOffset {
                            x: 0.0,
                            y: next as f32 / (total - 1) as f32,
                        },
                    );
                }
            }

            Message::SelectPrev => {
                self.context_menu = None;
                if self.filtered_indices.is_empty() {
                    return Task::none();
                }
                let cur = self
                    .selected
                    .and_then(|s| self.filtered_indices.iter().position(|&i| i == s))
                    .unwrap_or(0);
                let prev = cur.saturating_sub(1);
                self.selected = Some(self.filtered_indices[prev]);
                let total = self.filtered_indices.len();
                // Same immediate update for SelectPrev.
                self.playlist_scroll_offset_y = prev as f32 * ui::row_height();
                if total > 1 {
                    return iced::widget::operation::snap_to(
                        ui::playlist_scrollable_id(),
                        iced::widget::scrollable::RelativeOffset {
                            x: 0.0,
                            y: prev as f32 / (total - 1) as f32,
                        },
                    );
                }
            }

            Message::FocusSearch => {
                self.context_menu = None;
                return iced::widget::operation::focus(ui::search_input_id());
            }

            // ── Recently played ───────────────────────────────────────────
            Message::ShowRecentlyPlayed => {
                self.context_menu = None;
                self.show_recently_played = !self.show_recently_played;
                if self.show_recently_played {
                    self.show_settings = false;
                    self.show_sid_panel = false;
                    self.show_hvsc_browser = false;
                }
            }

            Message::PlayRecentEntry(i) => {
                self.context_menu = None;
                if let Some(recent_entry) = self.recently_played.entries.get(i).cloned() {
                    let playlist_idx = self
                        .playlist
                        .entries
                        .iter()
                        .position(|e| e.md5.as_deref() == Some(recent_entry.md5.as_str()));
                    if let Some(idx) = playlist_idx {
                        self.show_recently_played = false;
                        self.play_track(idx);
                    } else if recent_entry.path.exists() {
                        let paths = vec![recent_entry.path.clone()];
                        let pg = self.loading_progress.clone();
                        self.show_recently_played = false;
                        return Task::perform(
                            async move { playlist::parse_files(paths, pg) },
                            Message::FilesLoaded,
                        );
                    } else {
                        eprintln!(
                            "[phosphor] Recent entry path no longer exists: {}",
                            recent_entry.path.display()
                        );
                    }
                }
            }

            Message::ClearRecentlyPlayed => {
                self.recently_played = RecentlyPlayed::default();
                self.recently_played.save();
            }

            // ── Settings ──────────────────────────────────────────────────
            Message::ToggleSettings => {
                self.context_menu = None;
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    self.show_recently_played = false;
                    self.show_sid_panel = false;
                    self.show_device_config = false;
                    self.show_hvsc_browser = false;
                }
            }

            Message::ToggleDeviceConfig => {
                self.context_menu = None;
                self.show_device_config = !self.show_device_config;
                if self.show_device_config {
                    self.show_settings = false;
                    self.show_recently_played = false;
                    self.show_sid_panel = false;
                    self.show_hvsc_browser = false;
                    // Auto-load on open.
                    self.device_cfg_status = "Reading device…".into();
                    let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                        player::DeviceConfigCmd::Refresh,
                    ));
                }
            }

            Message::DeviceConfigRefresh => {
                self.device_cfg_status = "Reading device…".into();
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::Refresh,
                ));
            }

            Message::DeviceConfigApplyPreset(p) => {
                self.device_cfg_status = format!("Applying preset: {}…", p.label());
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::ApplyPreset(p),
                ));
            }

            Message::DeviceConfigSetClock(rate) => {
                self.device_cfg_status = format!("Setting clock: {}…", rate.label());
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::SetClock(rate),
                ));
            }

            Message::DeviceConfigEdit(edit) => {
                self.device_cfg_status = "Updating…".into();
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::Edit(edit),
                ));
            }

            Message::DeviceConfigSave => {
                self.device_cfg_status = "Saving to flash…".into();
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::Save,
                ));
            }

            Message::DeviceConfigReset => {
                self.device_cfg_status = "Resetting to factory defaults…".into();
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::Reset,
                ));
            }

            Message::DeviceConfigAutoDetect => {
                self.device_cfg_status = "Auto-detecting (≈3 s)…".into();
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::AutoDetect,
                ));
            }

            Message::DeviceConfigAction(cmd) => {
                use player::DeviceConfigCmd as C;
                self.device_cfg_status = match &cmd {
                    C::Confirm => "Confirming config…".into(),
                    C::DetectSids => "Detecting SIDs…".into(),
                    C::DetectClones => "Detecting clones (≈2 s)…".into(),
                    C::TestSid(0) => "Test tone: all SIDs".into(),
                    C::TestSid(n) => format!("Test tone: SID{n}"),
                    C::StopTests => "Stopping test tones".into(),
                    C::ResetUsbsid => "Resetting USBSID — reconnecting…".into(),
                    C::RestartBus => "Restarting SID bus".into(),
                    C::RestartBusClk => "Restarting bus + clock".into(),
                    C::SyncPios => "Sync PIOs".into(),
                    C::SocketDetect => "Re-detecting sockets…".into(),
                    C::MidiLoadState => "Loading MIDI state".into(),
                    C::MidiSaveState => "Saving MIDI state".into(),
                    C::MidiResetState => "Resetting MIDI state".into(),
                    other => format!("Sending {other:?}…"),
                };
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(cmd));
            }

            Message::DeviceConfigResult(result) => match result {
                Ok(snap) => {
                    self.device_cfg = Some(snap);
                    self.device_cfg_status = "Loaded.".into();
                }
                Err(e) => {
                    self.device_cfg_status = format!("Error: {e}");
                }
            },

            Message::ToggleSkipRsid => {
                self.config.skip_rsid = !self.config.skip_rsid;
                self.config.save();
            }
            Message::ToggleForceStereo2sid => {
                self.config.force_stereo_2sid = !self.config.force_stereo_2sid;
                self.config.save();
            }

            #[cfg(target_os = "macos")]
            Message::SetMacosUsbMode(mode) => {
                if mode != self.config.macos_usb_mode {
                    eprintln!("[phosphor] macOS USB mode → {mode}");
                    // When switching to direct, stop the bridge daemon so it
                    // releases the USB handle. When switching back to bridge,
                    // ensure_daemon() will re-install / restart it on the
                    // next Play (the existing BridgeDevice::connect path).
                    if mode == "direct" {
                        if let Err(e) = crate::daemon_installer::stop_daemon() {
                            eprintln!("[phosphor] couldn't stop bridge daemon: {e}");
                        }
                    }
                    self.config.macos_usb_mode = mode.clone();
                    self.config.save();
                    let _ = self.cmd_tx.send(PlayerCmd::SetMacosUsbMode(mode));
                }
            }
            #[cfg(not(target_os = "macos"))]
            Message::SetMacosUsbMode(_) => {}

            Message::DefaultSongLengthChanged(val) => {
                self.default_length_text = val.clone();
                let new_val = val.trim().parse::<u32>().unwrap_or(0);
                if new_val != self.config.default_song_length_secs {
                    self.config.default_song_length_secs = new_val;
                    self.config.save();
                    if new_val > 0 {
                        apply_default_length(&mut self.playlist, new_val);
                    } else {
                        clear_default_lengths(&mut self.playlist);
                        self.apply_songlengths();
                    }
                }
            }

            Message::VolumeChanged(v) => {
                let clamped = v.clamp(0.0, 1.0);
                if (clamped - self.config.master_volume).abs() > 1e-4 {
                    self.config.master_volume = clamped;
                    crate::audio_volume::set(clamped);
                    self.config.save();
                }
            }

            Message::BaseFontSizeChanged(val) => {
                // Echo the keystroke back into the input buffer so the
                // field remains editable mid-type. On a successful parse
                // commit the clamped value and re-seed the global scale.
                self.base_font_size_text = val.clone();
                if let Ok(parsed) = val.trim().parse::<f32>() {
                    let clamped = parsed.clamp(8.0, 32.0);
                    if (clamped - self.config.base_font_size).abs() > f32::EPSILON {
                        self.config.base_font_size = clamped;
                        crate::ui::font::set_base(clamped);
                        self.config.save();
                    }
                }
            }

            // ── HTTP proxy ─────────────────────────────────────────────
            Message::ProxyUrlChanged(val) => {
                // Live-update the draft only; don't touch config or HTTP
                // clients until the user clicks Apply.
                self.proxy_url_text = val;
            }

            Message::ProxyApply => {
                let trimmed = self.proxy_url_text.trim().to_string();
                let new_val = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                if new_val != self.config.proxy_url {
                    self.config.proxy_url = new_val;
                    self.config.save();
                    eprintln!(
                        "[phosphor] Proxy applied: {:?}. Next outbound request \
                         will use the new setting.",
                        self.config.proxy_url
                    );
                }
            }

            Message::ProxyClear => {
                self.proxy_url_text.clear();
                if self.config.proxy_url.is_some() {
                    self.config.proxy_url = None;
                    self.config.save();
                    eprintln!("[phosphor] Proxy cleared.");
                }
            }

            Message::SetOutputEngine(engine) => {
                if engine != self.config.output_engine {
                    self.config.output_engine = engine.clone();
                    self.config.save();
                    // Remember if something was playing so we can resume.
                    let was_playing = self.status.state == PlayState::Playing;
                    let cur_idx = self.playlist.current;
                    let _ = self.cmd_tx.try_send(PlayerCmd::SetEngine(
                        engine,
                        self.config.u64_address.clone(),
                        self.config.u64_password.clone(),
                    ));
                    // Auto-resume on the new engine.
                    if was_playing {
                        if let Some(idx) = cur_idx {
                            self.play_track(idx);
                        }
                    }
                }
            }

            Message::SetU64Address(addr) => {
                self.config.u64_address = addr;
                self.config.save();
                let _ = self.cmd_tx.try_send(PlayerCmd::UpdateU64Config(
                    self.config.u64_address.clone(),
                    self.config.u64_password.clone(),
                ));
            }

            Message::SetU64Password(pass) => {
                self.config.u64_password = pass;
                self.config.save();
                let _ = self.cmd_tx.try_send(PlayerCmd::UpdateU64Config(
                    self.config.u64_address.clone(),
                    self.config.u64_password.clone(),
                ));
            }

            Message::DownloadSonglength => {
                self.download_status = "Downloading...".to_string();
                let url = self.config.hvsc_rsync_url.clone();
                return Task::perform(
                    config::download_songlength(url),
                    Message::SonglengthDownloaded,
                );
            }

            Message::SonglengthDownloaded(Ok(path)) => match SonglengthDb::load(&path) {
                Ok(db) => {
                    let count = db.entries.len();
                    db.apply_to_playlist(
                        &mut self.playlist,
                        self.config.hvsc_root.as_deref().map(std::path::Path::new),
                    );
                    if self.config.default_song_length_secs > 0 {
                        apply_default_length(
                            &mut self.playlist,
                            self.config.default_song_length_secs,
                        );
                    }
                    self.songlength_db = Some(db);
                    self.update_auto_download_status();
                    self.download_status = format!(
                        "Download success! Loaded {} entries from {}",
                        count,
                        path.display()
                    );
                }
                Err(e) => {
                    self.update_auto_download_status();
                    self.download_status = format!("Error loading DB: {e}");
                }
            },
            Message::SonglengthDownloaded(Err(e)) => {
                self.update_auto_download_status();
                self.download_status = format!("Error: {e}");
                eprintln!("[phosphor] Songlength download failed: {e}");
            }

            // ── Favorites ─────────────────────────────────────────────────
            Message::ToggleFavorite(idx) => {
                if let Some(entry) = self.playlist.entries.get(idx) {
                    if let Some(ref md5) = entry.md5 {
                        let is_fav = self.favorites.toggle(md5);
                        self.favorites.save();
                        eprintln!(
                            "[phosphor] {} \"{}\" ({})",
                            if is_fav {
                                "♥ Favorited"
                            } else {
                                "♡ Unfavorited"
                            },
                            entry.title,
                            md5
                        );
                        if self.favorites_only {
                            self.rebuild_filter();
                        }
                    }
                }
            }

            Message::ToggleFavoritesFilter => {
                self.context_menu = None;
                self.favorites_only = !self.favorites_only;
                self.rebuild_filter();
            }

            Message::FavoriteNowPlaying => {
                if let Some(idx) = self.playlist.current {
                    if let Some(entry) = self.playlist.entries.get(idx) {
                        if let Some(ref md5) = entry.md5 {
                            let is_fav = self.favorites.toggle(md5);
                            self.favorites.save();
                            eprintln!(
                                "[phosphor] {} \"{}\"",
                                if is_fav { "♥" } else { "♡" },
                                entry.title
                            );
                            if self.favorites_only {
                                self.rebuild_filter();
                            }
                        }
                    }
                }
            }

            Message::ScrollToNowPlaying => {
                if let Some(cur_idx) = self.playlist.current {
                    if let Some(pos) = self.filtered_indices.iter().position(|&i| i == cur_idx) {
                        let total = self.filtered_indices.len();
                        if total > 1 {
                            return iced::widget::operation::snap_to(
                                ui::playlist_scrollable_id(),
                                iced::widget::scrollable::RelativeOffset {
                                    x: 0.0,
                                    y: pos as f32 / (total - 1) as f32,
                                },
                            );
                        }
                    }
                }
            }

            // ── Tick ──────────────────────────────────────────────────────
            Message::Tick => {
                self.tick = self.tick.wrapping_add(1);
                self.poll_status();

                // Sleep-timer expiry: stop playback once we cross the deadline,
                // then clear the timer so it doesn't fire repeatedly.
                if let Some(deadline) = self.sleep_deadline {
                    if Instant::now() >= deadline {
                        eprintln!("[sleep] timer expired — stopping playback");
                        self.sleep_deadline = None;
                        self.sleep_selected_mins = None;
                        let _ = self.cmd_tx.send(PlayerCmd::Stop);
                    }
                }

                // Feed tracker history every tick while playing.
                if self.status.state == PlayState::Playing {
                    let num_sids = self
                        .status
                        .track_info
                        .as_ref()
                        .map(|i| i.num_sids)
                        .unwrap_or(1);
                    let is_pal = self
                        .status
                        .track_info
                        .as_ref()
                        .map(|i| i.is_pal)
                        .unwrap_or(true);
                    self.tracker_history
                        .push(&self.status.sid_regs, num_sids, is_pal);
                    self.tracker_view.invalidate();
                }
                // Refresh HVSC completion string for status bar.
                {
                    let total = self
                        .songlength_db
                        .as_ref()
                        .map(|db| db.entries.len())
                        .unwrap_or(0);
                    self.heard_text = self.heard_db.format_completion(total);
                }
                // Advance STIL ticker when tracker is in full-screen mode.
                if self.vis_expanded
                    && matches!(
                        self.visualizer.mode,
                        ui::visualizer::VisMode::Tracker | ui::visualizer::VisMode::Karaoke
                    )
                {
                    // ~80 logical px/s at ~30 fps
                    self.stil_scroll_x += 2.65;
                    self.visualizer.invalidate_expanded();
                }
                // Keep STIL text in sync with current subtune.
                // Only rebuild on tick for STIL entries (subtune may change).
                // WDS lyrics are static per track and loaded once via refresh_stil_entry.
                if self.stil_entry.is_some() {
                    self.rebuild_stil_display();
                }
                if self.scroll_to_current {
                    self.scroll_to_current = false;
                    if let Some(cur_idx) = self.playlist.current {
                        if let Some(pos) = self.filtered_indices.iter().position(|&i| i == cur_idx)
                        {
                            let total = self.filtered_indices.len();
                            if total > 1 {
                                return iced::widget::operation::snap_to(
                                    ui::playlist_scrollable_id(),
                                    iced::widget::scrollable::RelativeOffset {
                                        x: 0.0,
                                        y: pos as f32 / (total - 1) as f32,
                                    },
                                );
                            }
                        }
                    }
                }

                // ── Remote control ──────────────────────────────────────
                if self.http_remote_running {
                    self.update_remote_state();
                    let t = self.poll_remote_commands();
                    // A returned task means the remote enqueued an op
                    // that needs to re-enter update() (Surprise / load /
                    // restore). Fire it before we return.
                    return t;
                }
            }

            Message::VersionCheckDone(Ok(Some(info))) => {
                eprintln!(
                    "[phosphor] New version available: {} → {}",
                    env!("CARGO_PKG_VERSION"),
                    info.version
                );
                self.new_version = Some(info);
            }
            Message::VersionCheckDone(Ok(None)) => eprintln!("[phosphor] Version is up to date"),
            Message::VersionCheckDone(Err(e)) => eprintln!("[phosphor] Version check failed: {e}"),

            Message::OpenUpdateUrl => {
                if let Some(ref info) = self.new_version {
                    let _ = open::that(&info.download_url);
                }
            }

            // ── Window ────────────────────────────────────────────────────
            Message::WindowResized(wid, w, h) => {
                self.window_id = Some(wid);
                self.window_width = w;
                self.window_height = h;
                // Don't overwrite saved size while in mini mode — we want to
                // restore the full window dimensions when exiting mini mode.
                if !self.mini_mode {
                    self.config.window_width_saved = w;
                    self.config.window_height_saved = h;
                    self.config.save();
                }
            }

            Message::WindowMoved(x, y) => {
                self.config.window_x = Some(x);
                self.config.window_y = Some(y);
                self.config.save();
            }

            Message::ToggleVisMode => {
                self.visualizer.toggle_mode();
            }

            Message::ToggleVisFull => {
                self.vis_expanded = !self.vis_expanded;
            }

            Message::ToggleKaraoke => {
                // Only activate if we have karaoke lyrics.
                if !self.stil_display_text.is_empty() {
                    if self.vis_expanded && self.visualizer.mode == ui::visualizer::VisMode::Karaoke
                    {
                        self.vis_expanded = false;
                    } else {
                        self.visualizer.mode = ui::visualizer::VisMode::Karaoke;
                        self.vis_expanded = true;
                    }
                }
            }

            Message::HvscCheckDone(Ok(remote_ver)) => {
                self.hvsc_remote_version = Some(remote_ver);
                self.refresh_hvsc_status();
                let local_ver = self.stil_db.as_ref().and_then(|db| db.hvsc_version);
                let info = stil::HvscUpdateInfo {
                    remote_version: remote_ver,
                    local_version: local_ver,
                };
                if info.is_newer() {
                    eprintln!("[phosphor] {}", info.description());
                    // Auto-download both updated databases — same as first-launch flow.
                    // Show status while in flight.
                    self.auto_download_status =
                        format!("⬆ {} — updating databases…", info.description());
                    self.pending_auto_downloads = 2;
                    let base = self.config.hvsc_rsync_url.clone();
                    let base2 = base.clone();
                    return Task::batch([
                        Task::perform(
                            config::download_songlength(base),
                            Message::SonglengthDownloaded,
                        ),
                        Task::perform(stil::download_stil(base2), Message::StilDownloaded),
                    ]);
                } else {
                    eprintln!("[phosphor] HVSC is up to date (v{remote_ver})");
                    self.config.hvsc_known_version = Some(format!("v{remote_ver}"));
                    self.config.save();
                }
            }

            Message::HvscCheckDone(Err(e)) => {
                eprintln!("[phosphor] HVSC version check failed: {e}");
            }

            Message::HvscUpdateAvailable(_) => {}

            Message::ToggleMiniPlayer => {
                self.mini_mode = !self.mini_mode;
                let size = if self.mini_mode {
                    iced::Size::new(ui::mini_width(), ui::mini_height())
                } else {
                    iced::Size::new(
                        self.config.window_width_saved.max(400.0),
                        self.config.window_height_saved.max(300.0),
                    )
                };
                if let Some(wid) = self.window_id {
                    return iced::window::resize(wid, size);
                }
            }

            Message::Noop => {}

            // Context-sensitive key handlers — resolved here where self is available
            Message::KeyEscape => {
                if self.show_help {
                    self.show_help = false;
                } else if self.vis_expanded {
                    self.vis_expanded = false;
                } else {
                    self.context_menu = None;
                }
            }

            Message::KeyArrowLeft => {
                return self.update(Message::PrevTrack);
            }

            Message::KeyArrowRight => {
                return self.update(Message::NextTrack);
            }

            Message::ShowHelp => {
                self.show_help = !self.show_help;
            }

            Message::DismissHelp => {
                self.show_help = false;
            }

            // ── First-run welcome card ────────────────────────────────────
            Message::WelcomeSyncHvsc => {
                self.show_welcome = false;
                self.config.has_seen_welcome = true;
                self.config.save();
                // Reuse the existing sync toggle handler.
                return iced::Task::done(Message::HvscRsyncStart);
            }
            Message::WelcomeOpenLibrary => {
                self.show_welcome = false;
                self.config.has_seen_welcome = true;
                self.config.save();
                return iced::Task::done(Message::ToggleHvscBrowser);
            }
            Message::WelcomeDismiss => {
                self.show_welcome = false;
                self.config.has_seen_welcome = true;
                self.config.save();
            }

            // ── Sleep timer ──────────────────────────────────────────────
            Message::SetSleepTimer(mins) => match mins {
                Some(m) if m > 0 => {
                    self.sleep_deadline =
                        Some(Instant::now() + Duration::from_secs((m as u64) * 60));
                    self.sleep_selected_mins = Some(m);
                    eprintln!("[sleep] armed for {m} min");
                }
                _ => {
                    self.sleep_deadline = None;
                    self.sleep_selected_mins = None;
                    eprintln!("[sleep] disarmed");
                }
            },

            Message::ToggleFavoriteCurrent => {
                if let Some(cur_idx) = self.playlist.current {
                    let md5 = self
                        .playlist
                        .entries
                        .get(cur_idx)
                        .and_then(|e| e.md5.clone());
                    if let Some(ref md5) = md5 {
                        let is_fav = self.favorites.toggle(md5);
                        self.favorites.save();
                        eprintln!(
                            "[phosphor] {} current track",
                            if is_fav { "♥" } else { "♡" }
                        );
                    }
                }
            }

            Message::ToggleSidPanel => {
                self.show_sid_panel = !self.show_sid_panel;
                // Mutually exclusive with other panels
                if self.show_sid_panel {
                    self.show_settings = false;
                    self.show_recently_played = false;
                    self.show_hvsc_browser = false;
                }
                self.context_menu = None;
            }

            Message::ToggleHvscBrowser => {
                self.show_hvsc_browser = !self.show_hvsc_browser;
                if self.show_hvsc_browser {
                    self.show_settings = false;
                    self.show_recently_played = false;
                    self.show_sid_panel = false;
                    self.show_device_config = false;
                    // Re-sync root in case it changed since last open.
                    self.hvsc_browser.set_root(
                        self.config
                            .hvsc_root
                            .as_deref()
                            .map(std::path::PathBuf::from),
                    );
                    if let Err(e) = self.hvsc_browser.load_authors_if_needed() {
                        eprintln!("[hvsc-browser] {e}");
                    }
                }
                self.context_menu = None;
            }

            Message::HvscBrowserCategoryChanged(cat) => {
                self.hvsc_browser.set_category(cat);
                if let Err(e) = self.hvsc_browser.load_authors_if_needed() {
                    eprintln!("[hvsc-browser] {e}");
                }
            }

            Message::HvscBrowserSearchChanged(q) => {
                let was_empty = self.hvsc_browser.search().is_empty();
                self.hvsc_browser.set_search(q);
                // The first time the user types into the search box, lazily
                // build the flat-tune index for the current category so we
                // can match by song filename across all authors / sections.
                // Subsequent searches reuse the same index. ~50-200 ms for
                // a typical category on SSD.
                if was_empty && !self.hvsc_browser.search().is_empty() {
                    let _ = self.hvsc_browser.build_flat_index_if_needed();
                }
            }

            Message::HvscBrowserAuthorSelected(idx) => {
                self.hvsc_browser.select_author(
                    idx,
                    self.stil_db.as_ref(),
                    self.songlength_db.as_ref(),
                );
            }

            Message::HvscBrowserAddAllFromAuthor => {
                let entries: Vec<_> = self
                    .hvsc_browser
                    .tunes()
                    .iter()
                    .map(|t| t.entry.clone())
                    .collect();
                if !entries.is_empty() {
                    self.playlist.add_entries(entries);
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                }
            }

            Message::HvscBrowserAddTune(idx) => {
                if let Some(t) = self.hvsc_browser.tunes().get(idx) {
                    self.playlist.add_entries(vec![t.entry.clone()]);
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                }
            }

            Message::HvscBrowserPlayTune(idx) => {
                if let Some(t) = self.hvsc_browser.tunes().get(idx) {
                    let entry = t.entry.clone();
                    let path = entry.path.clone();
                    let song = entry.selected_song.max(1);
                    self.playlist.add_entries(vec![entry]);
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                    // Find the new entry in the (now-filtered) playlist + select it
                    // so the UI's "now playing" highlight follows. Then dispatch Play.
                    if let Some((vi, &abs_i)) =
                        self.filtered_indices.iter().enumerate().find(|(_, &i)| {
                            self.playlist.entries.get(i).map(|e| &e.path) == Some(&path)
                        })
                    {
                        let _ = vi;
                        self.selected = Some(abs_i);
                    }
                    let _ = self.cmd_tx.send(player::PlayerCmd::Play {
                        path,
                        song,
                        force_stereo: self.config.force_stereo_2sid
                            || std::env::args().any(|a| a == "--stereo"),
                        sid4_addr: parse_sid4_from_args(),
                        audio_port: if self.config.u64_audio_enabled {
                            Some(self.config.u64_audio_port)
                        } else {
                            None
                        },
                        restart_usb_on_load: self.config.restart_usb_on_load,
                    });
                    self.show_hvsc_browser = false;
                }
            }

            Message::HvscBrowserAddFlat(idx) => {
                if let Some(entry) = self
                    .hvsc_browser
                    .realise_flat(idx, self.songlength_db.as_ref())
                {
                    self.playlist.add_entries(vec![entry]);
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                }
            }

            Message::HvscBrowserPlayFlat(idx) => {
                if let Some(entry) = self
                    .hvsc_browser
                    .realise_flat(idx, self.songlength_db.as_ref())
                {
                    let path = entry.path.clone();
                    let song = entry.selected_song.max(1);
                    self.playlist.add_entries(vec![entry]);
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                    if let Some(abs_i) = self.playlist.entries.iter().position(|e| e.path == path) {
                        self.selected = Some(abs_i);
                    }
                    let _ = self.cmd_tx.send(player::PlayerCmd::Play {
                        path,
                        song,
                        force_stereo: self.config.force_stereo_2sid
                            || std::env::args().any(|a| a == "--stereo"),
                        sid4_addr: parse_sid4_from_args(),
                        audio_port: if self.config.u64_audio_enabled {
                            Some(self.config.u64_audio_port)
                        } else {
                            None
                        },
                        restart_usb_on_load: self.config.restart_usb_on_load,
                    });
                    self.show_hvsc_browser = false;
                }
            }

            // ── HVSC: 🎲 Surprise me ───────────────────────────────────────
            Message::HvscBrowserSurpriseMe => {
                // Ensure the flat index for the current category is loaded.
                let total = self.hvsc_browser.build_flat_index_if_needed();
                if total == 0 {
                    eprintln!("[surprise] No tunes in HVSC flat index — is the tree synced?");
                } else {
                    use rand::Rng;
                    let idx = rand::thread_rng().gen_range(0..total);
                    if let Some(entry) = self
                        .hvsc_browser
                        .realise_flat(idx, self.songlength_db.as_ref())
                    {
                        let path = entry.path.clone();
                        let song = entry.selected_song.max(1);
                        eprintln!(
                            "[surprise] picked {} of {}: {}",
                            idx + 1,
                            total,
                            path.display()
                        );
                        self.playlist.add_entries(vec![entry]);
                        if let Some(db) = self.songlength_db.as_ref() {
                            db.apply_to_playlist(
                                &mut self.playlist,
                                self.config.hvsc_root.as_deref().map(std::path::Path::new),
                            );
                        }
                        self.rebuild_filter();
                        if let Some(abs_i) =
                            self.playlist.entries.iter().position(|e| e.path == path)
                        {
                            self.selected = Some(abs_i);
                        }
                        let _ = self.cmd_tx.send(player::PlayerCmd::Play {
                            path,
                            song,
                            force_stereo: self.config.force_stereo_2sid
                                || std::env::args().any(|a| a == "--stereo"),
                            sid4_addr: parse_sid4_from_args(),
                            audio_port: if self.config.u64_audio_enabled {
                                Some(self.config.u64_audio_port)
                            } else {
                                None
                            },
                            restart_usb_on_load: self.config.restart_usb_on_load,
                        });
                        self.show_hvsc_browser = false;
                    }
                }
            }

            // ── Browse panel: source toggle ────────────────────────────────
            Message::BrowserSourceChanged(src) => {
                self.browser_source = src;
                self.config.browser_source = src.as_config_str().to_string();
                self.config.save();
            }

            // ── Assembly64 browser ─────────────────────────────────────────
            Message::Assembly64QueryChanged(q) => {
                self.assembly64_browser.set_query(q);
            }

            Message::Assembly64SearchSubmit => {
                let raw = self.assembly64_browser.query().trim().to_string();
                if raw.is_empty() {
                    return Task::none();
                }
                // Persist what the user typed (not the normalised form).
                self.config.assembly64_last_query = Some(raw.clone());
                self.config.save();
                let query = normalise_assembly64_query(&raw);
                // Stash the normalised form so "Load more" sends the same query.
                self.assembly64_browser.set_query(query.clone());
                self.assembly64_browser.begin_search();
                // Restore the user-facing text so the input doesn't suddenly
                // mutate under their cursor.
                self.assembly64_browser.set_query(raw.clone());
                let client = self.assembly64_client.clone();
                let page_size = self.assembly64_browser.page_size();
                return Task::perform(
                    async move {
                        client
                            .search(&query, 0, page_size)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::Assembly64SearchDone,
                );
            }

            Message::Assembly64SearchDone(result) => match result {
                Ok(page) => {
                    let prefetches = self.spawn_assembly64_prefetches(&page);
                    self.assembly64_browser.apply_results(page, true);
                    return Task::batch(prefetches);
                }
                Err(e) => self.assembly64_browser.set_search_error(e),
            },

            Message::Assembly64SearchMore => {
                let query = self.assembly64_browser.results_query().to_string();
                if query.is_empty() {
                    return Task::none();
                }
                let offset = self.assembly64_browser.offset();
                let page_size = self.assembly64_browser.page_size();
                self.assembly64_browser.begin_load_more();
                let client = self.assembly64_client.clone();
                return Task::perform(
                    async move {
                        client
                            .search(&query, offset, page_size)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::Assembly64SearchMoreDone,
                );
            }

            Message::Assembly64SearchMoreDone(result) => match result {
                Ok(page) => {
                    let prefetches = self.spawn_assembly64_prefetches(&page);
                    self.assembly64_browser.apply_results(page, false);
                    return Task::batch(prefetches);
                }
                Err(e) => self.assembly64_browser.set_search_error(e),
            },

            Message::Assembly64PrefetchDone(item_id, result) => match result {
                Ok(files) => self.assembly64_browser.record_prefetch(item_id, files),
                Err(_) => self.assembly64_browser.record_prefetch_failure(),
            },

            Message::Assembly64ToggleExpand(item_id, category_id) => {
                if self.assembly64_browser.expansion(&item_id).is_some() {
                    // Already expanded → collapse.
                    self.assembly64_browser.collapse(&item_id);
                    return Task::none();
                }
                // Cache hit from the post-search prefetch: skip the network round-trip.
                if let Some(cached) = self.assembly64_browser.prefetched_files(&item_id) {
                    let files = cached.to_vec();
                    self.assembly64_browser.set_expanded_loaded(item_id, files);
                    return Task::none();
                }
                self.assembly64_browser
                    .set_expanded_loading(item_id.clone());
                let client = self.assembly64_client.clone();
                let id_for_async = item_id.clone();
                return Task::perform(
                    async move {
                        let res = client
                            .list_files(&id_for_async, category_id)
                            .await
                            .map_err(|e| e.to_string());
                        (id_for_async, res)
                    },
                    |(id, res)| Message::Assembly64ExpandDone(id, res),
                );
            }

            Message::Assembly64ExpandDone(item_id, result) => match result {
                Ok(files) => self.assembly64_browser.set_expanded_loaded(item_id, files),
                Err(e) => self.assembly64_browser.set_expanded_failed(item_id, e),
            },

            Message::Assembly64PlayFile(item_id, category_id, file_id, file_path) => {
                return self.start_assembly64_download(
                    item_id,
                    category_id,
                    file_id,
                    file_path,
                    true,
                );
            }

            Message::Assembly64AddFile(item_id, category_id, file_id, file_path) => {
                return self.start_assembly64_download(
                    item_id,
                    category_id,
                    file_id,
                    file_path,
                    false,
                );
            }

            Message::Assembly64DownloadDone(result, play, song) => match result {
                Ok(cached_path) => match playlist::PlaylistEntry::from_path(&cached_path) {
                    Ok(entry) => {
                        let path = entry.path.clone();
                        let resolved_song = entry.selected_song.max(song).max(1);
                        self.playlist.add_entries(vec![entry]);
                        if let Some(db) = self.songlength_db.as_ref() {
                            db.apply_to_playlist(
                                &mut self.playlist,
                                self.config.hvsc_root.as_deref().map(std::path::Path::new),
                            );
                        }
                        self.rebuild_filter();
                        if play {
                            if let Some(abs_i) =
                                self.playlist.entries.iter().position(|e| e.path == path)
                            {
                                self.selected = Some(abs_i);
                            }
                            let _ = self.cmd_tx.send(player::PlayerCmd::Play {
                                path,
                                song: resolved_song,
                                force_stereo: self.config.force_stereo_2sid
                                    || std::env::args().any(|a| a == "--stereo"),
                                sid4_addr: parse_sid4_from_args(),
                                audio_port: if self.config.u64_audio_enabled {
                                    Some(self.config.u64_audio_port)
                                } else {
                                    None
                                },
                                restart_usb_on_load: self.config.restart_usb_on_load,
                            });
                            self.show_hvsc_browser = false;
                        }
                    }
                    Err(e) => {
                        self.assembly64_browser
                            .set_search_error(format!("Cannot parse SID: {e}"));
                    }
                },
                Err(e) => {
                    self.assembly64_browser
                        .set_search_error(format!("Download failed: {e}"));
                }
            },

            // ── Published playlists ───────────────────────────────────────
            Message::PublishedPlaylistsSyncStart => {
                if self.published_playlists_browser.sync_in_flight() {
                    return Task::none();
                }
                self.published_playlists_browser.begin_sync();
                let client = self.published_playlists_client.clone();
                return Task::perform(
                    async move { client.fetch_index().await },
                    Message::PublishedPlaylistsManifestDone,
                );
            }

            Message::PublishedPlaylistsManifestDone(result) => match result {
                Ok(manifest) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);

                    let cache_dir = published_playlists_cache_dir();
                    // Compare against the previously-cached index for delta-sync.
                    let prev_index_path = cache_dir.join("index.json");
                    let prev_shas: std::collections::HashMap<String, String> =
                        std::fs::read_to_string(&prev_index_path)
                            .ok()
                            .and_then(|s| {
                                serde_json::from_str::<published_playlists::Manifest>(&s).ok()
                            })
                            .map(|m| {
                                m.playlists
                                    .into_iter()
                                    .map(|p| (p.file, p.sha256))
                                    .collect()
                            })
                            .unwrap_or_default();

                    // Persist new manifest to disk for next launch's diff.
                    let _ = std::fs::create_dir_all(&cache_dir);
                    if let Ok(json) = serde_json::to_string_pretty(&serde_json::json!({
                        "version": manifest.version,
                        "playlists": manifest.playlists.iter().map(|p| serde_json::json!({
                            "file": p.file,
                            "name": p.name,
                            "description": p.description,
                            "tracks": p.tracks,
                            "sha256": p.sha256,
                        })).collect::<Vec<_>>(),
                    })) {
                        let _ = std::fs::write(&prev_index_path, json);
                    }

                    // Decide which files need downloading.
                    let mut downloads: Vec<Task<Message>> = Vec::new();
                    for entry in &manifest.playlists {
                        let cached_path = cache_dir.join(&entry.file);
                        let needs_download = !cached_path.exists()
                            || prev_shas
                                .get(&entry.file)
                                .map(|s| s != &entry.sha256)
                                .unwrap_or(true);
                        if needs_download {
                            let client = self.published_playlists_client.clone();
                            let file = entry.file.clone();
                            let dir = cache_dir.clone();
                            let file_for_done = file.clone();
                            downloads.push(Task::perform(
                                async move {
                                    let res = client.download_playlist(&file, dir).await;
                                    (file_for_done, res)
                                },
                                |(f, r)| Message::PublishedPlaylistsFileDone(f, r),
                            ));
                        }
                    }

                    self.published_playlists_browser
                        .note_download_started(downloads.len() as u32);
                    self.published_playlists_browser
                        .apply_manifest(manifest, now);
                    self.config.published_playlists_last_synced = Some(now);
                    self.config.save();

                    if !downloads.is_empty() {
                        return Task::batch(downloads);
                    }
                }
                Err(e) => self.published_playlists_browser.set_error(e),
            },

            Message::PublishedPlaylistsFileDone(file, result) => {
                self.published_playlists_browser.note_download_finished();
                if let Err(e) = &result {
                    eprintln!("[phosphor] Published playlist download failed for {file}: {e}");
                }
                // If the user has this row expanded and the preview was waiting,
                // kick off the parse now that the file is on disk.
                if matches!(
                    self.published_playlists_browser.preview(&file),
                    Some(crate::published_playlists_browser::PreviewState::Loading)
                ) && result.is_ok()
                {
                    return self.spawn_published_preview_parse(file);
                }
            }

            Message::PublishedPlaylistsToggleExpand(file) => {
                if self.published_playlists_browser.is_expanded(&file) {
                    self.published_playlists_browser.collapse(&file);
                    return Task::none();
                }
                self.published_playlists_browser
                    .set_preview_loading(file.clone());
                let cache_dir = published_playlists_cache_dir();
                let cached_path = cache_dir.join(&file);
                if cached_path.exists() {
                    return self.spawn_published_preview_parse(file);
                }
                // File not yet downloaded — PublishedPlaylistsFileDone will
                // re-trigger this parse when the download completes.
            }

            Message::PublishedPlaylistsPreviewDone(file, result) => match result {
                Ok(tracks) => self
                    .published_playlists_browser
                    .set_preview_ready(file, tracks),
                Err(e) => self.published_playlists_browser.set_preview_failed(file, e),
            },

            Message::PublishedPlaylistsLoad(file) => {
                let Some(hvsc_root) = self.config.hvsc_root.as_ref().map(PathBuf::from) else {
                    self.published_playlists_browser
                        .set_error("Configure HVSC root in Settings first.".into());
                    return Task::none();
                };
                let cache_dir = published_playlists_cache_dir();
                let cached_path = cache_dir.join(&file);
                let pg = self.loading_progress.clone();
                let file_for_async = file.clone();
                let file_for_load = file.clone();

                // Skeleton loader: no SID reads at parse time, so the
                // playlist swap is instant. Background `EnrichDone` task
                // fills in real titles + durations a moment later.
                if cached_path.exists() {
                    return Task::perform(
                        async move {
                            playlist::parse_playlist_skeleton_with_base(cached_path, hvsc_root, pg)
                                .map(|entries| (file_for_async, entries))
                        },
                        Message::PublishedPlaylistsLoadDone,
                    );
                }

                // Not cached yet — download then skeleton-parse, chained.
                let client = self.published_playlists_client.clone();
                let cache_dir_clone = cache_dir.clone();
                return Task::perform(
                    async move {
                        let dl_path = client
                            .download_playlist(&file_for_async, cache_dir_clone)
                            .await?;
                        playlist::parse_playlist_skeleton_with_base(dl_path, hvsc_root, pg)
                            .map(|entries| (file_for_load, entries))
                    },
                    Message::PublishedPlaylistsLoadDone,
                );
            }

            Message::PublishedPlaylistsLoadDone(result) => match result {
                Ok((file, skeletons)) => {
                    let n = skeletons.len();
                    eprintln!(
                        "[phosphor] Loaded published playlist '{file}' ({n} skeletons) — enriching in background"
                    );
                    self.playlist.entries.clear();
                    self.playlist.add_entries(skeletons.clone());
                    if let Some(db) = self.songlength_db.as_ref() {
                        db.apply_to_playlist(
                            &mut self.playlist,
                            self.config.hvsc_root.as_deref().map(std::path::Path::new),
                        );
                    }
                    self.rebuild_filter();
                    self.session_mode = SessionMode::PublishedReadOnly { file: file.clone() };
                    self.published_playlists_browser.set_active(file.clone());
                    self.selected = None;
                    self.show_hvsc_browser = false;

                    // Background enrichment — runs on a blocking task so
                    // the ~100 disk reads don't block the async runtime.
                    let file_for_enrich = file.clone();
                    return Task::perform(
                        async move {
                            let enriched = tokio::task::spawn_blocking(move || {
                                playlist::enrich_skeleton_entries(skeletons)
                            })
                            .await
                            .unwrap_or_default();
                            (file_for_enrich, enriched)
                        },
                        |(f, e)| Message::PublishedPlaylistsEnrichDone(f, e),
                    );
                }
                Err(e) => {
                    self.published_playlists_browser
                        .set_error(format!("Load failed: {e}"));
                }
            },

            Message::PublishedPlaylistsEnrichDone(source_file, enriched) => {
                // Drop the result if the user has since switched playlists or
                // restored their default — applying it would clobber state.
                let still_active = matches!(
                    &self.session_mode,
                    SessionMode::PublishedReadOnly { file } if file == &source_file
                );
                if !still_active {
                    eprintln!(
                        "[phosphor] Enrichment for '{source_file}' discarded — session changed"
                    );
                    return Task::none();
                }
                eprintln!(
                    "[phosphor] Enriched published playlist '{source_file}' ({} entries)",
                    enriched.len()
                );
                self.playlist.entries.clear();
                self.playlist.add_entries(enriched);
                if let Some(db) = self.songlength_db.as_ref() {
                    db.apply_to_playlist(
                        &mut self.playlist,
                        self.config.hvsc_root.as_deref().map(std::path::Path::new),
                    );
                }
                self.rebuild_filter();
            }

            Message::PublishedPlaylistsRestoreDefault => {
                let pg = self.loading_progress.clone();
                return Task::perform(
                    async move { playlist::parse_startup(Vec::new(), pg) },
                    Message::PublishedPlaylistsRestoreDone,
                );
            }

            Message::PublishedPlaylistsRestoreDone(entries) => {
                let n = entries.len();
                eprintln!("[phosphor] Restored default playlist ({n} entries)");
                self.playlist.entries.clear();
                self.playlist.add_entries(entries);
                if let Some(db) = self.songlength_db.as_ref() {
                    db.apply_to_playlist(
                        &mut self.playlist,
                        self.config.hvsc_root.as_deref().map(std::path::Path::new),
                    );
                }
                self.rebuild_filter();
                self.session_mode = SessionMode::Default;
                self.published_playlists_browser.clear_active();
                self.selected = None;
            }

            // ── Virtual scroll ────────────────────────────────────────────
            Message::PlaylistScrolled(viewport) => {
                // Store absolute Y offset and viewport height so playlist_view()
                // can compute the visible window on the next view() call.
                self.playlist_scroll_offset_y = viewport.absolute_offset().y;
                self.playlist_viewport_height = viewport.bounds().height;
                self.playlist_viewport_y = viewport.bounds().y;
            }

            // ── STIL overlay ──────────────────────────────────────────────
            Message::ShowStilOverlay => {
                self.show_stil_overlay = true;
                self.context_menu = None;
            }

            Message::DismissStilOverlay => {
                self.show_stil_overlay = false;
            }

            Message::HvscRootChanged(root) => {
                self.config.hvsc_root = if root.trim().is_empty() {
                    None
                } else {
                    Some(root)
                };
            }

            Message::SetHvscRoot(root) => {
                self.config.hvsc_root = if root.trim().is_empty() {
                    None
                } else {
                    Some(root.trim().to_string())
                };
                self.config.save();
                self.refresh_stil_entry();
            }

            Message::HvscRsyncUrlChanged(url) => {
                // Trim — a trailing space (easy to introduce by copy-paste)
                // breaks every URL we derive from this base. Strip here so
                // the saved config stays clean.
                self.config.hvsc_rsync_url = url.trim().to_string();
                self.config.save();
            }

            Message::HvscRsyncStart => {
                if self.hvsc_sync.is_some() {
                    // Already running — ignore.
                } else {
                    // Pick destination: hvsc_root if set, else platform default.
                    let dest = match self
                        .config
                        .hvsc_root
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(PathBuf::from)
                        .or_else(hvsc_sync::default_hvsc_root)
                    {
                        Some(p) => p,
                        None => {
                            self.hvsc_sync_status =
                                "Cannot determine destination — set HVSC root manually."
                                    .to_string();
                            return Task::none();
                        }
                    };
                    let url = self.config.hvsc_rsync_url.clone();
                    match hvsc_sync::HvscSyncHandle::start(&url, &dest) {
                        Ok(handle) => {
                            // Persist the destination so it survives restarts.
                            self.config.hvsc_root = Some(dest.to_string_lossy().into_owned());
                            self.config.save();
                            self.hvsc_sync = Some(handle);
                            self.hvsc_sync_status = "Connecting…".to_string();
                            self.hvsc_sync_progress = None;
                        }
                        Err(e) => {
                            self.hvsc_sync_status = format!("Error: {e}");
                        }
                    }
                }
            }

            Message::HvscRsyncCancel => {
                if let Some(h) = self.hvsc_sync.as_ref() {
                    h.cancel();
                    self.hvsc_sync_status = "Cancelling…".to_string();
                }
            }

            Message::HvscRsyncPoll => {
                // No-op — actual drain happens in poll_status() each Tick.
            }

            Message::DownloadStil => {
                self.stil_status = "Downloading…".to_string();
                let url = self.config.hvsc_rsync_url.clone();
                return Task::perform(stil::download_stil(url), Message::StilDownloaded);
            }

            Message::StilDownloaded(result) => match result {
                Ok(path) => {
                    self.config.remember_stil_path(&path);
                    match stil::StilDb::load(&path) {
                        Ok(db) => {
                            let count = db.count;
                            self.stil_status = format!("Loaded {} entries", count);
                            self.stil_db = Some(db);
                            self.update_auto_download_status();
                            self.refresh_stil_entry();
                            self.refresh_hvsc_status();
                        }
                        Err(e) => self.stil_status = format!("Error loading STIL: {e}"),
                    }
                }
                Err(e) => {
                    self.stil_status = format!("Download failed: {e}");
                    self.update_auto_download_status();
                    eprintln!("[phosphor] STIL download failed: {e}");
                }
            },

            Message::LoadStil => {
                return Task::perform(pick_stil_file(), Message::StilFileChosen);
            }

            Message::StilFileChosen(opt) => {
                if let Some(path) = opt {
                    self.config.remember_stil_path(&path);
                    match stil::StilDb::load(&path) {
                        Ok(db) => {
                            let count = db.count;
                            self.stil_status =
                                format!("Loaded {} entries from {}", count, path.display());
                            self.stil_db = Some(db);
                            self.refresh_stil_entry();
                            self.refresh_hvsc_status();
                        }
                        Err(e) => self.stil_status = format!("Error: {e}"),
                    }
                }
            }

            // ── U64 audio streaming ───────────────────────────────────────
            Message::ToggleU64Audio => {
                self.config.u64_audio_enabled = !self.config.u64_audio_enabled;
                self.config.save();
            }

            Message::U64AudioPortChanged(val) => {
                if let Ok(port) = val.trim().parse::<u16>() {
                    if port >= 1024 {
                        self.config.u64_audio_port = port;
                        self.config.save();
                    }
                }
            }

            Message::ToggleHttpRemote => {
                if self.http_remote_running {
                    // Can't stop tiny_http gracefully, but we can flag it.
                    // The server thread will keep running but commands will
                    // be ignored because we won't poll them.
                    self.http_remote_running = false;
                    self.config.http_remote_enabled = false;
                    self.config.save();
                    eprintln!("[phosphor] Remote control disabled (restart app to free the port)");
                } else {
                    remote::start_server(
                        self.config.http_remote_port,
                        Arc::clone(&self.remote_state),
                        self.remote_cmd_tx.clone(),
                    );
                    self.http_remote_running = true;
                    self.config.http_remote_enabled = true;
                    self.config.save();
                }
            }

            Message::HttpRemotePortChanged(val) => {
                self.http_port_text = val.clone();
                if let Ok(port) = val.trim().parse::<u16>() {
                    if port > 0 {
                        self.config.http_remote_port = port;
                        self.config.save();
                        // Restart server on the new port if running.
                        if self.http_remote_running {
                            remote::start_server(
                                port,
                                Arc::clone(&self.remote_state),
                                self.remote_cmd_tx.clone(),
                            );
                        }
                    }
                }
            }

            Message::None => {}
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let _perf = ProfilerGuard::view();
        // ── Mini player mode ─────────────────────────────────────────────────
        if self.mini_mode {
            // Prefer the live TrackInfo (fresh from the player thread) as
            // the authoritative source of md5 / current_song. Play paths
            // like Surprise Me / Library-direct-play skip `play_track()`
            // and therefore never update `playlist.current`, so falling
            // back to `playlist.current_entry()` would show the *previous*
            // track's duration. TrackInfo is always in sync with what's
            // actually playing.
            let live_md5 = self.status.track_info.as_ref().map(|i| i.md5.as_str());
            let live_song = self
                .status
                .track_info
                .as_ref()
                .map(|i| i.current_song)
                .unwrap_or(1);
            let current_duration = live_md5
                .and_then(|m| {
                    self.songlength_db
                        .as_ref()
                        .and_then(|db| db.lookup(m, live_song.saturating_sub(1) as usize))
                })
                // Fallbacks: playlist entry cache, then the U64 on-screen total.
                .or_else(|| self.playlist.current_entry().and_then(|e| e.duration_secs))
                .or_else(|| self.status.u64_screen_total_secs.map(|s| s as u32));
            let is_fav = live_md5
                .map(|m| self.favorites.is_favorite(m))
                .unwrap_or(false);
            let is_heard = live_md5.map(|m| self.heard_db.contains(m)).unwrap_or(false);
            // 1-based position in the (unfiltered) playlist for the "01" badge.
            let track_position = self.playlist.current.map(|i| i + 1);
            return ui::mini_player_view(
                &self.status,
                current_duration,
                is_fav,
                is_heard,
                track_position,
                self.tick,
            );
        }

        let is_now_playing_fav = self
            .playlist
            .current_entry()
            .and_then(|e| e.md5.as_ref())
            .map(|m| self.favorites.is_favorite(m))
            .unwrap_or(false);

        let num_sids_for_tracker = self
            .status
            .track_info
            .as_ref()
            .map(|i| i.num_sids)
            .unwrap_or(1);
        let tracker_ref = if self.visualizer.mode == ui::visualizer::VisMode::Tracker {
            Some(TrackerRef {
                history: &self.tracker_history,
                num_sids: num_sids_for_tracker,
            })
        } else {
            None
        };
        // Engine label suffix: when we're on USB and the device has been
        // probed at least once, show what chips are actually on the board
        // (e.g. "2× MOS8580" or "MOS6581 + MOS8580"). Other engines: none.
        let engine_suffix = if self.config.output_engine == "usb" {
            self.device_cfg.as_ref().map(format_usb_chip_summary)
        } else {
            None
        };
        let info_bar = ui::track_info_bar(
            &self.status,
            &self.visualizer,
            tracker_ref,
            is_now_playing_fav,
            self.status.track_info.is_some(),
            self.stil_entry.is_some() || !self.stil_display_text.is_empty(),
            self.window_width,
            &self.config.output_engine,
            self.config.master_volume,
            engine_suffix.as_deref(),
        );
        // Alarm the Library button (rotating red ring) when HVSC is unconfigured
        // or its root path no longer exists on disk. Silenced while a sync is
        // in flight — the panel itself shows progress in that case — and
        // silenced permanently once the user has dismissed the first-run
        // welcome card (the intro told them where the Library lives, so
        // continued pulsing is just noise).
        let hvsc_needs_attention = self
            .config
            .hvsc_root
            .as_ref()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(true)
            && self.hvsc_sync.is_none()
            && !self.config.has_seen_welcome;
        let controls = ui::controls_bar(
            &self.status,
            &self.playlist,
            self.new_version.as_ref(),
            self.window_width,
            self.show_recently_played,
            self.show_sid_panel,
            self.tick,
            hvsc_needs_attention,
        );
        let current_duration = self.playlist.current_entry().and_then(|e| e.duration_secs);
        let progress = ui::progress_bar(&self.status, current_duration);

        // Build the main content area
        let main_content: Element<'_, Message> = if self.show_device_config {
            let panel =
                ui::device_panel::device_panel(self.device_cfg.as_ref(), &self.device_cfg_status);
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                panel
            ]
            .into()
        } else if self.show_settings {
            let settings = ui::settings_panel(
                &self.config,
                &self.default_length_text,
                &self.download_status,
                &self.stil_status,
                self.http_remote_running,
                &self.http_port_text,
                &self.base_font_size_text,
                &self.proxy_url_text,
                self.hvsc_sync.is_some(),
                &self.hvsc_sync_status,
                self.hvsc_sync_progress,
                self.sleep_selected_mins,
            );
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                settings
            ]
            .into()
        } else if self.show_recently_played {
            let current_md5 = self.playlist.current_entry().and_then(|e| e.md5.as_deref());
            let recent_panel = ui::recently_played_view(&self.recently_played, current_md5);
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                recent_panel
            ]
            .into()
        } else if self.show_sid_panel {
            let num_sids = self
                .status
                .track_info
                .as_ref()
                .map(|i| i.num_sids)
                .unwrap_or(1);
            let is_pal = self
                .status
                .track_info
                .as_ref()
                .map(|i| i.is_pal)
                .unwrap_or(true);
            let tracker_height = (self.window_height * 0.45).clamp(200.0, 400.0);
            let sid_view = ui::sid_panel::sid_panel(
                &self.tracker_view,
                &self.tracker_history,
                &self.status.sid_regs,
                num_sids,
                is_pal,
                tracker_height,
            );
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                sid_view
            ]
            .into()
        } else if self.show_hvsc_browser {
            let browser = ui::browser_view(
                self.browser_source,
                &self.hvsc_browser,
                &self.assembly64_browser,
                &self.published_playlists_browser,
                self.config.hvsc_root.is_some(),
                self.hvsc_update_available,
                self.hvsc_sync.is_some(),
                &self.hvsc_sync_status,
                &self.session_mode,
            );
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                browser
            ]
            .into()
        } else {
            let loading_status = {
                let pg = self
                    .loading_progress
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                if !pg.is_empty() {
                    pg
                } else if !self.auto_download_status.is_empty() {
                    let track_count = self.filtered_indices.len();
                    let total = self.playlist.len();
                    let count_part = if track_count == total {
                        format!("{total} tracks")
                    } else {
                        format!("{track_count} / {total} tracks")
                    };
                    format!("{count_part}  {}", self.auto_download_status)
                } else {
                    String::new()
                }
            };
            let search = ui::search_bar(
                &self.search_text,
                self.filtered_indices.len(),
                self.playlist.len(),
                self.favorites_only,
                self.favorites.count(),
                &loading_status,
            );
            let playlist_widget = ui::playlist_view(
                &self.playlist,
                self.selected,
                &self.filtered_indices,
                &self.favorites,
                self.sort_column,
                self.sort_direction,
                self.playlist_scroll_offset_y,
                self.playlist_viewport_height,
                &loading_status,
                self.tick,
            );
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                search,
                rule::horizontal(1),
                playlist_widget,
                rule::horizontal(1),
                ui::status_bar(
                    &self.heard_text,
                    &self.hvsc_status_text,
                    self.hvsc_update_available,
                ),
            ]
            .into()
        };

        let base = container(main_content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
                ..Default::default()
            });

        // Always use a stack so the widget tree structure stays identical
        // regardless of which overlay is active.  Switching between stack and
        // non-stack causes iced to discard all internal widget state — including
        // the scrollable's position — which makes the playlist jump to the top
        // every time the context menu opens or closes.
        let overlay: Element<'_, Message> = if let Some(ref cm) = self.context_menu {
            ui::context_menu_overlay(
                cm.x,
                cm.y,
                cm.track_idx,
                &self.playlist,
                &self.favorites,
                self.window_width,
                self.window_height,
            )
        } else if self.vis_expanded {
            // Wrap in mouse_area to capture all clicks — prevents click-through
            // to the playlist underneath when the overlay is active.
            mouse_area(
                container(self.visualizer.view_expanded(
                    self.vis_expanded_info.as_ref(),
                    if self.visualizer.mode == ui::visualizer::VisMode::Tracker {
                        Some(TrackerRef {
                            history: &self.tracker_history,
                            num_sids: self
                                .status
                                .track_info
                                .as_ref()
                                .map(|i| i.num_sids)
                                .unwrap_or(1),
                        })
                    } else {
                        None
                    },
                ))
                .width(Length::Fill)
                .height(Length::Fill),
            )
            .on_press(Message::Noop)
            .on_right_press(Message::Noop)
            .into()
        } else if self.show_welcome {
            ui::welcome_overlay(self.config.hvsc_root.is_some())
        } else if self.show_help {
            ui::help_overlay()
        } else if self.show_stil_overlay
            && (self.stil_entry.is_some() || !self.stil_display_text.is_empty())
        {
            let current_song = self
                .status
                .track_info
                .as_ref()
                .map(|i| i.current_song)
                .unwrap_or(1);
            ui::stil_overlay(&self.stil_display_text, current_song)
        } else {
            // No overlay — empty zero-size space so the stack structure is stable.
            Space::new()
                .width(Length::Shrink)
                .height(Length::Shrink)
                .into()
        };

        iced::widget::stack![base, overlay]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = time::every(Duration::from_millis(33)).map(|_| Message::Tick);

        let window_events = event::listen_with(|event, status, id| match event {
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(id, size.width, size.height))
            }
            iced::Event::Window(iced::window::Event::Moved(point)) => {
                // Piggyback window ID capture on the Moved event —
                // this fires at startup when the saved position is restored.
                // We'll handle both WindowMoved and store the ID.
                Some(Message::WindowMoved(point.x as i32, point.y as i32))
            }
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                use iced::event::Status;
                use iced::keyboard::key::Named;
                use iced::keyboard::Key;
                match key {
                    // Escape — context-sensitive, resolved in update()
                    Key::Named(Named::Escape) => Some(Message::KeyEscape),
                    // Space — play/pause (not when typing)
                    Key::Named(Named::Space) if status != Status::Captured => {
                        Some(Message::PlayPause)
                    }
                    // V — cycle visualiser mode
                    Key::Character(ref c) if c.as_str() == "v" && status != Status::Captured => {
                        Some(Message::ToggleVisMode)
                    }
                    // F — toggle full-screen visualiser
                    Key::Character(ref c)
                        if c.as_str() == "f"
                            && !modifiers.control()
                            && status != Status::Captured =>
                    {
                        Some(Message::ToggleVisFull)
                    }
                    // H — toggle favourite for currently playing track
                    Key::Character(ref c) if c.as_str() == "h" && status != Status::Captured => {
                        Some(Message::ToggleFavoriteCurrent)
                    }
                    // K — toggle karaoke mode (MUS files with WDS lyrics)
                    Key::Character(ref c) if c.as_str() == "k" && status != Status::Captured => {
                        Some(Message::ToggleKaraoke)
                    }
                    // M — toggle mini player
                    Key::Character(ref c) if c.as_str() == "m" && status != Status::Captured => {
                        Some(Message::ToggleMiniPlayer)
                    }
                    // L — toggle 📚 Library panel
                    Key::Character(ref c) if c.as_str() == "l" && status != Status::Captured => {
                        Some(Message::ToggleHvscBrowser)
                    }
                    // ? — show/hide help overlay
                    Key::Character(ref c) if c.as_str() == "?" => Some(Message::ShowHelp),
                    // Arrow keys — context-sensitive, resolved in update()
                    Key::Named(Named::ArrowLeft) => Some(Message::KeyArrowLeft),
                    Key::Named(Named::ArrowRight) => Some(Message::KeyArrowRight),
                    Key::Named(Named::ArrowUp) => Some(Message::SelectPrev),
                    Key::Named(Named::ArrowDown) => Some(Message::SelectNext),
                    Key::Named(Named::Delete) => Some(Message::RemoveSelected),
                    // Ctrl+F — focus search
                    Key::Character(ref c) if c.as_str() == "f" && modifiers.control() => {
                        Some(Message::FocusSearch)
                    }
                    _ => None,
                }
            }
            _ => None,
        });

        Subscription::batch([tick, window_events])
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    /// Reset all timing-related status fields the auto-advance check reads.
    ///
    /// Must be called whenever we queue a `Play` or `SetSubtune` command —
    /// without this, `Tick` (every 33 ms) keeps reading the previous song's
    /// elapsed time while the player thread is busy processing the queued
    /// command (notably U64's `sid_play` REST call, ~300 ms). Each Tick
    /// re-fires SetSubtune on the same stale data, queueing 5-15 extra
    /// transitions before the player can respond — visibly skipping
    /// subtunes at ~3 per second.
    fn clear_advance_status(&mut self) {
        self.status.elapsed = Duration::ZERO;
        // Zero writes_per_frame too — without this, while the player thread
        // is blocked in a slow handler (notably U64 sid_play), every main.rs
        // Tick increments silence_frames against a stale 0-writes status.
        // It can't fire silence-advance because of the 5 s elapsed gate, but
        // zeroing here means one fewer counter that can drift.
        self.status.writes_per_frame = 0;
        self.status.u64_screen_elapsed_secs = None;
        self.status.u64_screen_read_at = None;
        self.status.u64_screen_total_secs = None;
        self.silence_frames = 0;
    }

    fn play_track(&mut self, idx: usize) {
        if let Some(entry) = self.playlist.entries.get(idx) {
            if self.config.skip_rsid && entry.is_rsid {
                eprintln!("[phosphor] Skipping RSID tune: \"{}\"", entry.title);
                self.playlist.current = Some(idx);
                if let Some(next_idx) = self.playlist.next() {
                    if next_idx != idx {
                        self.play_track(next_idx);
                    } else {
                        let _ = self.cmd_tx.send(PlayerCmd::Stop);
                    }
                } else {
                    let _ = self.cmd_tx.send(PlayerCmd::Stop);
                }
                return;
            }

            if let Some(ref md5) = entry.md5 {
                self.recently_played.record(
                    md5,
                    &entry.title,
                    &entry.author,
                    &entry.released,
                    &entry.path,
                );
                self.recently_played.save();
                // Record in the heard-set for HVSC completion tracking.
                // save() is deferred — only writes when dirty.
                self.heard_db.record(md5);
                self.heard_db.save();
            }

            self.silence_frames = 0;
            self.karaoke_groups.clear();
            self.karaoke_line = 0;
            self.last_flag_count = 0;
            self.playlist.current = Some(idx);
            self.selected = Some(idx);
            self.scroll_to_current = true;

            let force_stereo =
                self.config.force_stereo_2sid || std::env::args().any(|a| a == "--stereo");
            let sid4_addr = parse_sid4_from_args();
            let play_path = entry.path.clone();
            let play_song = entry.selected_song;

            self.show_stil_overlay = false;
            self.tracker_history.reset();
            self.tracker_view.reset();
            let audio_port = if self.config.output_engine == "u64" && self.config.u64_audio_enabled
            {
                Some(self.config.u64_audio_port)
            } else {
                None
            };
            let _ = self.cmd_tx.send(PlayerCmd::Play {
                path: play_path,
                song: play_song,
                force_stereo,
                sid4_addr,
                audio_port,
                restart_usb_on_load: self.config.restart_usb_on_load,
            });
            self.clear_advance_status();
            // Fresh track — drop the debounce so the auto-advance for THIS
            // track's first subtune isn't gated by the previous track's fire.
            self.last_advance_at = None;
            // First USB playback this session → ask the bridge for the
            // device's actual SID chip layout once. The result goes into
            // self.device_cfg via DeviceConfigResult, and from there into
            // the engine label suffix in track_info_bar.
            if self.config.output_engine == "usb" && !self.usb_info_fetched {
                self.usb_info_fetched = true;
                let _ = self.cmd_tx.send(player::PlayerCmd::DeviceConfig(
                    player::DeviceConfigCmd::Refresh,
                ));
            }
            // entry borrow ends here; now safe to call &mut self method.
            self.refresh_stil_entry();
        }
    }

    /// Recompute the auto-download status line shown in the search bar.
    /// Call after each auto-download completes.  Clears the line once both
    /// databases are loaded so it doesn't clutter the UI permanently.
    /// Called when an auto-download task completes (success or failure).
    /// Decrements the in-flight counter and clears the status banner when done.
    fn update_auto_download_status(&mut self) {
        if self.pending_auto_downloads > 0 {
            self.pending_auto_downloads -= 1;
        }
        if self.pending_auto_downloads == 0 {
            self.auto_download_status.clear();
            // Both databases are now current — record the confirmed HVSC version
            // so we don't re-download on the next launch.
            if let Some(ver) = self.stil_db.as_ref().and_then(|db| db.hvsc_version) {
                self.config.hvsc_known_version = Some(format!("v{ver}"));
                self.config.save();
                eprintln!("[phosphor] HVSC databases updated to v{ver}");
            }
        }
    }

    /// After a successful HVSC rsync, re-point the Songlength and STIL
    /// databases at the freshly synced copies under `<hvsc_root>/DOCUMENTS/`
    /// when (a) they're currently unset, or (b) they point outside the new
    /// HVSC root. Then reload both DBs and refresh the now-playing entry.
    /// Recompute the HVSC version indicator shown in the status bar.
    /// Call whenever the local STIL DB or the cached remote version changes.
    fn refresh_hvsc_status(&mut self) {
        let local_v = self.stil_db.as_ref().and_then(|db| db.hvsc_version);
        let (text, available) = match (local_v, self.hvsc_remote_version) {
            (Some(l), Some(r)) if r > l => (format!("HVSC v{l} → v{r} ⚠"), true),
            (Some(l), Some(r)) if l == r => (format!("HVSC v{l} ✓"), false),
            (Some(l), _) => (format!("HVSC v{l}"), false),
            (None, Some(r)) => (format!("HVSC v{r} available"), false),
            (None, None) => (String::new(), false),
        };
        self.hvsc_status_text = text;
        self.hvsc_update_available = available;
    }

    fn apply_post_hvsc_sync(&mut self) {
        let Some(root) = self.config.hvsc_root.clone() else {
            return;
        };
        let root = PathBuf::from(root);

        let path_inside = |p: &Option<String>| -> bool {
            p.as_deref()
                .map(|s| PathBuf::from(s).starts_with(&root))
                .unwrap_or(false)
        };

        // Songlengths.md5
        let sl_candidate = root.join("DOCUMENTS").join("Songlengths.md5");
        if sl_candidate.is_file() && !path_inside(&self.config.last_songlength_file) {
            self.config.remember_songlength_path(&sl_candidate);
            if let Ok(db) = SonglengthDb::load(&sl_candidate) {
                let count = db.entries.len();
                db.apply_to_playlist(
                    &mut self.playlist,
                    self.config.hvsc_root.as_deref().map(std::path::Path::new),
                );
                self.songlength_db = Some(db);
                self.download_status =
                    format!("Loaded {} entries from {}", count, sl_candidate.display());
            }
        }

        // STIL.txt
        let stil_candidate = root.join("DOCUMENTS").join("STIL.txt");
        if stil_candidate.is_file() && !path_inside(&self.config.last_stil_file) {
            self.config.remember_stil_path(&stil_candidate);
            if let Ok(db) = stil::StilDb::load(&stil_candidate) {
                self.stil_status = format!("Loaded {} entries", db.count);
                self.stil_db = Some(db);
                self.refresh_stil_entry();
                self.refresh_hvsc_status();
            }
        }
    }

    /// Re-resolve STIL info for the currently playing track.
    /// Call whenever the track changes or the STIL db / hvsc_root is updated.
    fn refresh_stil_entry(&mut self) {
        let db = match self.stil_db.as_ref() {
            Some(db) => db,
            None => {
                self.stil_entry = None;
                self.stil_display_text.clear();
                return;
            }
        };
        let path = match self.playlist.current_entry() {
            Some(e) => e.path.clone(),
            None => {
                self.stil_entry = None;
                self.stil_display_text.clear();
                return;
            }
        };
        let hvsc_root = self.config.hvsc_root.as_deref().map(std::path::Path::new);
        self.stil_entry = db.lookup(&path, hvsc_root).cloned();
        self.rebuild_stil_display();
        if let Some(ref entry) = self.stil_entry {
            eprintln!("[phosphor] STIL: found entry for {}", entry.hvsc_path);
        }
    }

    /// Rebuild `stil_display_text` from the current `stil_entry` and active subtune.
    /// For MUS files without STIL info, loads companion .wds lyrics if available.
    fn rebuild_stil_display(&mut self) {
        let subtune = self
            .status
            .track_info
            .as_ref()
            .map(|i| i.current_song)
            .unwrap_or(1);
        self.stil_display_text = match self.stil_entry.as_ref() {
            Some(e) => e.format_for_display(subtune),
            None => {
                // For MUS files: load WDS lyrics and MUS embedded credits.
                // WDS lyrics go to karaoke; credits + lyrics go to STIL overlay.
                self.karaoke_flag_times.clear();
                // Extract path and state from the current entry before mutating.
                let mus_info = self.playlist.current_entry().map(|entry| {
                    let is_mus = entry
                        .path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("mus"))
                        .unwrap_or(false);
                    (entry.path.clone(), is_mus, entry.duration_secs.is_some())
                });
                if let Some((mus_path, is_mus, _has_dur)) = mus_info {
                    if !is_mus {
                        String::new()
                    } else {
                        let mut parts = Vec::new();

                        // Use libsidplayfp comments (properly converted PETSCII→ASCII).
                        // Only if the track_info matches the current track path.
                        if let Some(ref info) = self.status.track_info {
                            if info.path == mus_path && !info.mus_comments.is_empty() {
                                parts.push(info.mus_comments.join("\n"));
                            }
                        }

                        // Check for FLAG commands in MUS and companion STR file.
                        if let Ok(mus_data) = std::fs::read(&mus_path) {
                            self.karaoke_has_flags = petscii::mus_has_flags(&mus_data);
                        }
                        if !self.karaoke_has_flags {
                            // Stereo voices 4-6 are in a separate .str file;
                            // FLAGs may live there instead.
                            for ext in &["str", "STR"] {
                                let str_path = mus_path.with_extension(ext);
                                if let Ok(str_data) = std::fs::read(&str_path) {
                                    if petscii::mus_has_flags(&str_data) {
                                        self.karaoke_has_flags = true;
                                        break;
                                    }
                                }
                            }
                        }

                        // Load WDS lyrics as logical groups.
                        if let Some(groups) = petscii::load_wds_lyrics(&mus_path) {
                            let total_rows: usize = groups.iter().map(|g| g.len()).sum();
                            eprintln!(
                                "[phosphor] Karaoke: {} lyric groups ({} display rows)",
                                groups.len(),
                                total_rows,
                            );
                            // Flatten groups into plain text for STIL overlay.
                            let flat: String = groups
                                .iter()
                                .flat_map(|g| g.iter().map(String::as_str))
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !parts.is_empty() {
                                parts.push(String::new());
                            }
                            parts.push(flat);
                            self.karaoke_groups = groups;
                        } else {
                            self.karaoke_groups.clear();
                        }

                        parts.join("\n")
                    }
                } else {
                    String::new()
                }
            }
        };
    }

    fn poll_status(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            self.status = status;
        }
        // Pump device-config results from the player thread into the same
        // update flow as everything else. Each event populates / resets
        // device_cfg + device_cfg_status.
        while let Ok(event) = self.device_cfg_rx.try_recv() {
            match event {
                Ok(snap) => {
                    self.device_cfg = Some(snap);
                    self.device_cfg_status = "Loaded.".into();
                }
                Err(e) => {
                    self.device_cfg_status = format!("Error: {e}");
                }
            }
        }

        // Drain HVSC rsync sync events, if a sync is in flight. Same
        // try_recv-loop pattern as status_rx — fires at 33ms cadence.
        if let Some(handle) = self.hvsc_sync.as_ref() {
            let mut done: Option<Result<(), String>> = None;
            while let Ok(ev) = handle.rx.try_recv() {
                match ev {
                    hvsc_sync::HvscSyncEvent::Progress {
                        files_done,
                        files_total,
                        bytes_done,
                        bytes_total: _,
                        current,
                    } => {
                        // Progress bar uses files (we know those exactly).
                        // Hide the bar while files_total is 0 (early
                        // listing phase) by leaving hvsc_sync_progress=None.
                        self.hvsc_sync_progress = if files_total > 0 {
                            Some((files_done, files_total))
                        } else {
                            None
                        };
                        let mb_done = bytes_done / (1024 * 1024);
                        self.hvsc_sync_status = if current.is_empty() {
                            format!("Listing… {} files queued", files_total)
                        } else if files_total > 0 {
                            // Show files for the headline metric + bytes
                            // downloaded so far (no total — unknown).
                            format!(
                                "[{}/{} files] {} MB — {}",
                                files_done, files_total, mb_done, current
                            )
                        } else {
                            current
                        };
                    }
                    hvsc_sync::HvscSyncEvent::Done(result) => {
                        done = Some(result);
                    }
                }
            }
            if let Some(result) = done {
                self.hvsc_sync = None;
                self.hvsc_sync_progress = None;
                match result {
                    Ok(()) => {
                        // Stamp the timestamp in the config.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        // Simple ISO-8601 from epoch — avoids pulling in chrono
                        // at this layer (we already have it transitively via
                        // arrsync-phosphor, but keep the boundary clean).
                        self.config.hvsc_last_sync = Some(format_iso8601(now));
                        self.config.save();

                        // Auto-repoint Songlength + STIL paths to the newly
                        // synced copies, if not already pointing into the
                        // HVSC root.
                        self.apply_post_hvsc_sync();
                        self.hvsc_sync_status = format!(
                            "Done. Last synced: {}",
                            self.config.hvsc_last_sync.as_deref().unwrap_or("")
                        );
                    }
                    Err(e) => {
                        self.hvsc_sync_status = format!("Error: {e}");
                    }
                }
            }
        }

        // Drop stale timing data from statuses that pre-date our most recent
        // SetSubtune. The player thread runs its drain at the START of each
        // frame, so a SetSubtune cmd queued mid-frame doesn't get picked up
        // until the NEXT frame — and the current frame still finishes engine
        // + send_status using the OLD ctx. That status arrives in status_rx
        // and, without this filter, would overwrite the cleared state we set
        // in clear_advance_status(), causing auto-advance to re-fire on
        // stale `elapsed` / `u64_screen_elapsed_secs`.
        //
        // Signal: the status carries `track_info.current_song` from the ctx
        // that produced it. If main.rs has already moved the playlist
        // entry's selected_song forward (post-SetSubtune), a mismatch means
        // this status is from the previous subtune — zero the timing fields
        // so auto-advance sees `elapsed=0 < dur` and doesn't fire.
        let stale = match (
            self.status.track_info.as_ref(),
            self.playlist
                .current
                .and_then(|i| self.playlist.entries.get(i)),
        ) {
            (Some(info), Some(entry)) => info.current_song != entry.selected_song,
            _ => false,
        };
        if stale {
            self.status.elapsed = Duration::ZERO;
            self.status.writes_per_frame = 0;
            self.status.u64_screen_elapsed_secs = None;
            self.status.u64_screen_read_at = None;
            self.status.u64_screen_total_secs = None;
        }

        // Advance karaoke line when new FLAG events are detected.
        if self.status.flag_count > self.last_flag_count {
            let new_flags = self.status.flag_count - self.last_flag_count;
            self.karaoke_line += new_flags as usize;
            self.last_flag_count = self.status.flag_count;
        }

        if let Some(ref info) = self.status.track_info {
            self.visualizer.set_num_sids(info.num_sids);
            // Keep the expanded-overlay info in sync with the current track.
            let current_duration = self
                .playlist
                .current_entry()
                .and_then(|e| e.duration_secs)
                .map(|d| d as f32);
            self.vis_expanded_info = Some(ui::visualizer::ExpandedInfo {
                name: info.name.clone(),
                author: info.author.clone(),
                released: info.released.clone(),
                sid_type: info.sid_type.clone(),
                current_song: info.current_song,
                songs: info.songs,
                is_pal: info.is_pal,
                is_rsid: info.is_rsid,
                is_mus: info
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("mus"))
                    .unwrap_or(false),
                num_sids: info.num_sids,
                elapsed_secs: self.status.elapsed.as_secs_f32(),
                duration_secs: current_duration,
                stil_text: self.stil_display_text.clone(),
                stil_scroll_x: self.stil_scroll_x,
                karaoke_groups: self.karaoke_groups.clone(),
                karaoke_line: self.karaoke_line,
            });
        } else {
            self.vis_expanded_info = None;
        }
        // On USB hardware, the USBSID-Pico's stereo output has SID1 on
        // the right channel and SID2 on the left.  Swap the voice level
        // groups so the visualizer matches what you actually hear.
        let levels = if self.config.output_engine == "usb" && self.status.voice_levels.len() == 6 {
            let mut swapped = self.status.voice_levels.clone();
            // Swap SID1 voices (0-2) with SID2 voices (3-5)
            swapped.swap(0, 3);
            swapped.swap(1, 4);
            swapped.swap(2, 5);
            swapped
        } else {
            self.status.voice_levels.clone()
        };
        self.visualizer.update(&levels);

        if self.status.state == PlayState::Playing {
            // Track silence — when SID writes drop to zero for ~3 seconds
            // (90 frames at 30fps tick rate), the song has ended.
            if self.status.writes_per_frame == 0 {
                self.silence_frames += 1;
            } else {
                self.silence_frames = 0;
            }

            if let Some(cur_idx) = self.playlist.current {
                let advance_info = self.playlist.entries.get(cur_idx).and_then(|entry| {
                    // Prefer the U64's on-screen total when HVSC has no entry —
                    // it's whatever the U64 SID-player UI shows next to the timer.
                    let dur = entry
                        .duration_secs
                        .or_else(|| self.status.u64_screen_total_secs.map(|s| s as u32));
                    // Prefer the U64's on-screen elapsed seconds over host wall-clock
                    // so playback advances based on actual hardware position, not on
                    // host time that started counting before the C64 produced audio.
                    // Interpolate sub-second time using the wall-clock delta since
                    // the last successful read — without this, elapsed lags up to
                    // ~1.5 s behind reality (1 s screen-render granularity + 0.5 s
                    // poll period). Falls back to wall-clock for non-U64 engines or
                    // when the U64's player UI couldn't be parsed.
                    let elapsed = match (
                        self.status.u64_screen_elapsed_secs,
                        self.status.u64_screen_read_at,
                    ) {
                        (Some(secs), Some(read_at)) => secs as u64 + read_at.elapsed().as_secs(),
                        _ => self.status.elapsed.as_secs(),
                    };
                    // Advance if duration exceeded OR prolonged silence detected.
                    // Silence detection: ~90 frames ≈ 3 seconds at 30fps tick.
                    // Only trigger after at least 5 seconds of playback to avoid
                    // false positives during song intro.
                    let silence_ended = self.silence_frames > 90 && elapsed > 5;
                    let duration_ended = dur.map_or(false, |d| elapsed >= d as u64);

                    if duration_ended || silence_ended {
                        if silence_ended && dur.is_none() {
                            eprintln!("[phosphor] Silence detected after {}s — advancing", elapsed);
                        }
                        let trigger = if duration_ended {
                            "duration"
                        } else {
                            "silence"
                        };
                        Some((
                            entry.selected_song,
                            entry.songs,
                            entry.md5.clone(),
                            elapsed,
                            dur,
                            trigger,
                        ))
                    } else {
                        None
                    }
                });

                if let Some((cur_song, total_songs, md5, elapsed, dur, trigger)) = advance_info {
                    // Debounce: cap auto-advance at one per 500 ms.  Even if there's
                    // a stale-status race we haven't located, this bounds the
                    // user-visible symptom (subtune skipping by 1 every transition)
                    // to at most one advance per real subtune end.
                    let now = Instant::now();
                    let suppressed = self
                        .last_advance_at
                        .map(|t| now.duration_since(t) < Duration::from_millis(500))
                        .unwrap_or(false);
                    if suppressed {
                        // Log only once per debounce window — we only care about
                        // the FIRST suppressed candidate per real subtune end.
                        // Subsequent Ticks within the window are normal artefacts
                        // of the stale-status race and would just spam stderr.
                        if !self.advance_suppress_logged {
                            let since = self
                                .last_advance_at
                                .map(|t| now.duration_since(t))
                                .unwrap_or_default();
                            eprintln!(
                                "[advance] SUPPRESSED by debounce ({:?} since last)  cur_song={}/{} elapsed={} dur={:?} u64_secs={:?} silence_frames={} trigger={}",
                                since, cur_song, total_songs, elapsed, dur,
                                self.status.u64_screen_elapsed_secs, self.silence_frames, trigger,
                            );
                            self.advance_suppress_logged = true;
                        }
                    } else {
                        eprintln!(
                            "[advance] cur_song={}/{} elapsed={} dur={:?} u64_secs={:?} silence_frames={} trigger={}",
                            cur_song, total_songs, elapsed, dur,
                            self.status.u64_screen_elapsed_secs, self.silence_frames, trigger,
                        );
                        self.last_advance_at = Some(now);
                        self.advance_suppress_logged = false;
                        if cur_song < total_songs {
                            let next_song = cur_song + 1;
                            let subtune_idx = (next_song - 1) as usize;
                            let next_dur = md5
                                .as_ref()
                                .and_then(|m| {
                                    self.songlength_db
                                        .as_ref()
                                        .and_then(|db| db.lookup(m, subtune_idx))
                                })
                                .or_else(|| {
                                    let d = self.config.default_song_length_secs;
                                    if d > 0 {
                                        Some(d)
                                    } else {
                                        None
                                    }
                                });
                            let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(next_song));
                            self.clear_advance_status();
                            if let Some(e) = self.playlist.entries.get_mut(cur_idx) {
                                e.selected_song = next_song;
                                e.duration_secs = next_dur;
                            }
                        } else {
                            let first_dur = md5
                                .as_ref()
                                .and_then(|m| {
                                    self.songlength_db.as_ref().and_then(|db| db.lookup(m, 0))
                                })
                                .or_else(|| {
                                    let d = self.config.default_song_length_secs;
                                    if d > 0 {
                                        Some(d)
                                    } else {
                                        None
                                    }
                                });
                            if let Some(e) = self.playlist.entries.get_mut(cur_idx) {
                                e.selected_song = 1;
                                e.duration_secs = first_dur;
                            }
                            if let Some(idx) = self.playlist.next() {
                                self.play_track(idx);
                            } else {
                                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Update the current playlist entry's selected_song and duration_secs
    /// when the user manually changes the subtune via the tune buttons.
    fn update_entry_subtune(&mut self, song: u16) {
        if let Some(cur_idx) = self.playlist.current {
            let md5 = self
                .playlist
                .entries
                .get(cur_idx)
                .and_then(|e| e.md5.clone());
            let new_dur = md5
                .as_deref()
                .and_then(|m| {
                    self.songlength_db
                        .as_ref()
                        .and_then(|db| db.lookup(m, (song - 1) as usize))
                })
                .or_else(|| {
                    let d = self.config.default_song_length_secs;
                    if d > 0 {
                        Some(d)
                    } else {
                        None
                    }
                });
            if let Some(e) = self.playlist.entries.get_mut(cur_idx) {
                e.selected_song = song;
                e.duration_secs = new_dur;
            }
        }
    }

    /// Push current status + playlist snapshot to the remote HTTP server.
    fn update_remote_state(&self) {
        if let Ok(mut rs) = self.remote_state.try_lock() {
            let info = self.status.track_info.as_ref();

            let current_md5 = self.playlist.current_entry().and_then(|e| e.md5.as_deref());
            let is_favorite_current = current_md5
                .map(|m| self.favorites.hashes.contains(m))
                .unwrap_or(false);

            // Sleep timer countdown — seconds until deadline. `None` when
            // no timer is armed.
            let sleep_remaining_secs = self
                .sleep_deadline
                .map(|dl| dl.saturating_duration_since(Instant::now()).as_secs() as u32);

            let active_published_playlist = match &self.session_mode {
                SessionMode::PublishedReadOnly { file } => Some(file.clone()),
                _ => None,
            };

            rs.status = remote::RemoteStatus {
                state: match self.status.state {
                    PlayState::Playing => "playing",
                    PlayState::Paused => "paused",
                    PlayState::Stopped => "stopped",
                }
                .to_string(),
                title: info.map(|i| i.name.clone()).unwrap_or_default(),
                author: info.map(|i| i.author.clone()).unwrap_or_default(),
                released: info.map(|i| i.released.clone()).unwrap_or_default(),
                current_song: info.map(|i| i.current_song).unwrap_or(0),
                songs: info.map(|i| i.songs).unwrap_or(0),
                elapsed_secs: self.status.elapsed.as_secs_f32(),
                duration_secs: self
                    .playlist
                    .current_entry()
                    .and_then(|e| e.duration_secs)
                    .map(|d| d as f32),
                current_index: self.playlist.current,
                num_sids: info.map(|i| i.num_sids).unwrap_or(1),
                sid_type: info.map(|i| i.sid_type.clone()).unwrap_or_default(),
                is_pal: info.map(|i| i.is_pal).unwrap_or(true),
                engine: self.config.output_engine.clone(),
                is_favorite: is_favorite_current,
                master_volume: self.config.master_volume,
                shuffle: self.playlist.shuffle,
                repeat: match self.playlist.repeat {
                    playlist::RepeatMode::Off => "off",
                    playlist::RepeatMode::All => "all",
                    playlist::RepeatMode::Single => "one",
                }
                .to_string(),
                sleep_selected_mins: self.sleep_selected_mins,
                sleep_remaining_secs,
                hvsc_sync_active: self.hvsc_sync.is_some(),
                hvsc_sync_progress: self.hvsc_sync_progress.map(|(done, total)| [done, total]),
                active_published_playlist,
            };

            // Snapshot hvsc_root + published manifest for the library
            // browse endpoints on the HTTP thread.
            rs.hvsc_root = self.config.hvsc_root.clone().map(PathBuf::from);
            rs.published_manifest = self.published_playlists_browser.manifest().cloned();

            // Rebuild playlist snapshot when entries OR favourites change.
            // We stuff both into one epoch so the version check catches both.
            let favs_epoch = self.favorites.hashes.len() as u64;
            let version = ((self.playlist.len() as u64) << 32) | favs_epoch;
            if rs.playlist_version != version {
                let fav_set = &self.favorites.hashes;
                rs.playlist = self
                    .playlist
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| remote::RemotePlaylistEntry {
                        index: i,
                        title: e.title.clone(),
                        author: e.author.clone(),
                        duration: e.duration_secs,
                        num_sids: e.num_sids,
                        is_rsid: e.is_rsid,
                        is_favorite: e
                            .md5
                            .as_deref()
                            .map(|m| fav_set.contains(m))
                            .unwrap_or(false),
                    })
                    .collect();
                rs.playlist_version = version;
            }
        }
    }

    /// Process commands from the HTTP remote control server.
    ///
    /// Returns a `Task<Message>` batch so commands that map to a full
    /// `Message::…` handler (Surprise / LoadPublishedPlaylist / etc.)
    /// can trigger the same code path the desktop UI uses. Simple ops
    /// (favourites toggle, shuffle, volume) are executed in-line and
    /// contribute `Task::none()` to the batch.
    fn poll_remote_commands(&mut self) -> Task<Message> {
        let mut tasks: Vec<Task<Message>> = Vec::new();
        while let Ok(cmd) = self.remote_cmd_rx.try_recv() {
            match cmd {
                remote::RemoteCmd::PlayTrack(idx) => {
                    self.play_track(idx);
                }
                remote::RemoteCmd::Stop => {
                    let _ = self.cmd_tx.send(PlayerCmd::Stop);
                }
                remote::RemoteCmd::TogglePause => {
                    let _ = self.cmd_tx.send(PlayerCmd::TogglePause);
                }
                remote::RemoteCmd::NextTrack => {
                    if let Some(cur) = self.playlist.current {
                        if cur + 1 < self.playlist.len() {
                            self.play_track(cur + 1);
                        }
                    }
                }
                remote::RemoteCmd::PrevTrack => {
                    if let Some(cur) = self.playlist.current {
                        if cur > 0 {
                            self.play_track(cur - 1);
                        }
                    }
                }
                remote::RemoteCmd::SetSubtune(n) => {
                    let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(n));
                    self.clear_advance_status();
                }

                // ── Playback QOL (in-line, no Task needed) ───────────
                remote::RemoteCmd::ToggleFavorite(idx) => {
                    if let Some(md5) = self.playlist.entries.get(idx).and_then(|e| e.md5.clone()) {
                        let _ = self.favorites.toggle(&md5);
                        self.favorites.save();
                    }
                }
                remote::RemoteCmd::ToggleFavoriteCurrent => {
                    let md5 = self.playlist.current_entry().and_then(|e| e.md5.clone());
                    if let Some(md5) = md5 {
                        let _ = self.favorites.toggle(&md5);
                        self.favorites.save();
                    }
                }
                remote::RemoteCmd::ToggleShuffle => {
                    self.playlist.toggle_shuffle();
                }
                remote::RemoteCmd::CycleRepeat => {
                    self.playlist.repeat = self.playlist.repeat.cycle();
                }
                remote::RemoteCmd::SetSleepTimer(mins) => match mins {
                    Some(m) if m > 0 => {
                        self.sleep_deadline =
                            Some(Instant::now() + Duration::from_secs((m as u64) * 60));
                        self.sleep_selected_mins = Some(m);
                    }
                    _ => {
                        self.sleep_deadline = None;
                        self.sleep_selected_mins = None;
                    }
                },
                remote::RemoteCmd::SetVolume(v) => {
                    self.config.master_volume = v.clamp(0.0, 1.0);
                    self.config.save();
                }

                // ── Ops that reuse full Message handlers via Task::done
                remote::RemoteCmd::Surprise => {
                    tasks.push(Task::done(Message::HvscBrowserSurpriseMe));
                }
                remote::RemoteCmd::LoadPublishedPlaylist(file) => {
                    tasks.push(Task::done(Message::PublishedPlaylistsLoad(file)));
                }
                remote::RemoteCmd::RestoreDefaultPlaylist => {
                    tasks.push(Task::done(Message::PublishedPlaylistsRestoreDefault));
                }

                // ── HVSC direct play / add by absolute path ──────────
                // Reuses the same PlaylistEntry::from_path + add_entries +
                // songlength chain as the HvscBrowserPlayTune handler.
                remote::RemoteCmd::HvscPlay(path) => {
                    self.direct_hvsc_action(path, /*play=*/ true);
                }
                remote::RemoteCmd::HvscAdd(path) => {
                    self.direct_hvsc_action(path, /*play=*/ false);
                }
            }
        }
        if tasks.is_empty() {
            Task::none()
        } else {
            Task::batch(tasks)
        }
    }

    /// Realise a single SID at an absolute path (typically inside the
    /// HVSC tree, but any path works), add it to the playlist, apply
    /// songlengths, and optionally start playback. Shared by the two
    /// remote `HvscPlay` / `HvscAdd` commands.
    fn direct_hvsc_action(&mut self, path: PathBuf, play: bool) {
        let entry = match playlist::PlaylistEntry::from_path(&path) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[remote] HVSC direct action failed: {e}");
                return;
            }
        };
        let entry_path = entry.path.clone();
        let song = entry.selected_song.max(1);
        self.playlist.add_entries(vec![entry]);
        if let Some(db) = self.songlength_db.as_ref() {
            db.apply_to_playlist(
                &mut self.playlist,
                self.config.hvsc_root.as_deref().map(std::path::Path::new),
            );
        }
        self.rebuild_filter();
        if let Some(abs_i) = self
            .playlist
            .entries
            .iter()
            .position(|e| e.path == entry_path)
        {
            self.selected = Some(abs_i);
            if play {
                let _ = self.cmd_tx.send(player::PlayerCmd::Play {
                    path: entry_path,
                    song,
                    force_stereo: self.config.force_stereo_2sid
                        || std::env::args().any(|a| a == "--stereo"),
                    sid4_addr: parse_sid4_from_args(),
                    audio_port: if self.config.u64_audio_enabled {
                        Some(self.config.u64_audio_port)
                    } else {
                        None
                    },
                    restart_usb_on_load: self.config.restart_usb_on_load,
                });
            }
        }
    }

    fn apply_songlengths(&mut self) {
        if let Some(ref db) = self.songlength_db {
            db.apply_to_playlist(
                &mut self.playlist,
                self.config.hvsc_root.as_deref().map(std::path::Path::new),
            );
        }
        if self.config.default_song_length_secs > 0 {
            apply_default_length(&mut self.playlist, self.config.default_song_length_secs);
        }
    }

    fn rebuild_filter(&mut self) {
        let mut indices = ui::filter_playlist(
            &self.playlist,
            &self.search_text,
            self.favorites_only,
            &self.favorites,
        );
        sort_indices(
            &self.playlist,
            &mut indices,
            self.sort_column,
            self.sort_direction,
        );
        self.filtered_indices = indices;
        // Scroll back to top whenever the visible set changes so the virtual
        // window starts at row 0 rather than mid-list with wrong rows shown.
        self.playlist_scroll_offset_y = 0.0;
    }

    /// Parse a cached published M3U into a `Vec<PreviewTrack>` off-thread
    /// so a 100-track preview doesn't stall the UI.
    fn spawn_published_preview_parse(&self, file: String) -> Task<Message> {
        let cache_dir = published_playlists_cache_dir();
        let cached_path = cache_dir.join(&file);
        let file_for_async = file.clone();
        Task::perform(
            async move {
                let res = std::fs::read_to_string(&cached_path)
                    .map(|s| playlist::parse_m3u_preview(&s))
                    .map_err(|e| format!("Read {}: {e}", cached_path.display()));
                (file_for_async, res)
            },
            |(f, r)| Message::PublishedPlaylistsPreviewDone(f, r),
        )
    }

    /// Fire one `list_files` per search hit so we can hide releases
    /// that contain no playable `.sid` files. Cache lives in
    /// `assembly64_browser.file_cache`, so a later manual expand on a
    /// verified entry is instant. Failures leave the row visible.
    fn spawn_assembly64_prefetches(
        &mut self,
        page: &[crate::assembly64::AsmEntry],
    ) -> Vec<Task<Message>> {
        if page.is_empty() {
            return Vec::new();
        }
        self.assembly64_browser
            .note_prefetch_started(page.len() as u32);
        page.iter()
            .map(|entry| {
                let client = self.assembly64_client.clone();
                let id = entry.id.clone();
                let cat = entry.category;
                Task::perform(
                    async move {
                        let res = client.list_files(&id, cat).await.map_err(|e| e.to_string());
                        (id, res)
                    },
                    |(id, res)| Message::Assembly64PrefetchDone(id, res),
                )
            })
            .collect()
    }

    fn start_assembly64_download(
        &mut self,
        item_id: String,
        category_id: u32,
        file_id: u32,
        file_path: String,
        play: bool,
    ) -> Task<Message> {
        let client = self.assembly64_client.clone();
        let cache_root = match config::config_dir() {
            Some(d) => d.join("assembly64_cache"),
            None => std::env::temp_dir().join("phosphor_assembly64_cache"),
        };
        let id_for_dir = item_id.clone();
        let filename = sanitise_assembly64_filename(&file_path);

        Task::perform(
            async move {
                let bytes = client
                    .download(&id_for_dir, category_id, file_id)
                    .await
                    .map_err(|e| e.to_string())?;
                let target_dir = cache_root.join(&id_for_dir);
                std::fs::create_dir_all(&target_dir)
                    .map_err(|e| format!("create cache dir: {e}"))?;
                let target = target_dir.join(&filename);
                std::fs::write(&target, &bytes).map_err(|e| format!("write cache file: {e}"))?;
                Ok::<PathBuf, String>(target)
            },
            move |result| Message::Assembly64DownloadDone(result, play, 0),
        )
    }
}

/// Wrap a bare search term as `name:"…" category:music` so Assembly64
/// returns releases that actually contain playable SID files.
///
/// Why two clauses: the server rejects free text (HTTP 463), and
/// standalone `.sid` files live almost exclusively under the Music
/// category — Games/Demos bundle their music inside D64 disk images
/// which we can't unpack. Without the filter, searches turn up
/// releases with zero playable files (e.g. "Commando II" the game).
///
/// If the user types any `:` we trust them and pass through verbatim —
/// power-users can opt out with bare `name:"commando"`, widen with
/// `category:demos commando`, etc. The `name:"…"` form (quoted phrase,
/// not `*term*` glob) is the current substring-search idiom — globs
/// were dropped in a 2026-05 API change.
fn normalise_assembly64_query(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "category:music sort:updated order:desc".to_string();
    }
    if trimmed.contains(':') {
        return trimmed.to_string();
    }
    let phrase = trimmed.replace('"', "");
    format!("name:\"{phrase}\" category:music")
}

/// Cache directory for downloaded published playlists +
/// the last-seen manifest. Lives under the standard app-data path.
fn published_playlists_cache_dir() -> PathBuf {
    config::config_dir()
        .map(|d| d.join("published_playlists"))
        .unwrap_or_else(|| std::env::temp_dir().join("phosphor_published_playlists"))
}

fn sanitise_assembly64_filename(path: &str) -> String {
    // The wire path is e.g. "MUSICIANS/H/Hubbard_Rob/Commando.sid". We want
    // just the basename, with any path-unsafe chars replaced. If the file has
    // no .sid-family extension already, force `.sid`.
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+' | '(' | ')' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let lower = out.to_ascii_lowercase();
    if !(lower.ends_with(".sid") || lower.ends_with(".psid") || lower.ends_with(".rsid")) {
        out.push_str(".sid");
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
//  Sort helper
// ─────────────────────────────────────────────────────────────────────────────

fn sort_indices(
    playlist: &Playlist,
    indices: &mut Vec<usize>,
    col: SortColumn,
    dir: SortDirection,
) {
    indices.sort_by(|&a, &b| {
        let ea = &playlist.entries[a];
        let eb = &playlist.entries[b];
        let ord = match col {
            SortColumn::Index => a.cmp(&b),
            SortColumn::Title => ea.title.to_lowercase().cmp(&eb.title.to_lowercase()),
            SortColumn::Author => ea.author.to_lowercase().cmp(&eb.author.to_lowercase()),
            SortColumn::Released => ea.released.to_lowercase().cmp(&eb.released.to_lowercase()),
            SortColumn::Duration => match (ea.duration_secs, eb.duration_secs) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(da), Some(db)) => da.cmp(&db),
            },
            SortColumn::SidType => ea.is_rsid.cmp(&eb.is_rsid),
            SortColumn::NumSids => ea.num_sids.cmp(&eb.num_sids),
        };
        if dir == SortDirection::Descending {
            ord.reverse()
        } else {
            ord
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  Misc helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Format a Unix timestamp as "YYYY-MM-DD HH:MM:SS UTC" for display.
/// Tiny date math — avoids pulling a date crate at this layer.
fn format_iso8601(secs: u64) -> String {
    // Days since 1970-01-01
    let day = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let hh = time / 3600;
    let mm = (time % 3600) / 60;
    let ss = time % 60;

    // Howard Hinnant's algorithm — civil_from_days
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y, m, d, hh, mm, ss
    )
}

// ─────────────────────────────────────────────────────────────────────────────
//  Playlist helpers
// ─────────────────────────────────────────────────────────────────────────────

fn apply_default_length(playlist: &mut Playlist, default_secs: u32) {
    let mut count = 0;
    for entry in &mut playlist.entries {
        if entry.duration_secs.is_none() {
            // Skip MUS files — they use silence detection for song endings.
            let is_mus = entry
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mus"))
                .unwrap_or(false);
            if is_mus {
                continue;
            }
            entry.duration_secs = Some(default_secs);
            count += 1;
        }
    }
    if count > 0 {
        eprintln!("[phosphor] Applied default length ({default_secs}s) to {count} entries");
    }
}

fn clear_default_lengths(playlist: &mut Playlist) {
    for entry in &mut playlist.entries {
        entry.duration_secs = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Drop
// ─────────────────────────────────────────────────────────────────────────────

impl Drop for App {
    fn drop(&mut self) {
        eprintln!("[phosphor] App closing, stopping playback...");
        // Only save session if the background load completed.
        // Otherwise the playlist is empty and we'd delete the
        // existing session file.
        //
        // Skip the save entirely when a published playlist is loaded:
        // the user's own default lives untouched in session_playlist.m3u
        // and we never want to clobber it with the read-only contents.
        match &self.session_mode {
            SessionMode::Default if self.session_loaded => {
                self.playlist.save_session();
            }
            SessionMode::Default => {}
            SessionMode::PublishedReadOnly { file } => {
                eprintln!(
                    "[phosphor] Published playlist '{file}' active — \
                     skipping session save to preserve your default"
                );
            }
        }
        self.heard_db.save();
        let _ = self.cmd_tx.send(PlayerCmd::Stop);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = self.cmd_tx.send(PlayerCmd::Quit);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  File dialogs
// ─────────────────────────────────────────────────────────────────────────────

async fn pick_files(start_dir: Option<String>) -> Vec<PathBuf> {
    let mut d = rfd::AsyncFileDialog::new()
        .set_title("Add SID files")
        .add_filter("SID files", &["sid", "SID", "mus", "MUS"]);
    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            d = d.set_directory(&p);
        }
    }
    rfd::AsyncFileDialog::pick_files(d)
        .await
        .map(|h| h.iter().map(|f| f.path().to_path_buf()).collect())
        .unwrap_or_default()
}

async fn pick_folder(start_dir: Option<String>) -> Option<PathBuf> {
    let mut d = rfd::AsyncFileDialog::new().set_title("Add folder of SID files");
    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            d = d.set_directory(&p);
        }
    }
    d.pick_folder().await.map(|h| h.path().to_path_buf())
}

async fn pick_stil_file() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Load STIL.txt")
        .add_filter("STIL", &["txt"])
        .add_filter("All files", &["*"])
        .pick_file()
        .await
        .map(|h| h.path().to_path_buf())
}

async fn pick_songlength_file(start_dir: Option<String>) -> Option<PathBuf> {
    let mut d = rfd::AsyncFileDialog::new()
        .set_title("Load HVSC Songlength.md5")
        .add_filter("Songlength", &["md5", "txt"]);
    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            d = d.set_directory(&p);
        }
    }
    d.pick_file().await.map(|h| h.path().to_path_buf())
}

async fn pick_playlist_file(start_dir: Option<String>) -> Option<PathBuf> {
    let mut d = rfd::AsyncFileDialog::new()
        .set_title("Open Playlist")
        .add_filter("Playlists", &["m3u", "m3u8", "pls"])
        .add_filter("All files", &["*"]);
    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            d = d.set_directory(&p);
        }
    }
    d.pick_file().await.map(|h| h.path().to_path_buf())
}

async fn save_playlist_dialog(
    entries: Vec<(PathBuf, String, String, Option<u32>)>,
    start_dir: Option<String>,
) -> Result<PathBuf, String> {
    let mut d = rfd::AsyncFileDialog::new()
        .set_title("Save Playlist")
        .add_filter("M3U Playlist", &["m3u"])
        .set_file_name("playlist.m3u");
    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            d = d.set_directory(&p);
        }
    }
    match d.save_file().await {
        Some(h) => {
            let path = h.path().to_path_buf();
            write_m3u(&path, &entries)?;
            Ok(path)
        }
        None => Err("Cancelled".into()),
    }
}

fn write_m3u(
    path: &std::path::Path,
    entries: &[(PathBuf, String, String, Option<u32>)],
) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    writeln!(f, "#EXTM3U").map_err(|e| format!("{e}"))?;
    for (file_path, author, title, duration) in entries {
        let dur = duration.unwrap_or(0) as i64;
        let display = if author.is_empty() {
            title.clone()
        } else {
            format!("{author} - {title}")
        };
        writeln!(f, "#EXTINF:{dur},{display}").map_err(|e| format!("{e}"))?;
        writeln!(f, "{}", file_path.display()).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
//  CLI helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Compact human summary of the SID chips actually present on a USBSID-Pico.
/// Reads each enabled socket's chip type / SID model and joins them.
///
/// SID models come from `SidType::label()`; clone chips come from
/// `ChipType::label()` and cover **every non-`Real`, non-`Unknown` variant**
/// in `usbsid-pico-config` — i.e. SKPico, ARMSID, ARM2SID, FPGASID,
/// RedipSID, PDSID, BackSID, SIDEmu. No per-chip special-casing here.
///
/// Examples:
///   - `"2× MOS8580"`              both sockets enabled, same chip
///   - `"MOS6581 + MOS8580"`       different real chips per socket
///   - `"MOS8580 (FPGASID)"`       clone chip emulating an 8580
///   - `"SKPico + ARMSID"`         two different clones, no real SID
///   - `""`                        firmware gave us nothing useful
fn format_usb_chip_summary(snap: &ui::DeviceConfigSnapshot) -> String {
    use usbsid_pico_config::{ChipType, SidType};
    fn one(s: &usbsid_pico_config::SocketConfig) -> Option<String> {
        if !s.enabled {
            return None;
        }
        let sid = match s.sid1.kind {
            SidType::Unknown | SidType::Na => None,
            other => Some(other.label().to_string()),
        };
        let chip = match s.chip_type {
            ChipType::Real | ChipType::Unknown => None,
            other => Some(other.label().to_string()),
        };
        match (sid, chip) {
            (Some(s), Some(c)) => Some(format!("{s} ({c})")),
            (Some(s), None) => Some(s),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }
    let parts: Vec<String> = [&snap.config.socket1, &snap.config.socket2]
        .iter()
        .filter_map(|s| one(s))
        .collect();
    match parts.len() {
        0 => String::new(),
        1 => parts.into_iter().next().unwrap(),
        2 if parts[0] == parts[1] => format!("2× {}", parts[0]),
        _ => parts.join(" + "),
    }
}

/// Diagnostic walker for `--check-numsids`. Calls the exact same
/// `PlaylistEntry::from_path` every real load path uses, prints what
/// num_sids it computes plus the underlying header bytes, and flags
/// any file whose computed num_sids would render as multi-SID in the
/// playlist column (i.e. > 1).
///
/// Stats summary at the end so the user can paste back something short
/// like "1234 files, 1238 single-SID, 56 multi-SID" instead of dumping
/// the whole tree.
fn check_numsids(root: &std::path::Path) {
    use walkdir::WalkDir;

    let mut total: usize = 0;
    let mut single: usize = 0;
    let mut multi: usize = 0;
    let mut errors: usize = 0;

    let iter: Box<dyn Iterator<Item = walkdir::DirEntry>> = if root.is_file() {
        // Single-file mode: just process the one file.
        Box::new(
            WalkDir::new(root)
                .max_depth(0)
                .into_iter()
                .filter_map(|e| e.ok()),
        )
    } else {
        Box::new(WalkDir::new(root).into_iter().filter_map(|e| e.ok()))
    };

    for dirent in iter {
        let p = dirent.path();
        if !p.is_file() {
            continue;
        }
        let ext_ok = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "sid" | "mus"))
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }

        total += 1;
        // Read the raw bytes ourselves so we can dump 0x7A/0x7B alongside
        // what from_path reports — that's the whole point of the test.
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[ERR] {}: cannot read: {e}", p.display());
                errors += 1;
                continue;
            }
        };
        let b7a = bytes.get(0x7A).copied().unwrap_or(0);
        let b7b = bytes.get(0x7B).copied().unwrap_or(0);

        match playlist::PlaylistEntry::from_path(p) {
            Ok(entry) => {
                let tag = if entry.num_sids > 1 {
                    "MULTI"
                } else {
                    "single"
                };
                if entry.num_sids > 1 {
                    multi += 1;
                } else {
                    single += 1;
                }
                println!(
                    "[{tag}] {}: num_sids={} b[0x7A]=0x{:02X} b[0x7B]=0x{:02X}",
                    p.display(),
                    entry.num_sids,
                    b7a,
                    b7b,
                );
            }
            Err(e) => {
                eprintln!("[ERR] {}: from_path failed: {e}", p.display());
                errors += 1;
            }
        }
    }

    eprintln!(
        "\n--- check-numsids summary ---\n\
         total       : {total}\n\
         single-SID  : {single}\n\
         multi-SID   : {multi}\n\
         errors      : {errors}"
    );
}

fn parse_sid4_from_args() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|w| w[0] == "--sid4")
        .and_then(|w| parse_hex_addr(&w[1]))
        .unwrap_or(0)
}

fn parse_hex_addr(s: &str) -> Option<u16> {
    let s = s.trim();
    let hex = s
        .strip_prefix('$')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(hex, 16).ok()
}

async fn flush_frame() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

// ─────────────────────────────────────────────────────────────────────────────
//  Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> iced::Result {
    // On macOS with iced_winit 0.14, the Cocoa event loop delivers close/resize
    // events after iced's channel and wgpu resources are already torn down.
    // These arrive inside a Cocoa block marked `cannot unwind`, so any panic
    // that reaches the default hook — which calls std::backtrace — will
    // double-panic and abort.  We replace the hook entirely to handle both:
    //   • known shutdown panics  → suppress silently
    //   • real panics            → print location + message ourselves, then
    //                              abort (safe from any context)
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<unknown>");

        // Suppress known iced_winit / wgpu macOS shutdown panics.
        if msg.contains("SendError")
            || msg.contains("Disconnected")
            || msg.contains("Validation Error")
            || msg.contains("wgpu error")
            || msg.contains("StagingBelt")
            || msg.contains("still mapped")
        {
            return; // silently suppress — these are expected on window close
        }

        // For real panics, print a minimal message and let the runtime abort.
        // We do NOT call the default hook because that uses std::backtrace which
        // is itself not unwind-safe and can double-panic in a Cocoa block context.
        if let Some(loc) = info.location() {
            eprintln!("panic at {}:{}: {}", loc.file(), loc.line(), msg);
        } else {
            eprintln!("panic: {}", msg);
        }
    }));

    env_logger::init();

    // Diagnostic subcommand: walk a directory of .sid files, call the
    // regular `PlaylistEntry::from_path` on each (the same function
    // every load path in Phosphor uses), and print num_sids + relevant
    // header bytes per file. Exits without launching the GUI.
    //
    //   phosphor --check-numsids <path>
    //
    // The output is identical to what Phosphor would record at load
    // time, so any "library says 2SID, files-picker says 1" mismatch
    // is reproducible here too — or proven NOT to be in `from_path`.
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--check-numsids") {
            match args.get(i + 1).cloned() {
                Some(p) => {
                    check_numsids(std::path::Path::new(&p));
                    return Ok(());
                }
                None => {
                    eprintln!("Usage: phosphor --check-numsids <file-or-directory>");
                    std::process::exit(2);
                }
            }
        }
    }

    // Windows: pin the system timer to 1 ms resolution for the lifetime of
    // `main()`. Without this the player thread misses PAL frames whenever
    // Phosphor runs in the background (sleep granularity reverts to ~15.6 ms).
    // No-op on Linux/macOS — sleep granularity is already ~1 ms by default.
    #[cfg(windows)]
    let _hi_res_timer = windows_timer::HiResTimerGuard::raise();

    let config_for_window = Config::load();
    // Seed the global font scale before the first frame so the very first
    // render already honours the user's configured base font size.
    crate::ui::font::set_base(config_for_window.base_font_size);
    // Seed master volume too so the audio thread starts at the saved level.
    crate::audio_volume::set(config_for_window.master_volume);

    let icon = {
        let bytes = include_bytes!("../assets/phosphor.png");
        let img = image::load_from_memory(bytes)
            .expect("Failed to load icon")
            .to_rgba8();
        let (w, h) = img.dimensions();
        iced::window::icon::from_rgba(img.into_raw(), w, h).expect("Failed to create icon")
    };

    iced::application(App::boot, App::update, App::view)
        .title(|_: &App| format!("Phosphor v{}", env!("CARGO_PKG_VERSION")))
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size((
            config_for_window.window_width_saved,
            config_for_window.window_height_saved,
        ))
        .window({
            #[allow(unused_mut)]
            let mut s = iced::window::Settings {
                icon: Some(icon),
                position: match (config_for_window.window_x, config_for_window.window_y) {
                    (Some(x), Some(y)) => {
                        iced::window::Position::Specific(iced::Point::new(x as f32, y as f32))
                    }
                    _ => iced::window::Position::Default,
                },
                ..Default::default()
            };
            // Pin the X11 WM_CLASS / Wayland app_id so KDE (and other desktops
            // that key icons off the .desktop file rather than _NET_WM_ICON)
            // can match the running window to packaging/phosphor.desktop's
            // StartupWMClass=phosphor and pull the title-bar icon from there.
            #[cfg(target_os = "linux")]
            {
                s.platform_specific = iced::window::settings::PlatformSpecific {
                    application_id: "phosphor".to_string(),
                    ..Default::default()
                };
            }
            s
        })
        .run()
}

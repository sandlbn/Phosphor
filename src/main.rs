#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
mod c64_emu;
mod config;
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

#[cfg(all(feature = "usb", not(target_os = "macos")))]
mod sid_direct;

mod remote;
mod sid_emulated;
mod sid_sidlite;
mod sid_u64;

use std::path::PathBuf;
use std::time::Duration;

use std::sync::{Arc, Mutex};

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
    /// Raw text from the default-song-length input field (may be mid-edit).
    default_length_text: String,
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
    /// Whether the recently played panel is visible instead of the playlist.
    show_recently_played: bool,
    /// Whether the SID register info panel is visible instead of the playlist.
    show_sid_panel: bool,

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
        let config = Config::load();
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

        let (cmd_tx, status_rx) = player::spawn_player(
            config.output_engine(),
            config.u64_address.clone(),
            config.u64_password.clone(),
        );

        let playlist = Playlist::new();

        // Collect CLI file/dir args for background loading.
        let cli_paths: Vec<PathBuf> = std::env::args()
            .skip(1)
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .collect();

        let songlength_db = config
            .last_songlength_file
            .as_ref()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .and_then(|p| {
                eprintln!(
                    "[phosphor] Loading remembered Songlengths at {}",
                    p.display()
                );
                SonglengthDb::load(&p).ok()
            })
            .or_else(|| {
                config::songlength_db_path()
                    .filter(|p| p.exists())
                    .and_then(|p| {
                        eprintln!("[phosphor] Found Songlengths.md5 at {}", p.display());
                        SonglengthDb::load(&p).ok()
                    })
            })
            .or_else(|| SonglengthDb::auto_load());

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
        let auto_songlength_url = config.songlength_url.clone();
        let auto_stil_url = config.stil_url.clone();
        let auto_last_sl_file = config.last_songlength_file.clone();
        let auto_last_stil_file = config.last_stil_file.clone();

        let app = Self {
            cmd_tx,
            status_rx,
            status: PlayerStatus {
                state: PlayState::Stopped,
                track_info: None,
                elapsed: Duration::ZERO,
                voice_levels: vec![],
                writes_per_frame: 0,
                error: None,
                sid_regs: vec![0u8; 128],
                flag_count: 0,
            },
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
            default_length_text,
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
            show_recently_played: false,
            show_sid_panel: false,
            playlist_scroll_offset_y: 0.0,
            // Use the saved window height as a reasonable first-frame estimate;
            // the real value arrives with the first PlaylistScrolled event.
            playlist_viewport_height: window_height,
            playlist_viewport_y: 0.0,
            pixel_ratio: 1.0,
            context_menu: None,
            silence_frames: 0,
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

        // Kick off HVSC version check using the STIL URL.
        // We only fetch the first 1 KB of the remote STIL so this is cheap.
        let hvsc_check_url = auto_stil_url.clone();
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

        let mut tasks = vec![version_task, hvsc_task, session_task];
        let mut auto_status_parts: Vec<&str> = vec![];

        if songlength_missing {
            eprintln!("[phosphor] Songlength DB not found — auto-downloading");
            let url = auto_songlength_url.clone();
            tasks.push(Task::perform(
                config::download_songlength(url),
                Message::SonglengthDownloaded,
            ));
            auto_status_parts.push("Songlengths");
        }

        if stil_missing {
            eprintln!("[phosphor] STIL.txt not found — auto-downloading");
            let url = auto_stil_url.clone();
            tasks.push(Task::perform(
                stil::download_stil(url),
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

        (app, Task::batch(tasks))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
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
                //   playlist_viewport_y + display_pos*ROW_HEIGHT - scroll_offset_y + ROW_HEIGHT/2
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
                let logical_row_y = self.playlist_viewport_y + display_pos * ui::ROW_HEIGHT
                    - self.playlist_scroll_offset_y
                    + ui::ROW_HEIGHT * 0.5;

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
                let menu_y = (self.playlist_viewport_y + display_pos * ui::ROW_HEIGHT
                    - self.playlist_scroll_offset_y
                    + ui::ROW_HEIGHT)
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
                }
            }

            Message::Stop => {
                self.context_menu = None;
                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                self.visualizer.reset();
                self.tracker_history.reset();
                self.tracker_view.reset();
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
                        self.update_entry_subtune(next);
                    }
                }
            }

            Message::PrevSubtune => {
                if let Some(ref info) = self.status.track_info {
                    let prev = info.current_song.saturating_sub(1).max(1);
                    if prev != info.current_song {
                        let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(prev));
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
                                db.apply_to_playlist(&mut self.playlist);
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
                        db.apply_to_playlist(&mut self.playlist);
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
                self.playlist_scroll_offset_y = next as f32 * ui::ROW_HEIGHT;
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
                self.playlist_scroll_offset_y = prev as f32 * ui::ROW_HEIGHT;
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
                }
            }

            Message::ToggleSkipRsid => {
                self.config.skip_rsid = !self.config.skip_rsid;
                self.config.save();
            }
            Message::ToggleForceStereo2sid => {
                self.config.force_stereo_2sid = !self.config.force_stereo_2sid;
                self.config.save();
            }

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

            Message::SonglengthUrlChanged(url) => {
                self.config.songlength_url = url;
                self.config.save();
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
                let url = self.config.songlength_url.clone();
                return Task::perform(
                    config::download_songlength(url),
                    Message::SonglengthDownloaded,
                );
            }

            Message::SonglengthDownloaded(Ok(path)) => match SonglengthDb::load(&path) {
                Ok(db) => {
                    let count = db.entries.len();
                    db.apply_to_playlist(&mut self.playlist);
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
                    self.poll_remote_commands();
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
                    let sl_url = self.config.songlength_url.clone();
                    let stil_url = self.config.stil_url.clone();
                    return Task::batch([
                        Task::perform(
                            config::download_songlength(sl_url),
                            Message::SonglengthDownloaded,
                        ),
                        Task::perform(stil::download_stil(stil_url), Message::StilDownloaded),
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
                    iced::Size::new(ui::MINI_WIDTH, ui::MINI_HEIGHT)
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
                }
                self.context_menu = None;
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

            // ── STIL settings ─────────────────────────────────────────────
            Message::StilUrlChanged(url) => {
                self.config.stil_url = url;
                self.config.save();
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

            Message::DownloadStil => {
                self.stil_status = "Downloading…".to_string();
                let url = self.config.stil_url.clone();
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
        // ── Mini player mode ─────────────────────────────────────────────────
        if self.mini_mode {
            let current_duration = self.playlist.current_entry().and_then(|e| e.duration_secs);
            let is_fav = self
                .playlist
                .current_entry()
                .and_then(|e| e.md5.as_deref())
                .map(|m| self.favorites.is_favorite(m))
                .unwrap_or(false);
            return ui::mini_player_view(&self.status, current_duration, is_fav);
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
        let info_bar = ui::track_info_bar(
            &self.status,
            &self.visualizer,
            tracker_ref,
            is_now_playing_fav,
            self.status.track_info.is_some(),
            self.stil_entry.is_some() || !self.stil_display_text.is_empty(),
            self.window_width,
            &self.config.output_engine,
        );
        let controls = ui::controls_bar(
            &self.status,
            &self.playlist,
            self.new_version.as_ref(),
            self.window_width,
            self.show_recently_played,
            self.show_sid_panel,
        );
        let current_duration = self.playlist.current_entry().and_then(|e| e.duration_secs);
        let progress = ui::progress_bar(&self.status, current_duration);

        // Build the main content area
        let main_content: Element<'_, Message> = if self.show_settings {
            let settings = ui::settings_panel(
                &self.config,
                &self.default_length_text,
                &self.download_status,
                &self.stil_status,
                self.http_remote_running,
                &self.http_port_text,
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
                ui::status_bar(&self.heard_text),
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
                            let total_rows: usize =
                                groups.iter().map(|g| g.len()).sum();
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
                    let dur = entry.duration_secs;
                    // Advance if duration exceeded OR prolonged silence detected.
                    // Silence detection: ~90 frames ≈ 3 seconds at 30fps tick.
                    // Only trigger after at least 5 seconds of playback to avoid
                    // false positives during song intro.
                    let elapsed = self.status.elapsed.as_secs();
                    let silence_ended = self.silence_frames > 90 && elapsed > 5;
                    let duration_ended = dur.map_or(false, |d| elapsed >= d as u64);

                    if duration_ended || silence_ended {
                        if silence_ended && dur.is_none() {
                            eprintln!("[phosphor] Silence detected after {}s — advancing", elapsed);
                        }
                        Some((entry.selected_song, entry.songs, entry.md5.clone()))
                    } else {
                        None
                    }
                });

                if let Some((cur_song, total_songs, md5)) = advance_info {
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
            };

            // Only rebuild playlist snapshot when entries change.
            let version = self.playlist.len() as u64;
            if rs.playlist_version != version {
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
                    })
                    .collect();
                rs.playlist_version = version;
            }
        }
    }

    /// Process commands from the HTTP remote control server.
    fn poll_remote_commands(&mut self) {
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
                }
            }
        }
    }

    fn apply_songlengths(&mut self) {
        if let Some(ref db) = self.songlength_db {
            db.apply_to_playlist(&mut self.playlist);
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
        if self.session_loaded {
            self.playlist.save_session();
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

    let config_for_window = Config::load();

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
        .window(iced::window::Settings {
            icon: Some(icon),
            position: match (config_for_window.window_x, config_for_window.window_y) {
                (Some(x), Some(y)) => {
                    iced::window::Position::Specific(iced::Point::new(x as f32, y as f32))
                }
                _ => iced::window::Position::Default,
            },
            ..Default::default()
        })
        .run()
}

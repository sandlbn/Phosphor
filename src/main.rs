#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
mod c64_emu;
mod config;
mod player;
mod playlist;
mod recently_played;
mod sid_device;
mod ui;
mod version_check;

#[cfg(all(feature = "usb", target_os = "macos"))]
mod usb_bridge;

#[cfg(all(feature = "usb", target_os = "macos"))]
mod daemon_installer;

#[cfg(all(feature = "usb", not(target_os = "macos")))]
mod sid_direct;

mod sid_emulated;
mod sid_u64;

use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use iced::widget::{column, container, rule};
use iced::{event, time, Color, Element, Length, Subscription, Task, Theme};

use config::{Config, FavoritesDb};
use player::{PlayState, PlayerCmd, PlayerStatus};
use playlist::{Playlist, SonglengthDb};
use recently_played::RecentlyPlayed;
use ui::visualizer::Visualizer;
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

    /// Some(_) when the right-click context menu is visible.
    context_menu: Option<ContextMenu>,
    /// Whether the visualiser is expanded to fill the whole window (overlay mode).
    /// Double-clicking the visualiser canvas toggles this.
    vis_expanded: bool,
    /// Cached metadata for the concert-screen overlay, kept in sync with track_info
    /// on every tick so the borrow checker is happy in `view()`.
    vis_expanded_info: Option<ui::visualizer::ExpandedInfo>,
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

        let mut playlist = Playlist::new();

        let args: Vec<String> = std::env::args().collect();
        for arg in args.iter().skip(1) {
            if arg.starts_with("--") {
                continue;
            }
            let path = PathBuf::from(arg);
            if path.is_dir() {
                playlist.add_directory(&path);
            } else {
                let ext = path
                    .extension()
                    .map(|e| e.to_ascii_lowercase().to_string_lossy().to_string())
                    .unwrap_or_default();
                match ext.as_str() {
                    "sid" => {
                        let _ = playlist.add_file(&path);
                    }
                    "m3u" | "m3u8" | "pls" => match playlist.load_playlist_file(&path) {
                        Ok(n) => eprintln!("[phosphor] Loaded {n} tracks from {}", path.display()),
                        Err(e) => eprintln!("[phosphor] Failed to load playlist: {e}"),
                    },
                    _ => {
                        let _ = playlist.add_file(&path);
                    }
                }
            }
        }

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

        if let Some(ref db) = songlength_db {
            db.apply_to_playlist(&mut playlist);
        }
        if config.default_song_length_secs > 0 {
            apply_default_length(&mut playlist, config.default_song_length_secs);
        }

        let filtered_indices: Vec<usize> = (0..playlist.len()).collect();
        let default_length_text = if config.default_song_length_secs > 0 {
            config.default_song_length_secs.to_string()
        } else {
            String::new()
        };

        let favorites = FavoritesDb::load();
        let recently_played = RecentlyPlayed::load();
        let window_width = config.window_width_saved;
        let window_height = config.window_height_saved;

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
            loading_progress: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            pending_entries: None,
            scroll_to_current: false,
            new_version: None,
            favorites,
            favorites_only: false,
            window_width,
            window_height,
            recently_played,
            show_recently_played: false,
            show_sid_panel: false,
            playlist_scroll_offset_y: 0.0,
            // Use the saved window height as a reasonable first-frame estimate;
            // the real value arrives with the first PlaylistScrolled event.
            playlist_viewport_height: window_height,
            context_menu: None,
            vis_expanded: false,
            vis_expanded_info: None,
        };

        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let version_task = Task::perform(
            async move { version_check::check_github_release(&current_version).await },
            Message::VersionCheckDone,
        );

        (app, version_task)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // ── Any interaction dismisses the context menu ────────────────
            // (handled per-message below where needed; explicit dismiss too)
            Message::DismissContextMenu => {
                self.context_menu = None;
            }

            // ── Context menu actions ──────────────────────────────────────
            Message::ShowContextMenu(idx, x, y) => {
                self.context_menu = Some(ContextMenu {
                    track_idx: idx,
                    x,
                    y,
                });
                // Also select the row so keyboard actions target the same track
                self.selected = Some(idx);
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
                    }
                }
            }

            Message::PrevSubtune => {
                if let Some(ref info) = self.status.track_info {
                    let prev = info.current_song.saturating_sub(1).max(1);
                    if prev != info.current_song {
                        let _ = self.cmd_tx.send(PlayerCmd::SetSubtune(prev));
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
                    let n = entries.len();
                    self.pending_entries = Some(entries);
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

            Message::FileDropped(path) => {
                self.context_menu = None;
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                match ext.as_str() {
                    "sid" => {
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
                    let _ = self.cmd_tx.try_send(PlayerCmd::SetEngine(
                        engine,
                        self.config.u64_address.clone(),
                        self.config.u64_password.clone(),
                    ));
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
                    self.download_status = format!(
                        "Download success! Loaded {} entries from {}",
                        count,
                        path.display()
                    );
                }
                Err(e) => self.download_status = format!("Error loading DB: {e}"),
            },
            Message::SonglengthDownloaded(Err(e)) => {
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
                self.poll_status();
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
            Message::WindowResized(w, h) => {
                self.window_width = w;
                self.window_height = h;
                self.config.window_width_saved = w;
                self.config.window_height_saved = h;
                self.config.save();
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
            }

            Message::None => {}
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let is_now_playing_fav = self
            .playlist
            .current_entry()
            .and_then(|e| e.md5.as_ref())
            .map(|m| self.favorites.is_favorite(m))
            .unwrap_or(false);

        let info_bar = ui::track_info_bar(
            &self.status,
            &self.visualizer,
            is_now_playing_fav,
            self.status.track_info.is_some(),
            self.window_width,
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
            let sid_view = ui::sid_panel::sid_panel(&self.status.sid_regs, num_sids, is_pal);
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
            let loading_status = self
                .loading_progress
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default();
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
            );
            column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                search,
                rule::horizontal(1),
                playlist_widget
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

        // If context menu is open, layer it over the base using stack
        if let Some(ref cm) = self.context_menu {
            let overlay = ui::context_menu_overlay(
                cm.x,
                cm.y,
                cm.track_idx,
                &self.playlist,
                &self.favorites,
                self.window_width,
                self.window_height,
            );
            iced::widget::stack![base, overlay]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else if self.vis_expanded {
            // Full-window concert-screen overlay — double-click again to collapse.
            let vis_overlay = container(
                self.visualizer
                    .view_expanded(self.vis_expanded_info.as_ref()),
            )
            .width(Length::Fill)
            .height(Length::Fill);
            iced::widget::stack![base, vis_overlay]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            base.into()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = time::every(Duration::from_millis(33)).map(|_| Message::Tick);

        let window_events = event::listen_with(|event, status, _id| match event {
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size.width, size.height))
            }
            iced::Event::Window(iced::window::Event::Moved(point)) => {
                Some(Message::WindowMoved(point.x as i32, point.y as i32))
            }
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                use iced::event::Status;
                use iced::keyboard::key::Named;
                use iced::keyboard::Key;
                match key {
                    // Space only fires when no widget has captured the event —
                    // prevents stopping playback while typing in the search box.
                    Key::Named(Named::Space) if status != Status::Captured => {
                        Some(Message::PlayPause)
                    }
                    Key::Named(Named::ArrowLeft) => Some(Message::PrevTrack),
                    Key::Named(Named::ArrowRight) => Some(Message::NextTrack),
                    Key::Named(Named::ArrowUp) => Some(Message::SelectPrev),
                    Key::Named(Named::ArrowDown) => Some(Message::SelectNext),
                    Key::Named(Named::Delete) => Some(Message::RemoveSelected),
                    Key::Named(Named::Escape) => Some(Message::DismissContextMenu),
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
            }

            self.playlist.current = Some(idx);
            self.selected = Some(idx);
            self.scroll_to_current = true;

            let force_stereo =
                self.config.force_stereo_2sid || std::env::args().any(|a| a == "--stereo");
            let sid4_addr = parse_sid4_from_args();

            let _ = self.cmd_tx.send(PlayerCmd::Play {
                path: entry.path.clone(),
                song: entry.selected_song,
                force_stereo,
                sid4_addr,
            });
        }
    }

    fn poll_status(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            self.status = status;
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
                num_sids: info.num_sids,
                elapsed_secs: self.status.elapsed.as_secs_f32(),
                duration_secs: current_duration,
            });
        } else {
            self.vis_expanded_info = None;
        }
        self.visualizer.update(&self.status.voice_levels);

        if self.status.state == PlayState::Playing {
            if let Some(cur_idx) = self.playlist.current {
                let advance_info = self.playlist.entries.get(cur_idx).and_then(|entry| {
                    let dur = entry.duration_secs?;
                    if self.status.elapsed.as_secs() >= dur as u64 {
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
        .add_filter("SID files", &["sid", "SID"]);
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

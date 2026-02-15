#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
mod c64_emu;
mod config;
mod player;
mod playlist;
mod sid_device;
mod ui;

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
use ui::visualizer::Visualizer;
use ui::Message;

// ─────────────────────────────────────────────────────────────────────────────
//  Application state
// ─────────────────────────────────────────────────────────────────────────────

struct App {
    /// Channel to send commands to the player thread.
    cmd_tx: Sender<PlayerCmd>,
    /// Channel to receive status from the player thread.
    status_rx: Receiver<PlayerStatus>,
    /// Last known player status.
    status: PlayerStatus,

    /// Playlist model.
    playlist: Playlist,
    /// Selected row in playlist (not necessarily playing).
    selected: Option<usize>,
    /// Visualiser state.
    visualizer: Visualizer,
    /// Songlength database (loaded on demand).
    songlength_db: Option<SonglengthDb>,

    /// Current search / filter query.
    search_text: String,
    /// Indices into playlist.entries that match the current search.
    filtered_indices: Vec<usize>,

    /// Persistent configuration.
    config: Config,
    /// Whether the settings panel is visible.
    show_settings: bool,
    /// Text in the default song length input field.
    default_length_text: String,
    /// Status message for songlength download.
    download_status: String,

    /// Favorites database (MD5 hashes).
    favorites: FavoritesDb,
    /// Whether to show only favorite tunes.
    favorites_only: bool,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let config = Config::load();
        eprintln!(
            "[phosphor] Config: skip_rsid={}, default_length={}s, engine={}",
            config.skip_rsid, config.default_song_length_secs, config.output_engine,
        );

        // macOS: if the daemon plist points to a stale binary (e.g. app was
        // moved or updated), proactively reinstall so the user doesn't hit
        // a confusing error later when they try to play a tune.
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

        // Load files from CLI args
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
                    } // try anyway
                }
            }
        }

        // Auto-load Songlength.md5 — try remembered path, then config dir, then auto-detect
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

        // Apply default song length for entries that still have no duration
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
            },
            playlist,
            selected: None,
            visualizer: Visualizer::new(),
            songlength_db,
            search_text: String::new(),
            filtered_indices,
            config,
            show_settings: false,
            default_length_text,
            download_status: String::new(),
            favorites,
            favorites_only: false,
        };

        (app, Task::none())
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // ── Transport ────────────────────────────────────────────────
            Message::PlayPause => {
                if self.status.state == PlayState::Stopped {
                    // Start playing selected or first track
                    let idx = self.selected.or(Some(0));
                    if let Some(i) = idx {
                        self.play_track(i);
                    }
                } else {
                    let _ = self.cmd_tx.send(PlayerCmd::TogglePause);
                }
            }

            Message::Stop => {
                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                self.visualizer.reset();
            }

            Message::NextTrack => {
                if let Some(idx) = self.playlist.next() {
                    self.play_track(idx);
                }
            }

            Message::PrevTrack => {
                // If more than 3 seconds in, restart. Otherwise prev track.
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
                if self.selected == Some(idx) {
                    // Double-click behaviour: play the selected track
                    self.play_track(idx);
                } else {
                    self.selected = Some(idx);
                }
            }

            Message::PlaylistDoubleClick(idx) => {
                self.play_track(idx);
            }

            Message::AddFiles => {
                let start_dir = self.config.last_sid_dir.clone();
                return Task::perform(pick_files(start_dir), Message::FilesChosen);
            }

            Message::AddFolder => {
                let start_dir = self.config.last_sid_dir.clone();
                return Task::perform(pick_folder(start_dir), Message::FolderChosen);
            }

            Message::ClearPlaylist => {
                let _ = self.cmd_tx.send(PlayerCmd::Stop);
                self.playlist.clear();
                self.selected = None;
                self.visualizer.reset();
                self.rebuild_filter();
            }

            Message::RemoveSelected => {
                if let Some(idx) = self.selected {
                    // If removing currently playing track, stop
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
                self.playlist.toggle_shuffle();
            }

            Message::CycleRepeat => {
                self.playlist.cycle_repeat();
            }

            // ── Songlength ───────────────────────────────────────────────
            Message::LoadSonglength => {
                let start_dir = self.config.last_songlength_dir.clone();
                return Task::perform(
                    pick_songlength_file(start_dir),
                    Message::SonglengthFileChosen,
                );
            }

            // ── Playlist save / load ─────────────────────────────────────
            Message::SavePlaylist => {
                if self.playlist.is_empty() {
                    return Task::none();
                }
                let entries: Vec<(std::path::PathBuf, String, String, Option<u32>)> = self
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
                let start_dir = self.config.last_playlist_dir.clone();
                return Task::perform(pick_playlist_file(start_dir), Message::PlaylistFileChosen);
            }

            // ── Async results ────────────────────────────────────────────
            Message::FilesChosen(paths) => {
                if paths.is_empty() {
                    return Task::none();
                }
                // Remember the directory for next time.
                if let Some(first) = paths.first() {
                    self.config.remember_sid_dir(first);
                }
                // Parse SID headers off the UI thread
                return Task::perform(
                    async move { playlist::parse_files(paths) },
                    Message::FilesLoaded,
                );
            }

            Message::FolderChosen(Some(path)) => {
                self.config.remember_sid_dir(&path);
                // Walk + parse off the UI thread
                return Task::perform(
                    async move { playlist::parse_directory(path) },
                    Message::FolderLoaded,
                );
            }
            Message::FolderChosen(None) => {}

            Message::FilesLoaded(entries) => {
                if !entries.is_empty() {
                    self.playlist.add_entries(entries);
                    self.apply_songlengths();
                    self.rebuild_filter();
                }
            }

            Message::FolderLoaded(entries) => {
                if !entries.is_empty() {
                    self.playlist.add_entries(entries);
                    self.apply_songlengths();
                    self.rebuild_filter();
                }
            }

            // ── Drag & drop ─────────────────────────────────────────
            Message::FileDropped(path) => {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                match ext.as_str() {
                    // SID file → add to playlist
                    "sid" => {
                        self.config.remember_sid_dir(&path);
                        let paths = vec![path];
                        return Task::perform(
                            async move { playlist::parse_files(paths) },
                            Message::FilesLoaded,
                        );
                    }
                    // Songlength database
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
                                eprintln!(
                                    "[phosphor] Songlength DB loaded via drop: {count} entries"
                                );
                            }
                            Err(e) => {
                                eprintln!("[phosphor] Dropped file failed to load: {e}");
                                // Not a songlength file — might be a playlist
                            }
                        }
                    }
                    // Playlist files
                    "m3u" | "m3u8" | "pls" => {
                        self.config.remember_playlist_dir(&path);
                        return Task::perform(
                            async move { playlist::parse_playlist_file(path) },
                            Message::PlaylistLoaded,
                        );
                    }
                    _ => {
                        // Try as a directory (folder drop)
                        if path.is_dir() {
                            self.config.remember_sid_dir(&path);
                            let dir = path;
                            return Task::perform(
                                async move { playlist::parse_directory(dir) },
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
                        log::info!(
                            "Songlength DB loaded: {} entries",
                            self.songlength_db.as_ref().unwrap().entries.len()
                        );
                    }
                    Err(e) => log::error!("Failed to load Songlength DB: {e}"),
                }
            }
            Message::SonglengthFileChosen(None) => {}

            Message::PlaylistSaved(Ok(path)) => {
                self.config.remember_playlist_dir(&path);
                eprintln!("[phosphor] Playlist saved to {}", path.display());
            }
            Message::PlaylistSaved(Err(e)) => {
                eprintln!("[phosphor] Save failed: {e}");
            }

            Message::PlaylistFileChosen(Some(path)) => {
                self.config.remember_playlist_dir(&path);
                // Parse playlist + SID headers off the UI thread
                return Task::perform(
                    async move { playlist::parse_playlist_file(path) },
                    Message::PlaylistLoaded,
                );
            }
            Message::PlaylistFileChosen(None) => {}

            Message::PlaylistLoaded(Ok(entries)) => {
                if !entries.is_empty() {
                    eprintln!("[phosphor] Loaded {} tracks from playlist", entries.len());
                    self.playlist.add_entries(entries);
                    self.apply_songlengths();
                    self.rebuild_filter();
                }
            }
            Message::PlaylistLoaded(Err(e)) => {
                eprintln!("[phosphor] Failed to load playlist: {e}");
            }

            // ── Search / filter ───────────────────────────────────────
            Message::SearchChanged(query) => {
                self.search_text = query;
                self.filtered_indices = ui::filter_playlist(
                    &self.playlist,
                    &self.search_text,
                    self.favorites_only,
                    &self.favorites,
                );
            }

            Message::ClearSearch => {
                self.search_text.clear();
                self.rebuild_filter();
            }

            // ── Settings ─────────────────────────────────────────────────
            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
            }

            Message::ToggleSkipRsid => {
                self.config.skip_rsid = !self.config.skip_rsid;
                self.config.save();
            }

            Message::DefaultSongLengthChanged(val) => {
                self.default_length_text = val.clone();
                // Parse and apply the value
                let new_val = val.trim().parse::<u32>().unwrap_or(0);
                if new_val != self.config.default_song_length_secs {
                    self.config.default_song_length_secs = new_val;
                    self.config.save();
                    // Re-apply default lengths to playlist
                    if new_val > 0 {
                        apply_default_length(&mut self.playlist, new_val);
                    } else {
                        // Remove default lengths (re-apply only songlength DB)
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
                    eprintln!("[phosphor] Output engine → '{engine}'");
                    self.config.output_engine = engine.clone();
                    self.config.save();
                    // Tell the player thread to switch engines (include U64 config).
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
                // Update player thread config without stopping playback.
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
                        path.display(),
                    );
                    eprintln!("[phosphor] Songlength DB refreshed: {count} entries");
                }
                Err(e) => {
                    self.download_status = format!("Error loading DB: {e}");
                }
            },

            Message::SonglengthDownloaded(Err(e)) => {
                self.download_status = format!("Error: {e}");
                eprintln!("[phosphor] Songlength download failed: {e}");
            }

            // ── Favorites ────────────────────────────────────────────────
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
                            md5,
                        );
                        // Rebuild filter in case favorites_only is active
                        if self.favorites_only {
                            self.rebuild_filter();
                        }
                    }
                }
            }

            Message::ToggleFavoritesFilter => {
                self.favorites_only = !self.favorites_only;
                self.rebuild_filter();
            }

            // ── Tick ─────────────────────────────────────────────────────
            Message::Tick => {
                self.poll_status();
            }

            Message::None => {}
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let info_bar = ui::track_info_bar(&self.status, &self.visualizer);
        let controls = ui::controls_bar(&self.status, &self.playlist);

        // Progress bar: get current track duration
        let current_duration = self.playlist.current_entry().and_then(|e| e.duration_secs);
        let progress = ui::progress_bar(&self.status, current_duration);

        if self.show_settings {
            // Settings view: replace search + playlist with settings panel
            let settings = ui::settings_panel(
                &self.config,
                &self.default_length_text,
                &self.download_status,
            );

            let content = column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                settings,
            ];

            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
                    ..Default::default()
                })
                .into()
        } else {
            // Normal view: search + playlist
            let search = ui::search_bar(
                &self.search_text,
                self.filtered_indices.len(),
                self.playlist.len(),
                self.favorites_only,
                self.favorites.count(),
            );

            let playlist = ui::playlist_view(
                &self.playlist,
                self.selected,
                &self.filtered_indices,
                &self.favorites,
            );

            let content = column![
                info_bar,
                progress,
                rule::horizontal(1),
                controls,
                rule::horizontal(1),
                search,
                rule::horizontal(1),
                playlist,
            ];

            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
                    ..Default::default()
                })
                .into()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        // Tick at ~30 Hz for smooth visualisation + status polling.
        let tick = time::every(Duration::from_millis(33)).map(|_| Message::Tick);

        // Listen for file-drop events from the OS.
        let file_drop = event::listen_with(|event, _status, _id| {
            if let iced::Event::Window(iced::window::Event::FileDropped(path)) = event {
                Some(Message::FileDropped(path))
            } else {
                None
            }
        });

        Subscription::batch([tick, file_drop])
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn play_track(&mut self, idx: usize) {
        if let Some(entry) = self.playlist.entries.get(idx) {
            // Skip RSID tunes if configured
            if self.config.skip_rsid && entry.is_rsid {
                eprintln!("[phosphor] Skipping RSID tune: \"{}\"", entry.title,);
                self.playlist.current = Some(idx);
                // Try next track (avoid infinite loop by tracking visited)
                if let Some(next_idx) = self.playlist.next() {
                    if next_idx != idx {
                        self.play_track(next_idx);
                    } else {
                        // Only RSID tunes left, stop
                        let _ = self.cmd_tx.send(PlayerCmd::Stop);
                    }
                } else {
                    let _ = self.cmd_tx.send(PlayerCmd::Stop);
                }
                return;
            }

            self.playlist.current = Some(idx);
            self.selected = Some(idx);

            let force_stereo = std::env::args().any(|a| a == "--stereo");
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
        // Drain all pending status messages, keep latest
        while let Ok(status) = self.status_rx.try_recv() {
            self.status = status;
        }

        // Update visualiser
        self.visualizer.update(&self.status.voice_levels);

        // Auto-advance: SID tunes loop forever, so we must check
        // elapsed time against the Songlength duration while playing
        // and force-advance to the next track or sub-tune.
        if self.status.state == PlayState::Playing {
            if let Some(cur_idx) = self.playlist.current {
                // Extract what we need from the entry before mutating
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
                        // Advance to next sub-tune
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
                                // Use default length if no DB entry
                                let def = self.config.default_song_length_secs;
                                if def > 0 {
                                    Some(def)
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
                        // All sub-tunes played — reset to first subtune
                        let first_dur = md5
                            .as_ref()
                            .and_then(|m| {
                                self.songlength_db.as_ref().and_then(|db| db.lookup(m, 0))
                            })
                            .or_else(|| {
                                let def = self.config.default_song_length_secs;
                                if def > 0 {
                                    Some(def)
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
        // Also apply default length for any remaining entries without duration
        if self.config.default_song_length_secs > 0 {
            apply_default_length(&mut self.playlist, self.config.default_song_length_secs);
        }
    }

    fn rebuild_filter(&mut self) {
        self.filtered_indices = ui::filter_playlist(
            &self.playlist,
            &self.search_text,
            self.favorites_only,
            &self.favorites,
        );
    }
}

/// Apply a default song length to all playlist entries that have no duration.
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

/// Clear durations that were set by default (reset entries with no DB match).
fn clear_default_lengths(playlist: &mut Playlist) {
    for entry in &mut playlist.entries {
        // We can't distinguish DB-set from default-set, so clear all
        // and let the caller re-apply songlength DB afterwards.
        entry.duration_secs = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Cleanup on exit
// ─────────────────────────────────────────────────────────────────────────────

impl Drop for App {
    fn drop(&mut self) {
        eprintln!("[phosphor] App closing, stopping playback...");
        let _ = self.cmd_tx.send(PlayerCmd::Stop);
        // Give the player thread time to mute + reset the hardware
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = self.cmd_tx.send(PlayerCmd::Quit);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Async file dialogs (using rfd via iced Task)
// ─────────────────────────────────────────────────────────────────────────────

async fn pick_files(start_dir: Option<String>) -> Vec<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Add SID files")
        .add_filter("SID files", &["sid", "SID"]);

    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            dialog = dialog.set_directory(&p);
        }
    }

    let result = dialog.pick_files().await;

    result
        .map(|handles| handles.iter().map(|h| h.path().to_path_buf()).collect())
        .unwrap_or_default()
}

async fn pick_folder(start_dir: Option<String>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Add folder of SID files");

    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            dialog = dialog.set_directory(&p);
        }
    }

    dialog.pick_folder().await.map(|h| h.path().to_path_buf())
}

async fn pick_songlength_file(start_dir: Option<String>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Load HVSC Songlength.md5")
        .add_filter("Songlength", &["md5", "txt"]);

    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            dialog = dialog.set_directory(&p);
        }
    }

    dialog.pick_file().await.map(|h| h.path().to_path_buf())
}

async fn pick_playlist_file(start_dir: Option<String>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Open Playlist")
        .add_filter("Playlists", &["m3u", "m3u8", "pls"])
        .add_filter("All files", &["*"]);

    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            dialog = dialog.set_directory(&p);
        }
    }

    dialog.pick_file().await.map(|h| h.path().to_path_buf())
}

/// Show save dialog, then write M3U. The entries are passed in so
/// we don't need to Send the full Playlist across the async boundary.
async fn save_playlist_dialog(
    entries: Vec<(PathBuf, String, String, Option<u32>)>,
    start_dir: Option<String>,
) -> Result<PathBuf, String> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Save Playlist")
        .add_filter("M3U Playlist", &["m3u"])
        .set_file_name("playlist.m3u");

    if let Some(ref dir) = start_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            dialog = dialog.set_directory(&p);
        }
    }

    let handle = dialog.save_file().await;

    match handle {
        Some(h) => {
            let path = h.path().to_path_buf();
            write_m3u(&path, &entries)?;
            Ok(path)
        }
        None => Err("Cancelled".into()),
    }
}

/// Write entries as extended M3U (called from async context).
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
//  CLI argument helpers
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
    let hex = if let Some(h) = s.strip_prefix("$") {
        h
    } else if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        h
    } else {
        s
    };
    u16::from_str_radix(hex, 16).ok()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> iced::Result {
    env_logger::init();

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
        .window_size((900.0, 600.0))
        .window(iced::window::Settings {
            icon: Some(icon),
            ..Default::default()
        })
        .run()
}

pub mod visualizer;

use std::path::PathBuf;
use std::time::Duration;

use iced::widget::{
    button, column, container, row, rule, scrollable, text, text_input, Column, Space,
};
use iced::{Alignment, Color, Element, Length, Padding, Theme};

use crate::config::{Config, FavoritesDb};
use crate::player::{PlayState, PlayerStatus};
use crate::playlist::Playlist;
use visualizer::Visualizer;

// ─────────────────────────────────────────────────────────────────────────────
//  Messages
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    // Transport
    PlayPause,
    Stop,
    NextTrack,
    PrevTrack,

    // Playlist
    PlaylistSelect(usize),
    PlaylistDoubleClick(usize),
    AddFiles,
    AddFolder,
    ClearPlaylist,
    RemoveSelected,

    // Modes
    ToggleShuffle,
    CycleRepeat,

    // Sub-tunes
    NextSubtune,
    PrevSubtune,

    // Songlength
    LoadSonglength,

    // Playlist save / load
    SavePlaylist,
    LoadPlaylist,

    // Search / filter
    SearchChanged(String),
    ClearSearch,

    // Player status tick
    Tick,

    // File dialog results
    FilesChosen(Vec<PathBuf>),
    FolderChosen(Option<PathBuf>),
    SonglengthFileChosen(Option<PathBuf>),
    PlaylistSaved(Result<PathBuf, String>),
    PlaylistFileChosen(Option<PathBuf>),

    // Background loading results (parsed off the UI thread)
    FilesLoaded(Vec<crate::playlist::PlaylistEntry>),
    FolderLoaded(Vec<crate::playlist::PlaylistEntry>),
    PlaylistLoaded(Result<Vec<crate::playlist::PlaylistEntry>, String>),

    // Settings
    ToggleSettings,
    ToggleSkipRsid,
    DefaultSongLengthChanged(String),
    SonglengthUrlChanged(String),
    DownloadSonglength,
    SonglengthDownloaded(Result<PathBuf, String>),
    SetOutputEngine(String),

    // Favorites
    ToggleFavorite(usize),
    ToggleFavoritesFilter,

    // File drag & drop
    FileDropped(PathBuf),

    // No-op
    None,
}

// ─────────────────────────────────────────────────────────────────────────────
//  View builders
// ─────────────────────────────────────────────────────────────────────────────

/// Build the track info + visualiser panel (top section).
pub fn track_info_bar<'a>(
    status: &'a PlayerStatus,
    visualizer: &'a Visualizer,
) -> Element<'a, Message> {
    let (title, author, extra) = match &status.track_info {
        Some(info) => (
            info.name.as_str(),
            info.author.as_str(),
            format!(
                "{}  •  {}  •  Song {}/{}  •  {}  •  {} writes/frame",
                if info.is_rsid { "RSID" } else { "PSID" },
                info.sid_type,
                info.current_song,
                info.songs,
                if info.is_pal { "PAL" } else { "NTSC" },
                status.writes_per_frame,
            ),
        ),
        None => ("No track loaded", "—", String::new()),
    };

    let state_icon = match status.state {
        PlayState::Playing => "▶",
        PlayState::Paused => "❚❚",
        PlayState::Stopped => "■",
    };

    let mut info_col = column![
        text(format!("{state_icon}  {title}")).size(18),
        text(author).size(14).color(Color::from_rgb(0.6, 0.7, 0.8)),
        text(extra).size(12).color(Color::from_rgb(0.5, 0.5, 0.6)),
    ]
    .spacing(2)
    .width(Length::Fill);

    // Show error message in red if present
    if let Some(ref err) = status.error {
        info_col = info_col.push(
            text(format!("⚠ {err}"))
                .size(12)
                .color(Color::from_rgb(1.0, 0.3, 0.3)),
        );
    }

    let vis = visualizer.view();

    let content = row![
        info_col,
        container(vis)
            .width(Length::Fixed(300.0))
            .height(Length::Fixed(60.0)),
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    container(content)
        .padding(Padding::from([10, 16]))
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.10, 0.11, 0.14))),
            ..Default::default()
        })
        .into()
}

/// Build the progress bar showing elapsed / total time.
pub fn progress_bar<'a>(
    status: &PlayerStatus,
    current_duration: Option<u32>,
) -> Element<'a, Message> {
    let elapsed_secs = status.elapsed.as_secs();
    let total_secs = current_duration.unwrap_or(0) as u64;

    let fraction = if total_secs > 0 {
        (elapsed_secs as f32 / total_secs as f32).min(1.0)
    } else {
        0.0
    };

    let elapsed_str = format_duration(status.elapsed);
    let total_str = if total_secs > 0 {
        format_duration(Duration::from_secs(total_secs))
    } else {
        "—:——".to_string()
    };

    let time_label = text(format!("  {elapsed_str} / {total_str}"))
        .size(11)
        .color(Color::from_rgb(0.6, 0.65, 0.7));

    // Build a two-layer progress bar using containers
    let bar_width_pct = (fraction * 100.0) as u16;

    let filled = container(Space::new().height(Length::Fixed(4.0)))
        .width(Length::FillPortion(bar_width_pct.max(1)))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.30, 0.70, 0.50))),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let remaining = container(Space::new().height(Length::Fixed(4.0)))
        .width(Length::FillPortion(
            100u16.saturating_sub(bar_width_pct).max(1),
        ))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.19, 0.22))),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let bar_row = row![filled, remaining].spacing(0).width(Length::Fill);

    let content = row![bar_row, time_label,]
        .spacing(8)
        .align_y(Alignment::Center);

    container(content)
        .padding(Padding::from([4, 16]))
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

/// Build the transport controls bar.
pub fn controls_bar<'a>(status: &PlayerStatus, playlist: &Playlist) -> Element<'a, Message> {
    let play_label = match status.state {
        PlayState::Playing => "❚❚",
        _ => "▶",
    };

    let transport = row![
        tool_button("◄◄", Message::PrevTrack),
        tool_button(play_label, Message::PlayPause),
        tool_button("■", Message::Stop),
        tool_button("►►", Message::NextTrack),
    ]
    .spacing(4);

    let subtune_controls = row![
        tool_button("◄ tune", Message::PrevSubtune),
        tool_button("tune ►", Message::NextSubtune),
    ]
    .spacing(4);

    let mode_controls = row![
        tool_button(
            if playlist.shuffle {
                "🔀 On"
            } else {
                "🔀 Off"
            },
            Message::ToggleShuffle,
        ),
        tool_button(playlist.repeat.label(), Message::CycleRepeat,),
    ]
    .spacing(4);

    let playlist_controls = row![
        tool_button("+ Files", Message::AddFiles),
        tool_button("+ Folder", Message::AddFolder),
        tool_button("📂 Open", Message::LoadPlaylist),
        tool_button("💾 Save", Message::SavePlaylist),
        tool_button("🗑 Clear", Message::ClearPlaylist),
        tool_button("⚙", Message::ToggleSettings),
    ]
    .spacing(4);

    let bar = row![
        transport,
        text(" │ ").color(Color::from_rgb(0.3, 0.3, 0.35)),
        subtune_controls,
        text(" │ ").color(Color::from_rgb(0.3, 0.3, 0.35)),
        mode_controls,
        Space::new().width(Length::Fill),
        playlist_controls,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding(Padding::from([6, 16]));

    container(bar)
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.12, 0.13, 0.16))),
            ..Default::default()
        })
        .into()
}

/// Build the search bar with filter input and track count.
pub fn search_bar<'a>(
    search_text: &str,
    visible_count: usize,
    total_count: usize,
    favorites_only: bool,
    favorites_count: usize,
) -> Element<'a, Message> {
    let search_input = text_input("Search playlist...", search_text)
        .on_input(Message::SearchChanged)
        .size(13)
        .padding(Padding::from([4, 8]))
        .width(Length::Fill)
        .style(|_theme: &Theme, _status| text_input::Style {
            background: iced::Background::Color(Color::from_rgb(0.14, 0.15, 0.18)),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            icon: Color::from_rgb(0.5, 0.5, 0.6),
            placeholder: Color::from_rgb(0.4, 0.4, 0.5),
            value: Color::from_rgb(0.85, 0.87, 0.9),
            selection: Color::from_rgba(0.3, 0.5, 0.8, 0.3),
        });

    let count_text = if favorites_only {
        format!("♥ {} / {} tracks", visible_count, total_count)
    } else if !search_text.is_empty() {
        format!("{} / {} tracks", visible_count, total_count)
    } else {
        format!("{} tracks", total_count)
    };

    let count_label = text(count_text)
        .size(12)
        .color(Color::from_rgb(0.5, 0.5, 0.6));

    let fav_label = if favorites_only {
        format!("♥ {}", favorites_count)
    } else {
        format!("♡ {}", favorites_count)
    };

    let fav_btn = button(text(fav_label).size(12))
        .on_press(Message::ToggleFavoritesFilter)
        .padding(Padding::from([4, 10]))
        .style(move |_theme: &Theme, status| {
            let bg = if favorites_only {
                match status {
                    button::Status::Hovered => Color::from_rgb(0.35, 0.18, 0.20),
                    button::Status::Pressed => Color::from_rgb(0.28, 0.14, 0.16),
                    _ => Color::from_rgb(0.30, 0.15, 0.18),
                }
            } else {
                match status {
                    button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                    button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                    _ => Color::from_rgb(0.18, 0.19, 0.22),
                }
            };
            let text_color = if favorites_only {
                Color::from_rgb(1.0, 0.4, 0.5)
            } else {
                Color::from_rgb(0.8, 0.82, 0.88)
            };
            button::Style {
                background: Some(iced::Background::Color(bg)),
                text_color,
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: if favorites_only {
                        Color::from_rgb(0.5, 0.2, 0.25)
                    } else {
                        Color::from_rgb(0.25, 0.27, 0.30)
                    },
                },
                ..Default::default()
            }
        });

    let mut search_row = row![
        text("🔍 ").size(13).color(Color::from_rgb(0.5, 0.5, 0.6)),
        search_input,
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    if !search_text.is_empty() {
        search_row = search_row.push(tool_button("✕", Message::ClearSearch));
    }

    let bar = row![
        search_row,
        Space::new().width(Length::Fixed(8.0)),
        fav_btn,
        Space::new().width(Length::Fixed(8.0)),
        count_label,
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .padding(Padding::from([4, 16]));

    container(bar)
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.11, 0.12, 0.14))),
            ..Default::default()
        })
        .into()
}

/// Build the playlist table.
/// `filtered_indices` maps visible row number → actual playlist index.
pub fn playlist_view<'a>(
    playlist: &Playlist,
    selected: Option<usize>,
    filtered_indices: &[usize],
    favorites: &FavoritesDb,
) -> Element<'a, Message> {
    // Column headers
    let header = playlist_row_view(
        "♥".into(),
        "#".into(),
        "Title".into(),
        "Author".into(),
        "Released".into(),
        "Time".into(),
        "Type".into(),
        "SIDs".into(),
        true,
        false,
        false,
        false,
    );

    let mut rows = Column::new()
        .spacing(0)
        .push(header)
        .push(rule::horizontal(1));

    if filtered_indices.is_empty() {
        let msg = if playlist.is_empty() {
            "Drag .sid files here or click \"+ Files\" / \"+ Folder\""
        } else {
            "No matching tracks"
        };
        rows = rows.push(
            container(text(msg).size(14).color(Color::from_rgb(0.4, 0.4, 0.5)))
                .padding(40)
                .center_x(Length::Fill),
        );
    } else {
        for &actual_idx in filtered_indices {
            if let Some(entry) = playlist.entries.get(actual_idx) {
                let is_current = playlist.current == Some(actual_idx);
                let is_selected = selected == Some(actual_idx);
                let is_fav = entry
                    .md5
                    .as_ref()
                    .map(|m| favorites.is_favorite(m))
                    .unwrap_or(false);
                let row_el = playlist_entry_row(actual_idx, entry, is_current, is_selected, is_fav);
                rows = rows.push(row_el);
            }
        }
    }

    scrollable(rows)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn playlist_entry_row<'a>(
    idx: usize,
    entry: &crate::playlist::PlaylistEntry,
    is_current: bool,
    is_selected: bool,
    is_favorite: bool,
) -> Element<'a, Message> {
    let sids_label = if entry.num_sids > 1 {
        format!("{}SID", entry.num_sids)
    } else {
        "1".to_string()
    };

    let type_label = if entry.is_rsid { "RSID" } else { "PSID" }.to_string();

    let song_title = if entry.songs > 1 {
        format!("{} [{}/{}]", entry.title, entry.selected_song, entry.songs)
    } else {
        entry.title.clone()
    };

    let bg = if is_selected {
        Some(iced::Background::Color(Color::from_rgba(
            0.3, 0.5, 0.8, 0.2,
        )))
    } else if is_current {
        Some(iced::Background::Color(Color::from_rgba(
            0.2, 0.6, 0.4, 0.1,
        )))
    } else {
        None
    };

    // Heart button (separate from row button so it's independently clickable)
    let heart_label = if is_favorite { "♥" } else { "♡" };
    let heart_color = if is_favorite {
        Color::from_rgb(1.0, 0.35, 0.45)
    } else {
        Color::from_rgb(0.35, 0.35, 0.40)
    };

    let heart_btn = button(text(heart_label).size(13).color(heart_color))
        .on_press(Message::ToggleFavorite(idx))
        .padding(Padding::from([4, 4]))
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Some(iced::Background::Color(Color::from_rgba(
                    1.0, 0.3, 0.4, 0.15,
                ))),
                _ => None,
            };
            button::Style {
                background: bg,
                text_color: Color::WHITE,
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        });

    let row_content = playlist_row_content(
        format!("{}", idx + 1),
        song_title,
        entry.author.clone(),
        entry.released.clone(),
        entry.format_duration(),
        type_label,
        sids_label,
        is_current,
    );

    // Row button (for selection/double-click)
    let row_btn = button(row_content)
        .on_press(Message::PlaylistSelect(idx))
        .padding(0)
        .style(|_theme: &Theme, _status| button::Style {
            background: None,
            text_color: Color::WHITE,
            ..Default::default()
        })
        .width(Length::Fill);

    let full_row = row![heart_btn, row_btn]
        .spacing(0)
        .align_y(Alignment::Center)
        .padding(Padding::from([0, 4]));

    container(full_row)
        .width(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: bg,
            ..Default::default()
        })
        .into()
}

/// Row content (without heart — used inside the row button).
fn playlist_row_content<'a>(
    num: String,
    title: String,
    author: String,
    released: String,
    time: String,
    sid_type: String,
    sids: String,
    is_current: bool,
) -> Element<'a, Message> {
    let size = 13;
    let color = if is_current {
        Color::from_rgb(0.35, 0.85, 0.55)
    } else {
        Color::from_rgb(0.78, 0.80, 0.84)
    };

    let type_color = if sid_type == "RSID" {
        Color::from_rgb(0.9, 0.65, 0.35)
    } else {
        Color::from_rgb(0.5, 0.75, 0.9)
    };

    let indicator = if is_current { "▶ " } else { "  " };

    row![
        text(format!("{indicator}{num:>3}"))
            .size(size)
            .color(color)
            .width(Length::Fixed(50.0)),
        text(title)
            .size(size)
            .color(color)
            .width(Length::FillPortion(4)),
        text(author)
            .size(size)
            .color(color)
            .width(Length::FillPortion(3)),
        text(released)
            .size(size)
            .color(color)
            .width(Length::FillPortion(2)),
        text(time)
            .size(size)
            .color(color)
            .width(Length::Fixed(55.0)),
        text(sid_type)
            .size(size)
            .color(type_color)
            .width(Length::Fixed(42.0)),
        text(sids)
            .size(size)
            .color(color)
            .width(Length::Fixed(45.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding(Padding::from([4, 4]))
    .into()
}

fn playlist_row_view<'a>(
    heart: String,
    num: String,
    title: String,
    author: String,
    released: String,
    time: String,
    sid_type: String,
    sids: String,
    is_header: bool,
    is_current: bool,
    is_selected: bool,
    _is_favorite: bool,
) -> Element<'a, Message> {
    let size = if is_header { 11 } else { 13 };
    let color = if is_header {
        Color::from_rgb(0.5, 0.5, 0.6)
    } else if is_current {
        Color::from_rgb(0.35, 0.85, 0.55)
    } else {
        Color::from_rgb(0.78, 0.80, 0.84)
    };

    let type_color = if is_header {
        color
    } else if sid_type == "RSID" {
        Color::from_rgb(0.9, 0.65, 0.35)
    } else {
        Color::from_rgb(0.5, 0.75, 0.9)
    };

    let bg = if is_selected {
        Some(iced::Background::Color(Color::from_rgba(
            0.3, 0.5, 0.8, 0.2,
        )))
    } else if is_current {
        Some(iced::Background::Color(Color::from_rgba(
            0.2, 0.6, 0.4, 0.1,
        )))
    } else {
        None
    };

    let indicator = if is_current && !is_header {
        "▶ "
    } else {
        "  "
    };

    let r = row![
        text(heart)
            .size(size)
            .color(color)
            .width(Length::Fixed(22.0)),
        text(format!("{indicator}{num:>3}"))
            .size(size)
            .color(color)
            .width(Length::Fixed(50.0)),
        text(title)
            .size(size)
            .color(color)
            .width(Length::FillPortion(4)),
        text(author)
            .size(size)
            .color(color)
            .width(Length::FillPortion(3)),
        text(released)
            .size(size)
            .color(color)
            .width(Length::FillPortion(2)),
        text(time)
            .size(size)
            .color(color)
            .width(Length::Fixed(55.0)),
        text(sid_type)
            .size(size)
            .color(type_color)
            .width(Length::Fixed(42.0)),
        text(sids)
            .size(size)
            .color(color)
            .width(Length::Fixed(45.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding(Padding::from([4, 16]));

    container(r)
        .width(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: bg,
            ..Default::default()
        })
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Settings panel
// ─────────────────────────────────────────────────────────────────────────────

/// Build the settings panel overlay.
pub fn settings_panel<'a>(
    config: &Config,
    default_length_text: &'a str,
    download_status: &'a str,
) -> Element<'a, Message> {
    let title = text("Settings")
        .size(18)
        .color(Color::from_rgb(0.85, 0.87, 0.9));

    let close_btn = tool_button("✕ Close", Message::ToggleSettings);

    let header =
        row![title, Space::new().width(Length::Fill), close_btn,].align_y(Alignment::Center);

    // ── Output Engine ──────────────────────────────────────────
    let engine_label = text("Audio output engine:")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82));

    let engines = crate::sid_device::available_engines();
    let current_engine = &config.output_engine;

    let engine_buttons: Vec<Element<'a, Message>> = engines
        .iter()
        .map(|&name| {
            let display = match name {
                "usb" => "🔌 USB Hardware (USBSID-Pico)",
                "emulated" => "🎵 Software Emulation (reSID)",
                other => other,
            };
            let is_active = current_engine == name
                || (current_engine == "auto" && engines.first() == Some(&name));
            let label = if current_engine == name {
                format!("● {display}")
            } else {
                format!("○ {display}")
            };
            let btn = button(text(label).size(12))
                .on_press(Message::SetOutputEngine(name.to_string()))
                .padding(Padding::from([4, 10]))
                .width(Length::Fill)
                .style(move |_theme: &Theme, status| {
                    let bg = if is_active {
                        match status {
                            button::Status::Hovered => Color::from_rgb(0.20, 0.30, 0.45),
                            button::Status::Pressed => Color::from_rgb(0.15, 0.22, 0.35),
                            _ => Color::from_rgb(0.16, 0.25, 0.40),
                        }
                    } else {
                        match status {
                            button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                            button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                            _ => Color::from_rgb(0.18, 0.19, 0.22),
                        }
                    };
                    button::Style {
                        background: Some(iced::Background::Color(bg)),
                        text_color: if is_active {
                            Color::from_rgb(0.9, 0.92, 0.96)
                        } else {
                            Color::from_rgb(0.8, 0.82, 0.88)
                        },
                        border: iced::Border {
                            radius: 3.0.into(),
                            width: 1.0,
                            color: if is_active {
                                Color::from_rgb(0.3, 0.45, 0.7)
                            } else {
                                Color::from_rgb(0.25, 0.27, 0.30)
                            },
                        },
                        ..Default::default()
                    }
                });
            btn.into()
        })
        .collect();

    // Auto button
    let auto_active = current_engine == "auto";
    let auto_btn = button(
        text(if auto_active {
            "● Auto (try USB, fall back to emulation)"
        } else {
            "○ Auto (try USB, fall back to emulation)"
        })
        .size(12),
    )
    .on_press(Message::SetOutputEngine("auto".to_string()))
    .padding(Padding::from([4, 10]))
    .width(Length::Fill)
    .style(move |_theme: &Theme, status| {
        let bg = if auto_active {
            match status {
                button::Status::Hovered => Color::from_rgb(0.20, 0.30, 0.45),
                button::Status::Pressed => Color::from_rgb(0.15, 0.22, 0.35),
                _ => Color::from_rgb(0.16, 0.25, 0.40),
            }
        } else {
            match status {
                button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                _ => Color::from_rgb(0.18, 0.19, 0.22),
            }
        };
        button::Style {
            background: Some(iced::Background::Color(bg)),
            text_color: if auto_active {
                Color::from_rgb(0.9, 0.92, 0.96)
            } else {
                Color::from_rgb(0.8, 0.82, 0.88)
            },
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: if auto_active {
                    Color::from_rgb(0.3, 0.45, 0.7)
                } else {
                    Color::from_rgb(0.25, 0.27, 0.30)
                },
            },
            ..Default::default()
        }
    });

    let mut engine_col = column![engine_label, auto_btn].spacing(6);
    for btn in engine_buttons {
        engine_col = engine_col.push(btn);
    }

    let engine_help = text("Changes take effect when next song starts playing.")
        .size(11)
        .color(Color::from_rgb(0.45, 0.47, 0.52));
    let engine_section = engine_col.push(engine_help);

    // ── Skip RSID ────────────────────────────────────────────────
    let rsid_label = text("Skip RSID tunes:")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82));

    let rsid_toggle = tool_button(
        if config.skip_rsid {
            "✓ Yes — skip RSID"
        } else {
            "✗ No — play all tunes"
        },
        Message::ToggleSkipRsid,
    );

    let rsid_help = text("When enabled, RSID tunes are automatically skipped during playback.")
        .size(11)
        .color(Color::from_rgb(0.45, 0.47, 0.52));

    let rsid_section = column![rsid_label, rsid_toggle, rsid_help].spacing(6);

    // ── Default song length ──────────────────────────────────────
    let length_label = text("Default song length (seconds):")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82));

    let length_input = text_input("0 = disabled", default_length_text)
        .on_input(Message::DefaultSongLengthChanged)
        .size(14)
        .padding(Padding::from([6, 10]))
        .width(Length::Fixed(180.0))
        .style(|_theme: &Theme, _status| text_input::Style {
            background: iced::Background::Color(Color::from_rgb(0.14, 0.15, 0.18)),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            icon: Color::from_rgb(0.5, 0.5, 0.6),
            placeholder: Color::from_rgb(0.4, 0.4, 0.5),
            value: Color::from_rgb(0.85, 0.87, 0.9),
            selection: Color::from_rgba(0.3, 0.5, 0.8, 0.3),
        });

    let current_val = if config.default_song_length_secs > 0 {
        let m = config.default_song_length_secs / 60;
        let s = config.default_song_length_secs % 60;
        format!(
            "Current: {}:{:02} ({}s)",
            m, s, config.default_song_length_secs
        )
    } else {
        "Disabled (0) — unknown songs won't auto-advance".to_string()
    };

    let length_info = text(current_val)
        .size(11)
        .color(Color::from_rgb(0.45, 0.47, 0.52));

    let length_help =
        text("Fallback duration for songs not found in Songlength DB. Set to 0 to disable.")
            .size(11)
            .color(Color::from_rgb(0.45, 0.47, 0.52));

    let length_section = column![length_label, length_input, length_info, length_help].spacing(6);

    // ── Songlength DB download ───────────────────────────────────
    let dl_label = text("HVSC Songlength database:")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82));

    let dl_url_input = text_input("Songlength.md5 URL", &config.songlength_url)
        .on_input(Message::SonglengthUrlChanged)
        .size(12)
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .style(|_theme: &Theme, _status| text_input::Style {
            background: iced::Background::Color(Color::from_rgb(0.14, 0.15, 0.18)),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            icon: Color::from_rgb(0.5, 0.5, 0.6),
            placeholder: Color::from_rgb(0.4, 0.4, 0.5),
            value: Color::from_rgb(0.85, 0.87, 0.9),
            selection: Color::from_rgba(0.3, 0.5, 0.8, 0.3),
        });

    let dl_btn = tool_button(
        "⬇ Download / Refresh Songlength.md5",
        Message::DownloadSonglength,
    );
    let load_btn = tool_button("📂 Load Songlength.md5 from file…", Message::LoadSonglength);

    let dl_status_color = if download_status.contains("Error") || download_status.contains("fail") {
        Color::from_rgb(1.0, 0.4, 0.4)
    } else if download_status.contains("success") || download_status.contains("Loaded") {
        Color::from_rgb(0.4, 0.9, 0.5)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };

    let dl_status = text(download_status).size(12).color(dl_status_color);

    let dl_section = column![dl_label, dl_url_input, dl_btn, load_btn, dl_status].spacing(6);

    // ── Assemble ─────────────────────────────────────────────────
    let content = column![
        header,
        rule::horizontal(1),
        engine_section,
        rule::horizontal(1),
        rsid_section,
        rule::horizontal(1),
        length_section,
        rule::horizontal(1),
        dl_section,
    ]
    .spacing(16)
    .padding(Padding::from([16, 24]))
    .width(Length::Fill);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn tool_button<'a>(label: &'a str, msg: Message) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(msg)
        .padding(Padding::from([4, 10]))
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                _ => Color::from_rgb(0.18, 0.19, 0.22),
            };
            button::Style {
                background: Some(iced::Background::Color(bg)),
                text_color: Color::from_rgb(0.8, 0.82, 0.88),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.25, 0.27, 0.30),
                },
                ..Default::default()
            }
        })
        .into()
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Filter playlist entries by search query and optional favorites-only mode.
/// Returns indices of entries that match (case-insensitive substring
/// against title, author, released, and file path).
pub fn filter_playlist(
    playlist: &Playlist,
    query: &str,
    favorites_only: bool,
    favorites: &FavoritesDb,
) -> Vec<usize> {
    let q = query.to_lowercase();

    playlist
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            // Favorites filter
            if favorites_only {
                let is_fav = entry
                    .md5
                    .as_ref()
                    .map(|m| favorites.is_favorite(m))
                    .unwrap_or(false);
                if !is_fav {
                    return false;
                }
            }

            // Text search filter
            if q.is_empty() {
                return true;
            }

            let type_str = if entry.is_rsid { "rsid" } else { "psid" };
            entry.title.to_lowercase().contains(&q)
                || entry.author.to_lowercase().contains(&q)
                || entry.released.to_lowercase().contains(&q)
                || entry.path.to_string_lossy().to_lowercase().contains(&q)
                || type_str.contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

pub mod right_click;
pub mod sid_panel;
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
use crate::recently_played::{format_played_at, RecentlyPlayed};
use right_click::RightClickArea;
use visualizer::Visualizer;

/// Fixed scrollable ID for the playlist widget.
pub fn playlist_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("phosphor-playlist")
}

/// Fixed scrollable ID for the recently played widget.
pub fn recent_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("phosphor-recent")
}

/// ID for the search text input — used by Ctrl+F focus shortcut.
pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("phosphor-search")
}

// ─────────────────────────────────────────────────────────────────────────────
//  Sort state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// Original load order (#).
    Index,
    Title,
    Author,
    Released,
    Duration,
    /// PSID / RSID type column.
    SidType,
    /// Number of SID chips (1SID / 2SID / 3SID).
    NumSids,
}

/// Sort direction — toggled when the user clicks the same column header twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    /// Flip to the opposite direction.
    pub fn flip(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    /// Arrow indicator shown next to the active column header.
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Ascending => " ▲",
            Self::Descending => " ▼",
        }
    }
}

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

    // Sort
    SortBy(SortColumn),

    // Keyboard navigation
    SelectPrev,
    SelectNext,
    FocusSearch,

    // Virtual scroll — fired by the scrollable widget on every scroll event.
    /// Carries the new absolute Y offset in pixels so the virtual list can
    /// recompute which rows are in the viewport.
    PlaylistScrolled(iced::widget::scrollable::Viewport),

    // Context menu
    ShowContextMenu(usize, f32, f32), // track_idx, abs_x, abs_y
    DismissContextMenu,
    ContextMenuPlay,
    ContextMenuRemove,
    ContextMenuMoveToTop,
    ContextMenuToggleFavorite,
    ContextMenuCopyTitle,

    // Recently played
    ShowRecentlyPlayed,
    PlayRecentEntry(usize),
    ClearRecentlyPlayed,

    // Player status tick
    Tick,

    // File dialog results
    FilesChosen(Vec<PathBuf>),
    FolderChosen(Option<PathBuf>),
    SonglengthFileChosen(Option<PathBuf>),
    PlaylistSaved(Result<PathBuf, String>),
    PlaylistFileChosen(Option<PathBuf>),

    // Background loading
    FilesLoaded(Vec<crate::playlist::PlaylistEntry>),
    FolderLoaded(Vec<crate::playlist::PlaylistEntry>),
    PlaylistLoaded(Result<Vec<crate::playlist::PlaylistEntry>, String>),

    // Chained post-processing
    ProcessPendingEntries,
    FinalizePendingEntries,

    // Settings
    ToggleSettings,
    ToggleSkipRsid,
    ToggleForceStereo2sid,
    DefaultSongLengthChanged(String),
    SonglengthUrlChanged(String),
    DownloadSonglength,
    SonglengthDownloaded(Result<PathBuf, String>),
    SetOutputEngine(String),
    SetU64Address(String),
    SetU64Password(String),

    // Favorites
    ToggleFavorite(usize),
    ToggleFavoritesFilter,
    FavoriteNowPlaying,
    ScrollToNowPlaying,

    // File drag & drop
    FileDropped(PathBuf),

    // Window
    WindowResized(f32, f32),
    WindowMoved(i32, i32),

    // Visualiser
    /// Toggle between Bar and Scope display modes.
    ToggleVisMode,

    // Panels
    /// Toggle the SID register info panel (mutually exclusive with settings
    /// and recently played).
    ToggleSidPanel,

    // Version check
    VersionCheckDone(Result<Option<crate::version_check::NewVersionInfo>, String>),
    OpenUpdateUrl,

    // No-op
    None,
}

// ─────────────────────────────────────────────────────────────────────────────
//  View builders
// ─────────────────────────────────────────────────────────────────────────────

/// Build the track info + visualiser panel (top section).
/// Switches to a compact layout when `window_width` is below 760 px.
pub fn track_info_bar<'a>(
    status: &'a PlayerStatus,
    visualizer: &'a Visualizer,
    is_now_playing_favorite: bool,
    has_track: bool,
    window_width: f32,
) -> Element<'a, Message> {
    let compact = window_width < 760.0;
    let title_size = if compact { 15.0_f32 } else { 18.0 };
    let author_size = if compact { 12.0_f32 } else { 14.0 };
    let extra_size = if compact { 10.0_f32 } else { 12.0 };
    let vis_width = if compact { 200.0_f32 } else { 300.0 };
    let vis_height = if compact { 48.0_f32 } else { 60.0 };

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
        text(format!("{state_icon}  {title}")).size(title_size),
        text(author)
            .size(author_size)
            .color(Color::from_rgb(0.6, 0.7, 0.8)),
        text(extra)
            .size(extra_size)
            .color(Color::from_rgb(0.5, 0.5, 0.6)),
    ]
    .spacing(2)
    .width(Length::Fill);

    if let Some(ref err) = status.error {
        info_col = info_col.push(
            text(format!("⚠ {err}"))
                .size(12)
                .color(Color::from_rgb(1.0, 0.3, 0.3)),
        );
    }

    let now_playing_buttons: Element<'_, Message> = if has_track {
        let heart_label = if is_now_playing_favorite {
            "♥"
        } else {
            "♡"
        };
        let heart_color = if is_now_playing_favorite {
            Color::from_rgb(1.0, 0.35, 0.45)
        } else {
            Color::from_rgb(0.5, 0.5, 0.6)
        };
        let heart_btn = button(text(heart_label).size(18).color(heart_color))
            .on_press(Message::FavoriteNowPlaying)
            .padding(Padding::from([4, 6]))
            .style(|_theme: &Theme, _status| button::Style {
                background: None,
                text_color: Color::WHITE,
                ..Default::default()
            });
        let scroll_btn = button(text("⌖").size(16).color(Color::from_rgb(0.5, 0.5, 0.6)))
            .on_press(Message::ScrollToNowPlaying)
            .padding(Padding::from([4, 6]))
            .style(|_theme: &Theme, _status| button::Style {
                background: None,
                text_color: Color::WHITE,
                ..Default::default()
            });
        column![heart_btn, scroll_btn]
            .spacing(0)
            .align_x(Alignment::Center)
            .into()
    } else {
        column![].into()
    };

    let content = row![
        info_col,
        now_playing_buttons,
        container(visualizer.view())
            .width(Length::Fixed(vis_width))
            .height(Length::Fixed(vis_height)),
    ]
    .spacing(8)
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

/// Build the thin progress bar showing elapsed / total time below the track info.
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
    let bar_pct = (fraction * 100.0) as u16;

    let filled = container(Space::new().height(Length::Fixed(4.0)))
        .width(Length::FillPortion(bar_pct.max(1)))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.30, 0.70, 0.50))),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });
    let remaining = container(Space::new().height(Length::Fixed(4.0)))
        .width(Length::FillPortion(100u16.saturating_sub(bar_pct).max(1)))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.19, 0.22))),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    container(
        row![
            row![filled, remaining].spacing(0).width(Length::Fill),
            time_label
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([4, 16]))
    .width(Length::Fill)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
        ..Default::default()
    })
    .into()
}

/// Build the transport controls bar (play/pause, prev/next, shuffle, repeat,
/// playlist management buttons). Wraps to two rows in compact mode.
pub fn controls_bar<'a>(
    status: &PlayerStatus,
    playlist: &Playlist,
    new_version: Option<&crate::version_check::NewVersionInfo>,
    window_width: f32,
    show_recently_played: bool,
    show_sid_panel: bool,
) -> Element<'a, Message> {
    let compact = window_width < 760.0;
    let btn_size = if compact { 11.0_f32 } else { 12.0 };
    let btn_pad = if compact { 3_u16 } else { 4 };
    let bar_pad = if compact { 4_u16 } else { 6 };

    let play_label = match status.state {
        PlayState::Playing => "❚❚",
        _ => "▶",
    };

    let small_button = |label: &'a str, msg: Message| -> Element<'a, Message> {
        button(text(label).size(btn_size))
            .on_press(msg)
            .padding(Padding::from([btn_pad, if compact { 6 } else { 10 }]))
            .style(|_theme: &Theme, st| {
                let bg = match st {
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
    };

    let sep = || -> Element<'a, Message> {
        text(" │ ")
            .size(btn_size)
            .color(Color::from_rgb(0.3, 0.3, 0.35))
            .into()
    };

    let transport = row![
        small_button("◄◄", Message::PrevTrack),
        small_button(play_label, Message::PlayPause),
        small_button("■", Message::Stop),
        small_button("►►", Message::NextTrack),
    ]
    .spacing(4);

    let subtune_controls = row![
        small_button("◄ tune", Message::PrevSubtune),
        small_button("tune ►", Message::NextSubtune),
    ]
    .spacing(4);

    let mode_controls = row![
        small_button(
            if playlist.shuffle {
                "🔀 On"
            } else {
                "🔀 Off"
            },
            Message::ToggleShuffle
        ),
        small_button(playlist.repeat.label(), Message::CycleRepeat),
    ]
    .spacing(4);

    let recent_btn: Element<'a, Message> =
        button(text(if compact { "🕐" } else { "🕐 Recent" }).size(btn_size))
            .on_press(Message::ShowRecentlyPlayed)
            .padding(Padding::from([btn_pad, if compact { 6 } else { 10 }]))
            .style(move |_theme: &Theme, st| {
                let bg = if show_recently_played {
                    match st {
                        button::Status::Hovered => Color::from_rgb(0.20, 0.30, 0.45),
                        button::Status::Pressed => Color::from_rgb(0.15, 0.22, 0.35),
                        _ => Color::from_rgb(0.16, 0.25, 0.40),
                    }
                } else {
                    match st {
                        button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                        button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                        _ => Color::from_rgb(0.18, 0.19, 0.22),
                    }
                };
                button::Style {
                    background: Some(iced::Background::Color(bg)),
                    text_color: if show_recently_played {
                        Color::from_rgb(0.55, 0.80, 1.0)
                    } else {
                        Color::from_rgb(0.8, 0.82, 0.88)
                    },
                    border: iced::Border {
                        radius: 3.0.into(),
                        width: 1.0,
                        color: if show_recently_played {
                            Color::from_rgb(0.3, 0.45, 0.7)
                        } else {
                            Color::from_rgb(0.25, 0.27, 0.30)
                        },
                    },
                    ..Default::default()
                }
            })
            .into();

    let sid_btn: Element<'a, Message> =
        button(text(if compact { "SID" } else { "SID" }).size(btn_size))
            .on_press(Message::ToggleSidPanel)
            .padding(Padding::from([btn_pad, if compact { 6 } else { 10 }]))
            .style(move |_theme: &Theme, st| {
                let bg = if show_sid_panel {
                    match st {
                        button::Status::Hovered => Color::from_rgb(0.15, 0.35, 0.25),
                        button::Status::Pressed => Color::from_rgb(0.10, 0.28, 0.18),
                        _ => Color::from_rgb(0.11, 0.30, 0.20),
                    }
                } else {
                    match st {
                        button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                        button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                        _ => Color::from_rgb(0.18, 0.19, 0.22),
                    }
                };
                button::Style {
                    background: Some(iced::Background::Color(bg)),
                    text_color: if show_sid_panel {
                        Color::from_rgb(0.30, 0.85, 0.55)
                    } else {
                        Color::from_rgb(0.8, 0.82, 0.88)
                    },
                    border: iced::Border {
                        radius: 3.0.into(),
                        width: 1.0,
                        color: if show_sid_panel {
                            Color::from_rgb(0.20, 0.55, 0.35)
                        } else {
                            Color::from_rgb(0.25, 0.27, 0.30)
                        },
                    },
                    ..Default::default()
                }
            })
            .into();

    let playlist_controls = if compact {
        row![
            small_button("+Files", Message::AddFiles),
            small_button("+Dir", Message::AddFolder),
            small_button("📂", Message::LoadPlaylist),
            small_button("💾", Message::SavePlaylist),
            small_button("🗑", Message::ClearPlaylist),
            recent_btn,
            sid_btn,
            small_button("⚙", Message::ToggleSettings),
        ]
        .spacing(3)
    } else {
        row![
            small_button("+ Files", Message::AddFiles),
            small_button("+ Folder", Message::AddFolder),
            small_button("📂 Open", Message::LoadPlaylist),
            small_button("💾 Save", Message::SavePlaylist),
            small_button("🗑 Clear", Message::ClearPlaylist),
            recent_btn,
            sid_btn,
            small_button("⚙", Message::ToggleSettings),
        ]
        .spacing(4)
    };

    let update_badge = |version: &str| -> Element<'a, Message> {
        button(
            text(format!("⬆ {version}"))
                .size(if compact { 11.0 } else { 12.0 })
                .color(Color::from_rgb(0.1, 0.1, 0.12)),
        )
        .on_press(Message::OpenUpdateUrl)
        .padding(Padding::from([
            if compact { 2 } else { 3 },
            if compact { 6 } else { 8 },
        ]))
        .style(|_theme: &Theme, _st| button::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.35, 0.85, 0.55))),
            text_color: Color::from_rgb(0.1, 0.1, 0.12),
            border: iced::Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
    };

    let bar: Element<'a, Message> = if compact {
        let top_row = row![transport, sep(), subtune_controls, sep(), mode_controls]
            .spacing(6)
            .align_y(Alignment::Center);
        let mut bottom_row = row![Space::new().width(Length::Fill)]
            .spacing(4)
            .align_y(Alignment::Center);
        if let Some(info) = new_version {
            bottom_row = bottom_row.push(update_badge(&info.version));
        }
        bottom_row = bottom_row.push(playlist_controls);
        column![top_row, bottom_row]
            .spacing(4)
            .padding(Padding::from([bar_pad, 12]))
            .into()
    } else {
        let mut bar_row = row![
            transport,
            sep(),
            subtune_controls,
            sep(),
            mode_controls,
            Space::new().width(Length::Fill)
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(info) = new_version {
            bar_row = bar_row.push(update_badge(&info.version));
        }
        bar_row = bar_row.push(playlist_controls);
        bar_row.padding(Padding::from([bar_pad, 16])).into()
    };

    container(bar)
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.12, 0.13, 0.16))),
            ..Default::default()
        })
        .into()
}

/// Build the search / filter bar with track count and favorites toggle.
pub fn search_bar<'a>(
    search_text: &str,
    visible_count: usize,
    total_count: usize,
    favorites_only: bool,
    favorites_count: usize,
    loading_status: &str,
) -> Element<'a, Message> {
    let search_input = text_input("Search playlist...", search_text)
        .id(search_input_id())
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

    let count_text = if !loading_status.is_empty() {
        loading_status.to_string()
    } else if favorites_only {
        format!("♥ {} / {} tracks", visible_count, total_count)
    } else if !search_text.is_empty() {
        format!("{} / {} tracks", visible_count, total_count)
    } else {
        format!("{} tracks", total_count)
    };

    let count_color = if !loading_status.is_empty() {
        Color::from_rgb(0.4, 0.75, 0.9)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };
    let fav_label = if favorites_only {
        format!("♥ {favorites_count}")
    } else {
        format!("♡ {favorites_count}")
    };

    let fav_btn = button(text(fav_label).size(12))
        .on_press(Message::ToggleFavoritesFilter)
        .padding(Padding::from([4, 10]))
        .style(move |_theme: &Theme, st| {
            let bg = if favorites_only {
                match st {
                    button::Status::Hovered => Color::from_rgb(0.35, 0.18, 0.20),
                    button::Status::Pressed => Color::from_rgb(0.28, 0.14, 0.16),
                    _ => Color::from_rgb(0.30, 0.15, 0.18),
                }
            } else {
                match st {
                    button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                    button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                    _ => Color::from_rgb(0.18, 0.19, 0.22),
                }
            };
            button::Style {
                background: Some(iced::Background::Color(bg)),
                text_color: if favorites_only {
                    Color::from_rgb(1.0, 0.4, 0.5)
                } else {
                    Color::from_rgb(0.8, 0.82, 0.88)
                },
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

    container(
        row![
            search_row,
            Space::new().width(Length::Fixed(8.0)),
            fav_btn,
            Space::new().width(Length::Fixed(8.0)),
            text(count_text).size(12).color(count_color)
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .padding(Padding::from([4, 16])),
    )
    .width(Length::Fill)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.11, 0.12, 0.14))),
        ..Default::default()
    })
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Virtual list constants
// ─────────────────────────────────────────────────────────────────────────────

/// Height of a single playlist row in logical pixels.
/// Must match the actual rendered height: 4px top pad + 13px text + 4px bottom
/// pad + 1px rule = 22px.  We add a small buffer (26px) so a partially visible
/// row at either edge is always included.
pub const ROW_HEIGHT: f32 = 26.0;

/// Number of extra rows to render above and below the visible window.
/// Acts as a scroll lookahead so rows don't pop in mid-scroll.
const OVERSCAN: usize = 8;

/// Build the scrollable playlist table with sortable column headers.
/// `filtered_indices` maps visible row position → actual `playlist.entries` index.
///
/// Virtual scrolling: only the rows currently in the viewport (plus `OVERSCAN`
/// above/below) are built as iced widgets.  The rest of the space is filled by
/// two `Space` widgets so the scrollbar thumb stays correctly sized.
pub fn playlist_view<'a>(
    playlist: &Playlist,
    selected: Option<usize>,
    filtered_indices: &[usize],
    favorites: &FavoritesDb,
    sort_col: SortColumn,
    sort_dir: SortDirection,
    scroll_offset_y: f32,
    viewport_height: f32,
) -> Element<'a, Message> {
    let header_btn = move |label: &'static str, col: SortColumn| -> Element<'a, Message> {
        let is_active = sort_col == col;
        let display = if is_active {
            format!("{}{}", label, sort_dir.arrow())
        } else {
            label.to_string()
        };
        let text_color = if is_active {
            Color::from_rgb(0.75, 0.88, 1.0)
        } else {
            Color::from_rgb(0.5, 0.5, 0.6)
        };
        button(text(display).size(11).color(text_color))
            .on_press(Message::SortBy(col))
            .padding(Padding::from([2, 4]))
            .style(|_theme: &Theme, st| button::Style {
                background: match st {
                    button::Status::Hovered => Some(iced::Background::Color(Color::from_rgba(
                        1.0, 1.0, 1.0, 0.06,
                    ))),
                    _ => None,
                },
                text_color: Color::WHITE,
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    };

    // ── Header (lives outside the scrollable so it never scrolls away) ───────
    let header = container(
        row![
            text("♥")
                .size(11)
                .color(Color::from_rgb(0.5, 0.5, 0.6))
                .width(Length::Fixed(22.0)),
            container(header_btn("#", SortColumn::Index)).width(Length::Fixed(50.0)),
            container(header_btn("Title", SortColumn::Title)).width(Length::FillPortion(4)),
            container(header_btn("Author", SortColumn::Author)).width(Length::FillPortion(3)),
            container(header_btn("Released", SortColumn::Released)).width(Length::FillPortion(2)),
            container(header_btn("Time", SortColumn::Duration)).width(Length::Fixed(55.0)),
            container(header_btn("Type", SortColumn::SidType)).width(Length::Fixed(42.0)),
            container(header_btn("SIDs", SortColumn::NumSids)).width(Length::Fixed(45.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding(Padding::from([4, 16])),
    )
    .width(Length::Fill)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.11, 0.12, 0.15))),
        ..Default::default()
    });

    // ── Scrollable rows (no header inside) ───────────────────────────────────
    let mut rows = Column::new().spacing(0);

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
        let total_rows = filtered_indices.len();

        // ── Virtual window calculation ────────────────────────────────────
        // Compute which rows are visible, with overscan on both sides.
        let first_visible = ((scroll_offset_y / ROW_HEIGHT) as usize).saturating_sub(OVERSCAN);
        let rows_in_view = (viewport_height / ROW_HEIGHT).ceil() as usize + 1;
        let last_visible = (first_visible + rows_in_view + OVERSCAN * 2).min(total_rows);

        // Top spacer — replaces all rows above the render window
        let top_space = first_visible as f32 * ROW_HEIGHT;
        if top_space > 0.0 {
            rows = rows.push(Space::new().height(Length::Fixed(top_space)));
        }

        // Visible rows — only these are built as iced widgets
        for display_pos in first_visible..last_visible {
            let actual_idx = filtered_indices[display_pos];
            if let Some(entry) = playlist.entries.get(actual_idx) {
                let is_current = playlist.current == Some(actual_idx);
                let is_selected = selected == Some(actual_idx);
                let is_fav = entry
                    .md5
                    .as_ref()
                    .map(|m| favorites.is_favorite(m))
                    .unwrap_or(false);
                rows = rows.push(playlist_entry_row(
                    actual_idx,
                    display_pos + 1,
                    entry,
                    is_current,
                    is_selected,
                    is_fav,
                ));
            }
        }

        // Bottom spacer — replaces all rows below the render window
        let bottom_rows = total_rows.saturating_sub(last_visible);
        let bottom_space = bottom_rows as f32 * ROW_HEIGHT;
        if bottom_space > 0.0 {
            rows = rows.push(Space::new().height(Length::Fixed(bottom_space)));
        }
    }

    let scroll = scrollable(rows)
        .id(playlist_scrollable_id())
        .on_scroll(Message::PlaylistScrolled)
        .width(Length::Fill)
        .height(Length::Fill);

    // Stack header above the scrollable — both sit in a column so the header
    // takes its natural height and the scrollable fills the rest.
    column![header, rule::horizontal(1), scroll]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Build the floating context menu overlay.
/// Layer this over the main content using `iced::widget::stack![]` when
/// `context_menu` is `Some(_)`. The menu is flipped horizontally or vertically
/// if it would extend beyond the window edges.
pub fn context_menu_overlay<'a>(
    x: f32,
    y: f32,
    track_idx: usize,
    playlist: &Playlist,
    favorites: &FavoritesDb,
    window_width: f32,
    window_height: f32,
) -> Element<'a, Message> {
    let is_fav = playlist
        .entries
        .get(track_idx)
        .and_then(|e| e.md5.as_ref())
        .map(|m| favorites.is_favorite(m))
        .unwrap_or(false);

    let fav_label = if is_fav {
        "♥  Remove from favorites"
    } else {
        "♡  Add to favorites"
    };

    let menu_width = 210.0_f32;
    let item_height = 32.0_f32;
    let menu_height = item_height * 5.0 + 8.0;

    // Flip so menu never goes off-screen
    let menu_x = if x + menu_width > window_width {
        (x - menu_width).max(0.0)
    } else {
        x
    };
    let menu_y = if y + menu_height > window_height {
        (y - menu_height).max(0.0)
    } else {
        y
    };

    let item = |icon_label: &'a str, msg: Message| -> Element<'a, Message> {
        button(text(icon_label).size(13))
            .on_press(msg)
            .width(Length::Fill)
            .padding(Padding::from([7, 14]))
            .style(|_theme: &Theme, st| button::Style {
                background: Some(iced::Background::Color(match st {
                    button::Status::Hovered => Color::from_rgb(0.25, 0.40, 0.65),
                    button::Status::Pressed => Color::from_rgb(0.20, 0.33, 0.55),
                    _ => Color::from_rgba(0.0, 0.0, 0.0, 0.0),
                })),
                text_color: Color::from_rgb(0.88, 0.90, 0.94),
                border: iced::Border::default(),
                ..Default::default()
            })
            .into()
    };

    let menu_box = container(
        column![
            item("▶   Play", Message::ContextMenuPlay),
            item("⤒   Move to top", Message::ContextMenuMoveToTop),
            item(fav_label, Message::ContextMenuToggleFavorite),
            item("⎘   Copy title", Message::ContextMenuCopyTitle),
            item("✕   Remove from playlist", Message::ContextMenuRemove),
        ]
        .spacing(0)
        .width(Length::Fixed(menu_width)),
    )
    .padding(Padding::from([4, 0]))
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.15, 0.16, 0.20))),
        border: iced::Border {
            radius: 5.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.28, 0.30, 0.36),
        },
        shadow: iced::Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
            offset: iced::Vector::new(2.0, 4.0),
            blur_radius: 8.0,
        },
        ..Default::default()
    });

    // Transparent full-screen dismiss button sits below the menu popup
    let dismiss = button(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::DismissContextMenu)
        .padding(0)
        .style(|_theme: &Theme, _st| button::Style {
            background: Some(iced::Background::Color(Color::from_rgba(
                0.0, 0.0, 0.0, 0.0,
            ))),
            ..Default::default()
        })
        .width(Length::Fill)
        .height(Length::Fill);

    // Position the menu by padding a fill container from top-left
    let positioned = container(menu_box)
        .padding(Padding {
            top: menu_y,
            left: menu_x,
            right: 0.0,
            bottom: 0.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: None,
            ..Default::default()
        });

    iced::widget::stack![dismiss, positioned]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Build the recently played panel (shown instead of the playlist when active).
pub fn recently_played_view<'a>(
    recent: &'a RecentlyPlayed,
    current_md5: Option<&'a str>,
) -> Element<'a, Message> {
    let header_row = row![
        text("#")
            .size(11)
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::Fixed(40.0)),
        text("Title")
            .size(11)
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(4)),
        text("Author")
            .size(11)
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(3)),
        text("Released")
            .size(11)
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(2)),
        text("Played")
            .size(11)
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::Fixed(110.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding(Padding::from([4, 16]));

    let header = container(header_row)
        .width(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.11, 0.12, 0.15))),
            ..Default::default()
        });

    let toolbar = container(
        row![
            text(format!("🕐  {} recently played tracks", recent.len()))
                .size(12)
                .color(Color::from_rgb(0.55, 0.80, 1.0)),
            Space::new().width(Length::Fill),
            tool_button("🗑 Clear history", Message::ClearRecentlyPlayed),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding(Padding::from([6, 16])),
    )
    .width(Length::Fill)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.10, 0.11, 0.13))),
        ..Default::default()
    });

    let mut rows = Column::new()
        .spacing(0)
        .push(toolbar)
        .push(header)
        .push(rule::horizontal(1));

    if recent.is_empty() {
        rows = rows.push(
            container(
                text("No recently played tracks yet — start listening!")
                    .size(14)
                    .color(Color::from_rgb(0.4, 0.4, 0.5)),
            )
            .padding(40)
            .center_x(Length::Fill),
        );
    } else {
        for (i, entry) in recent.entries.iter().enumerate() {
            let is_current = current_md5 == Some(entry.md5.as_str());
            let color = if is_current {
                Color::from_rgb(0.35, 0.85, 0.55)
            } else {
                Color::from_rgb(0.78, 0.80, 0.84)
            };
            let indicator = if is_current { "▶ " } else { "  " };

            let row_content = row![
                text(format!("{}{}", indicator, i + 1))
                    .size(13)
                    .color(color)
                    .width(Length::Fixed(40.0)),
                text(entry.title.clone())
                    .size(13)
                    .color(color)
                    .width(Length::FillPortion(4)),
                text(entry.author.clone())
                    .size(13)
                    .color(color)
                    .width(Length::FillPortion(3)),
                text(entry.released.clone())
                    .size(13)
                    .color(color)
                    .width(Length::FillPortion(2)),
                text(format_played_at(entry.played_at))
                    .size(12)
                    .color(Color::from_rgb(0.5, 0.55, 0.65))
                    .width(Length::Fixed(110.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .padding(Padding::from([4, 4]));

            let row_btn = button(row_content)
                .on_press(Message::PlayRecentEntry(i))
                .padding(0)
                .style(move |_theme: &Theme, st| button::Style {
                    background: match st {
                        button::Status::Hovered => Some(iced::Background::Color(Color::from_rgba(
                            1.0, 1.0, 1.0, 0.04,
                        ))),
                        _ => {
                            if is_current {
                                Some(iced::Background::Color(Color::from_rgba(
                                    0.2, 0.6, 0.4, 0.1,
                                )))
                            } else {
                                None
                            }
                        }
                    },
                    text_color: Color::WHITE,
                    ..Default::default()
                })
                .width(Length::Fill);

            rows = rows.push(
                container(row_btn)
                    .width(Length::Fill)
                    .padding(Padding::from([0, 12])),
            );
        }
    }

    scrollable(rows)
        .id(recent_scrollable_id())
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Build a single playlist row, including the heart button and right-click wrapper.
/// `display_pos` is the 1-based row number shown in the # column (sorted order).
fn playlist_entry_row<'a>(
    idx: usize,
    display_pos: usize,
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

    let heart_label = if is_favorite { "♥" } else { "♡" };
    let heart_color = if is_favorite {
        Color::from_rgb(1.0, 0.35, 0.45)
    } else {
        Color::from_rgb(0.35, 0.35, 0.40)
    };

    let heart_btn = button(text(heart_label).size(13).color(heart_color))
        .on_press(Message::ToggleFavorite(idx))
        .padding(Padding::from([4, 4]))
        .style(|_theme: &Theme, st| button::Style {
            background: match st {
                button::Status::Hovered => Some(iced::Background::Color(Color::from_rgba(
                    1.0, 0.3, 0.4, 0.15,
                ))),
                _ => None,
            },
            text_color: Color::WHITE,
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let row_content = playlist_row_content(
        format!("{display_pos}"),
        song_title,
        entry.author.clone(),
        entry.released.clone(),
        entry.format_duration(),
        type_label,
        sids_label,
        is_current,
    );

    // Left-click: select / play
    let row_btn = button(row_content)
        .on_press(Message::PlaylistSelect(idx))
        .padding(0)
        .style(|_theme: &Theme, _st| button::Style {
            background: None,
            text_color: Color::WHITE,
            ..Default::default()
        })
        .width(Length::Fill);

    // Right-click: open context menu at cursor position
    let row_with_rclick: Element<'a, Message> =
        RightClickArea::new(row_btn, move |x, y| Message::ShowContextMenu(idx, x, y)).into();

    container(
        row![heart_btn, row_with_rclick]
            .spacing(0)
            .align_y(Alignment::Center)
            .padding(Padding::from([0, 4])),
    )
    .width(Length::Fill)
    .style(move |_theme: &Theme| container::Style {
        background: bg,
        ..Default::default()
    })
    .into()
}

/// Build the inner row content (without the heart button).
/// Used as the child of the left-click button inside each playlist row.
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

// ─────────────────────────────────────────────────────────────────────────────
//  Settings panel
// ─────────────────────────────────────────────────────────────────────────────

/// Build the settings panel (shown instead of the playlist when ⚙ is toggled).
pub fn settings_panel<'a>(
    config: &Config,
    default_length_text: &'a str,
    download_status: &'a str,
) -> Element<'a, Message> {
    let header = row![
        text("Settings")
            .size(18)
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
        Space::new().width(Length::Fill),
        tool_button("✕ Close", Message::ToggleSettings),
    ]
    .align_y(Alignment::Center);

    // ── Output Engine ────────────────────────────────────────────
    let engines = crate::sid_device::available_engines();
    let current_engine = &config.output_engine;

    let mut engine_col = column![text("Audio output engine:")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82)),]
    .spacing(6);

    let auto_active = current_engine == "auto";
    engine_col = engine_col.push(
        button(
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
        .style(move |_theme: &Theme, st| engine_btn_style(auto_active, st)),
    );

    for &name in &engines {
        let display = match name {
            "usb" => "🔌 USB Hardware (USBSID-Pico)",
            "emulated" => "🎵 Software Emulation (reSID)",
            "u64" => "🌐 Ultimate 64 (Network)",
            other => other,
        };
        let is_active = current_engine == name;
        let label = if is_active {
            format!("● {display}")
        } else {
            format!("○ {display}")
        };
        engine_col = engine_col.push(
            button(text(label).size(12))
                .on_press(Message::SetOutputEngine(name.to_string()))
                .padding(Padding::from([4, 10]))
                .width(Length::Fill)
                .style(move |_theme: &Theme, st| engine_btn_style(is_active, st)),
        );
    }

    engine_col = engine_col
        .push(
            text("Changes take effect when next song starts playing.")
                .size(11)
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        )
        .push(rule::horizontal(1))
        .push(
            text("Ultimate 64 connection:")
                .size(12)
                .color(Color::from_rgb(0.65, 0.67, 0.72)),
        )
        .push(
            text_input("IP address (e.g. 192.168.1.64)", &config.u64_address)
                .on_input(Message::SetU64Address)
                .size(12)
                .padding(Padding::from([4, 8]))
                .width(Length::Fill),
        )
        .push(
            text_input("Password (leave empty if none)", &config.u64_password)
                .on_input(Message::SetU64Password)
                .size(12)
                .padding(Padding::from([4, 8]))
                .width(Length::Fill),
        )
        .push(
            text("Set IP/hostname of your Ultimate 64 or Ultimate-II+ device.")
                .size(11)
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        );

    // ── Skip RSID ────────────────────────────────────────────────
    let rsid_section = column![
        text("Skip RSID tunes:")
            .size(14)
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        tool_button(
            if config.skip_rsid {
                "✓ Yes — skip RSID"
            } else {
                "✗ No — play all tunes"
            },
            Message::ToggleSkipRsid
        ),
        text("When enabled, RSID tunes are automatically skipped during playback.")
            .size(11)
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
    ]
    .spacing(6);

    // ── Force stereo ─────────────────────────────────────────────
    let stereo_section = column![
        text("Force stereo for 2SID tunes:").size(14).color(Color::from_rgb(0.75, 0.77, 0.82)),
        tool_button(
            if config.force_stereo_2sid { "✓ Yes — mirror SID1 to both channels" } else { "✗ No — true dual-SID (L=SID1, R=SID2)" },
            Message::ToggleForceStereo2sid,
        ),
        text("When enabled, 2SID tunes ignore the second SID and mirror SID1 to both speakers (same as mono).").size(11).color(Color::from_rgb(0.45, 0.47, 0.52)),
    ].spacing(6);

    // ── Default song length ──────────────────────────────────────
    let cur_len = if config.default_song_length_secs > 0 {
        let m = config.default_song_length_secs / 60;
        let s = config.default_song_length_secs % 60;
        format!(
            "Current: {}:{:02} ({}s)",
            m, s, config.default_song_length_secs
        )
    } else {
        "Disabled (0) — unknown songs won't auto-advance".to_string()
    };

    let length_section = column![
        text("Default song length (seconds):")
            .size(14)
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text_input("0 = disabled", default_length_text)
            .on_input(Message::DefaultSongLengthChanged)
            .size(14)
            .padding(Padding::from([6, 10]))
            .width(Length::Fixed(180.0))
            .style(|_theme: &Theme, _st| text_input::Style {
                background: iced::Background::Color(Color::from_rgb(0.14, 0.15, 0.18)),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.25, 0.27, 0.30)
                },
                icon: Color::from_rgb(0.5, 0.5, 0.6),
                placeholder: Color::from_rgb(0.4, 0.4, 0.5),
                value: Color::from_rgb(0.85, 0.87, 0.9),
                selection: Color::from_rgba(0.3, 0.5, 0.8, 0.3),
            }),
        text(cur_len)
            .size(11)
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
        text("Fallback duration for songs not found in Songlength DB. Set to 0 to disable.")
            .size(11)
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
    ]
    .spacing(6);

    // ── Songlength DB ────────────────────────────────────────────
    let dl_color = if download_status.contains("Error") || download_status.contains("fail") {
        Color::from_rgb(1.0, 0.4, 0.4)
    } else if download_status.contains("success") || download_status.contains("Loaded") {
        Color::from_rgb(0.4, 0.9, 0.5)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };

    let dl_section = column![
        text("HVSC Songlength database:")
            .size(14)
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text_input("Songlength.md5 URL", &config.songlength_url)
            .on_input(Message::SonglengthUrlChanged)
            .size(12)
            .padding(Padding::from([6, 10]))
            .width(Length::Fill)
            .style(|_theme: &Theme, _st| text_input::Style {
                background: iced::Background::Color(Color::from_rgb(0.14, 0.15, 0.18)),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.25, 0.27, 0.30)
                },
                icon: Color::from_rgb(0.5, 0.5, 0.6),
                placeholder: Color::from_rgb(0.4, 0.4, 0.5),
                value: Color::from_rgb(0.85, 0.87, 0.9),
                selection: Color::from_rgba(0.3, 0.5, 0.8, 0.3),
            }),
        tool_button(
            "⬇ Download / Refresh Songlength.md5",
            Message::DownloadSonglength
        ),
        tool_button("📂 Load Songlength.md5 from file…", Message::LoadSonglength),
        text(download_status).size(12).color(dl_color),
    ]
    .spacing(6);

    // ── Keyboard shortcuts ───────────────────────────────────────
    let mut kb_col = column![text("Keyboard shortcuts:")
        .size(14)
        .color(Color::from_rgb(0.75, 0.77, 0.82))]
    .spacing(4);
    for (key, desc) in [
        ("Space", "Play / Pause (when search inactive)"),
        ("← →", "Previous / Next track"),
        ("↑ ↓", "Navigate playlist"),
        ("Delete", "Remove selected"),
        ("Ctrl+F", "Focus search"),
    ] {
        kb_col = kb_col.push(
            row![
                text(key)
                    .size(12)
                    .color(Color::from_rgb(0.75, 0.88, 1.0))
                    .width(Length::Fixed(100.0)),
                text(desc).size(12).color(Color::from_rgb(0.65, 0.67, 0.72)),
            ]
            .spacing(8),
        );
    }

    let content = column![
        header,
        rule::horizontal(1),
        engine_col,
        rule::horizontal(1),
        rsid_section,
        rule::horizontal(1),
        stereo_section,
        rule::horizontal(1),
        length_section,
        rule::horizontal(1),
        dl_section,
        rule::horizontal(1),
        kb_col,
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

/// Shared style function for output-engine selector buttons.
/// `is_active` highlights the currently selected engine.
fn engine_btn_style(is_active: bool, st: button::Status) -> button::Style {
    let bg = if is_active {
        match st {
            button::Status::Hovered => Color::from_rgb(0.20, 0.30, 0.45),
            button::Status::Pressed => Color::from_rgb(0.15, 0.22, 0.35),
            _ => Color::from_rgb(0.16, 0.25, 0.40),
        }
    } else {
        match st {
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
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Small utility button used throughout the settings panel and toolbars.
fn tool_button<'a>(label: &'a str, msg: Message) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(msg)
        .padding(Padding::from([4, 10]))
        .style(|_theme: &Theme, st| button::Style {
            background: Some(iced::Background::Color(match st {
                button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                _ => Color::from_rgb(0.18, 0.19, 0.22),
            })),
            text_color: Color::from_rgb(0.8, 0.82, 0.88),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            ..Default::default()
        })
        .into()
}

/// Format a `Duration` as `m:ss` (e.g. `3:07`).
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Filter playlist entries by search query and optional favorites-only mode.
/// Returns indices of matching entries (case-insensitive substring match against
/// title, author, released year, file path, and PSID/RSID type string).
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
            if favorites_only {
                if !entry
                    .md5
                    .as_ref()
                    .map(|m| favorites.is_favorite(m))
                    .unwrap_or(false)
                {
                    return false;
                }
            }
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

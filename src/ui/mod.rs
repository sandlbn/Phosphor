pub mod device_panel;
pub mod font;
pub mod right_click;
pub mod sid_panel;
pub mod visualizer;

use std::path::PathBuf;
use std::time::Duration;

use iced::widget::canvas::{self, Frame, Geometry};
use iced::widget::{
    button, column, container, mouse_area, row, rule, scrollable, text, text_input,
    vertical_slider, Canvas, Column, Space,
};
use iced::{mouse, Alignment, Color, Element, Length, Padding, Point, Rectangle, Size, Theme};

use crate::config::{Config, FavoritesDb};
use crate::player::{PlayState, PlayerStatus};
use crate::playlist::Playlist;
use crate::recently_played::{format_played_at, RecentlyPlayed};
use right_click::RightClickArea;
use visualizer::{TrackerRef, Visualizer};

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
//  Device config snapshot — what we render in the Device tab
// ─────────────────────────────────────────────────────────────────────────────

/// Bundle of everything the Device tab needs from one device round-trip.
/// Produced by the player thread (it owns the SidDevice / Transport) and
/// shipped to the GUI in a `DeviceConfigResult` message.
#[derive(Debug, Clone)]
pub struct DeviceConfigSnapshot {
    pub firmware_version: String,
    pub pcb_version: String,
    pub config: usbsid_pico_config::DeviceConfig,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Messages
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    SessionLoaded(Vec<crate::playlist::PlaylistEntry>),

    // Chained post-processing
    ProcessPendingEntries,
    FinalizePendingEntries,

    // Device config (USBSID-Pico)
    ToggleDeviceConfig,
    DeviceConfigRefresh,
    DeviceConfigApplyPreset(usbsid_pico_config::Preset),
    DeviceConfigSetClock(usbsid_pico_config::ClockRate),
    DeviceConfigEdit(crate::player::DeviceConfigEdit),
    DeviceConfigSave,
    DeviceConfigReset,
    DeviceConfigAutoDetect,
    DeviceConfigResult(Result<DeviceConfigSnapshot, String>),

    // Settings
    ToggleSettings,
    ToggleSkipRsid,
    ToggleForceStereo2sid,
    /// macOS-only: switch USB transport between root bridge daemon and
    /// in-process libusb. Payload is "bridge" or "direct".
    SetMacosUsbMode(String),
    DefaultSongLengthChanged(String),
    BaseFontSizeChanged(String),
    VolumeChanged(f32),
    DownloadSonglength,
    SonglengthDownloaded(Result<PathBuf, String>),
    SetOutputEngine(String),
    SetU64Address(String),
    SetU64Password(String),

    // Remote control
    ToggleHttpRemote,
    HttpRemotePortChanged(String),

    // Favorites
    ToggleFavorite(usize),
    ToggleFavoritesFilter,
    FavoriteNowPlaying,
    ScrollToNowPlaying,

    // File drag & drop
    FileDropped(PathBuf),

    // Window
    WindowResized(iced::window::Id, f32, f32),
    WindowMoved(i32, i32),

    // Visualiser
    /// Toggle between Bar and Scope display modes.
    ToggleVisMode,
    ToggleFavoriteCurrent, // keyboard shortcut H — fav current track
    ShowHelp,
    DismissHelp,
    HvscUpdateAvailable(String), // description string e.g. "HVSC v85 available"
    HvscCheckDone(Result<u32, String>), // remote version result
    Noop,
    /// Raw key events — resolved to context-sensitive actions in update()
    KeyEscape,
    KeyArrowLeft,
    KeyArrowRight,
    ToggleMiniPlayer,
    /// Toggle fullscreen mode (triggered by double-clicking the visualiser).
    ToggleVisFull,
    /// Toggle karaoke fullscreen mode (K key — MUS files with WDS lyrics).
    ToggleKaraoke,

    // Panels
    /// Toggle the SID register info panel (mutually exclusive with settings
    /// and recently played).
    ToggleSidPanel,

    // HVSC browser (two-column Authors | Tunes)
    ToggleHvscBrowser,
    HvscBrowserCategoryChanged(crate::hvsc_browser::HvscCategory),
    HvscBrowserSearchChanged(String),
    HvscBrowserAuthorSelected(usize),
    HvscBrowserAddAllFromAuthor,
    HvscBrowserAddTune(usize),
    HvscBrowserPlayTune(usize),
    /// Play a tune from the flat search index (global, no author selected).
    HvscBrowserPlayFlat(usize),
    /// Add a tune from the flat search index to the playlist.
    HvscBrowserAddFlat(usize),

    // Browse panel: source toggle (Local HVSC vs Assembly64)
    BrowserSourceChanged(crate::hvsc_browser::BrowserSource),

    // Assembly64 browser
    Assembly64QueryChanged(String),
    Assembly64SearchSubmit,
    Assembly64SearchDone(Result<Vec<crate::assembly64::AsmEntry>, String>),
    Assembly64SearchMore,
    Assembly64SearchMoreDone(Result<Vec<crate::assembly64::AsmEntry>, String>),
    /// Background prefetch of one search hit's file list completed.
    /// Used to hide releases with zero playable SIDs.
    Assembly64PrefetchDone(String, Result<Vec<crate::assembly64::AsmFile>, String>),
    /// Toggle expansion of an entry's file list. (item_id, category_id).
    Assembly64ToggleExpand(String, u32),
    Assembly64ExpandDone(String, Result<Vec<crate::assembly64::AsmFile>, String>),
    /// Play a file from an expanded entry. (item_id, category_id, file_id, file_path).
    Assembly64PlayFile(String, u32, u32, String),
    /// Add a file from an expanded entry to the playlist (no play).
    Assembly64AddFile(String, u32, u32, String),
    /// Async download completed. (Result<cached_path, error>, play_after, song).
    Assembly64DownloadDone(Result<std::path::PathBuf, String>, bool, u16),

    // Published playlists (curated M3Us synced from the Phosphor repo)
    PublishedPlaylistsSyncStart,
    PublishedPlaylistsManifestDone(Result<crate::published_playlists::Manifest, String>),
    /// One per-playlist delta download completed.
    PublishedPlaylistsFileDone(String, Result<std::path::PathBuf, String>),
    /// Toggle the inline preview (▾) on a playlist row.
    PublishedPlaylistsToggleExpand(String),
    /// Lightweight parsed preview ready to display.
    PublishedPlaylistsPreviewDone(String, Result<Vec<crate::playlist::PreviewTrack>, String>),
    /// User clicked ▶ Load on a published playlist row.
    PublishedPlaylistsLoad(String),
    PublishedPlaylistsLoadDone(Result<(String, Vec<crate::playlist::PlaylistEntry>), String>),
    /// Background SID-header read + md5 + songlength enrichment for a
    /// just-loaded published playlist. Carries (source_file, enriched).
    /// We compare source_file against current session_mode before
    /// applying — if the user switched playlists mid-flight, drop it.
    PublishedPlaylistsEnrichDone(String, Vec<crate::playlist::PlaylistEntry>),
    /// User clicked "↺ Restore my playlist" while a published playlist is active.
    PublishedPlaylistsRestoreDefault,
    PublishedPlaylistsRestoreDone(Vec<crate::playlist::PlaylistEntry>),

    // Version check
    VersionCheckDone(Result<Option<crate::version_check::NewVersionInfo>, String>),
    OpenUpdateUrl,

    // U64 audio streaming
    ToggleU64Audio,
    U64AudioPortChanged(String),

    // STIL info overlay
    ShowStilOverlay,
    DismissStilOverlay,

    // STIL settings
    DownloadStil,
    StilDownloaded(Result<std::path::PathBuf, String>),
    LoadStil,
    StilFileChosen(Option<std::path::PathBuf>),
    HvscRootChanged(String),
    SetHvscRoot(String),

    // HVSC rsync (pulls the full tune tree)
    HvscRsyncUrlChanged(String),
    HvscRsyncStart,
    HvscRsyncCancel,
    /// Per-Tick drain — UI consumes the queued progress events here.
    HvscRsyncPoll,

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
    tracker: Option<TrackerRef<'a>>,
    is_now_playing_favorite: bool,
    has_track: bool,
    has_stil_info: bool,
    window_width: f32,
    engine_name: &str,
    master_volume: f32,
    // Optional engine-specific suffix appended to the engine label (e.g.
    // "2× MOS8580" for USB). Caller-formatted so this module stays free
    // of device-config types.
    engine_suffix: Option<&str>,
) -> Element<'a, Message> {
    let compact = window_width < 760.0;
    let title_size = if compact { 15.0_f32 } else { 18.0 };
    let author_size = if compact { 12.0_f32 } else { 14.0 };
    let extra_size = if compact { 10.0_f32 } else { 12.0 };
    let vis_width = if compact { 200.0_f32 } else { 300.0 };
    let vis_height = if compact { 48.0_f32 } else { 60.0 };

    let engine_base: &str = match engine_name {
        "usb" => "USB Hardware (USBSID-Pico)",
        "emulated" => "Software Emulation (reSID)",
        "sidlite" => "SIDLite Emulation (libsidplayfp)",
        "u64" => "Ultimate 64 (Network)",
        "auto" => "Auto",
        other => other,
    };
    let engine_label: String = match engine_suffix {
        Some(s) if !s.is_empty() => format!("{engine_base} — {s}"),
        _ => engine_base.to_string(),
    };

    let (title, author, extra) = match &status.track_info {
        Some(info) => {
            let format_label = if info
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mus"))
                .unwrap_or(false)
            {
                "MUS"
            } else if info.is_rsid {
                "RSID"
            } else {
                "PSID"
            };
            let chip_label = match info.sid_model {
                1 => "  •  MOS6581",
                2 => "  •  MOS8580",
                3 => "  •  MOS6581/8580",
                _ => "",
            };
            (
                info.name.as_str(),
                info.author.as_str(),
                format!(
                    "{}  •  {}{}  •  Song {}/{}  •  {}  •  {} writes/frame",
                    format_label,
                    info.sid_type,
                    chip_label,
                    info.current_song,
                    info.songs,
                    if info.is_pal { "PAL" } else { "NTSC" },
                    status.writes_per_frame,
                ),
            )
        }
        None => ("No track loaded", "—", String::new()),
    };

    let state_icon = match status.state {
        PlayState::Playing => "▶",
        PlayState::Paused => "❚❚",
        PlayState::Stopped => "■",
    };

    let mut info_col = column![
        text(format!("{state_icon}  {title}")).size(font::sized(title_size)),
        text(author)
            .size(font::sized(author_size))
            .color(Color::from_rgb(0.6, 0.7, 0.8)),
        text(extra)
            .size(font::sized(extra_size))
            .color(Color::from_rgb(0.5, 0.5, 0.6)),
        row![
            text(format!("Engine: {engine_label}"))
                .size(font::sized(extra_size))
                .color(Color::from_rgb(0.4, 0.55, 0.45)),
            if !status.device_connected {
                row![
                    Space::new().width(Length::Fixed(8.0)),
                    text("• Disconnected")
                        .size(font::sized(extra_size))
                        .color(Color::from_rgb(1.0, 0.35, 0.35)),
                ]
                .into()
            } else {
                Element::from(Space::new().width(Length::Shrink))
            },
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(2)
    .width(Length::Fill);

    if let Some(ref err) = status.error {
        info_col = info_col.push(
            text(format!("⚠ {err}"))
                .size(font::sized(12.0))
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
        let heart_btn = button(text(heart_label).size(font::sized(18.0)).color(heart_color))
            .on_press(Message::FavoriteNowPlaying)
            .padding(Padding::from([4, 6]))
            .style(|_theme: &Theme, _status| button::Style {
                background: None,
                text_color: Color::WHITE,
                ..Default::default()
            });
        let scroll_btn = button(
            text("⌖")
                .size(font::sized(16.0))
                .color(Color::from_rgb(0.5, 0.5, 0.6)),
        )
        .on_press(Message::ScrollToNowPlaying)
        .padding(Padding::from([4, 6]))
        .style(|_theme: &Theme, _status| button::Style {
            background: None,
            text_color: Color::WHITE,
            ..Default::default()
        });
        let info_btn = button(text("ⓘ").size(font::sized(15.0)).color(if has_stil_info {
            Color::from_rgb(0.45, 0.75, 1.0)
        } else {
            Color::from_rgb(0.30, 0.30, 0.40)
        }))
        .on_press(if has_stil_info {
            Message::ShowStilOverlay
        } else {
            Message::None
        })
        .padding(Padding::from([4, 6]))
        .style(|_theme: &Theme, _status| button::Style {
            background: None,
            ..Default::default()
        });
        column![heart_btn, scroll_btn, info_btn]
            .spacing(0)
            .align_x(Alignment::Center)
            .into()
    } else {
        column![].into()
    };

    // Volume control on the right of the visualizer — vertical slider, full
    // visualiser height. USB engine is analog-out so we can't scale it from
    // host; show a static "HW" label instead with a muted tooltip-ish hint.
    let volume_icon = if master_volume <= 0.001 {
        "🔇"
    } else if master_volume < 0.5 {
        "🔉"
    } else {
        "🔊"
    };
    let volume_block: Element<'a, Message> = if engine_name == "usb" {
        column![
            text("🔊")
                .size(font::sized(extra_size))
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
            text("HW")
                .size(font::sized(extra_size))
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        ]
        .spacing(2)
        .align_x(Alignment::Center)
        .into()
    } else {
        column![
            text(volume_icon).size(font::sized(extra_size)),
            vertical_slider(0.0..=1.0, master_volume, Message::VolumeChanged)
                .step(0.01)
                .height(Length::Fixed(vis_height - 18.0)),
        ]
        .spacing(2)
        .align_x(Alignment::Center)
        .into()
    };

    let content = row![
        info_col,
        now_playing_buttons,
        container(visualizer.view(tracker))
            .width(Length::Fixed(vis_width))
            .height(Length::Fixed(vis_height)),
        volume_block,
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
        .size(font::sized(11.0))
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
        button(text(label).size(font::sized(btn_size)))
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

    // Accent variant for the Library entry-point: deep teal background +
    // brighter border so the "browse content" affordance stands out from
    // the surrounding utility toggles (recent/sid/device/settings).
    let accent_button = |label: &'a str, msg: Message| -> Element<'a, Message> {
        button(text(label).size(font::sized(btn_size)))
            .on_press(msg)
            .padding(Padding::from([btn_pad, if compact { 8 } else { 12 }]))
            .style(|_theme: &Theme, st| {
                let bg = match st {
                    button::Status::Hovered => Color::from_rgb(0.22, 0.32, 0.42),
                    button::Status::Pressed => Color::from_rgb(0.12, 0.18, 0.24),
                    _ => Color::from_rgb(0.16, 0.22, 0.32),
                };
                button::Style {
                    background: Some(iced::Background::Color(bg)),
                    text_color: Color::from_rgb(0.92, 0.96, 1.0),
                    border: iced::Border {
                        radius: 3.0.into(),
                        width: 1.0,
                        color: Color::from_rgb(0.40, 0.55, 0.70),
                    },
                    ..Default::default()
                }
            })
            .into()
    };

    let sep = || -> Element<'a, Message> {
        // Vertical-rule separator. Sized to match the buttons so groups
        // visually align. Slightly muted colour so it reads as a divider
        // rather than competing with the buttons themselves.
        container(
            Space::new()
                .width(Length::Fixed(1.0))
                .height(Length::Fixed(if compact { 16.0 } else { 20.0 })),
        )
        .padding(Padding::from([0, 4]))
        .style(|_t: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.30, 0.32, 0.36))),
            ..Default::default()
        })
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
        button(text(if compact { "🕐" } else { "🕐 Recent" }).size(font::sized(btn_size)))
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
        button(text(if compact { "SID" } else { "SID" }).size(font::sized(btn_size)))
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

    // File-ops sub-group (add to playlist, open/save/clear).
    let file_ops = if compact {
        row![
            small_button("➕", Message::AddFiles),
            small_button("📁", Message::AddFolder),
            small_button("📂", Message::LoadPlaylist),
            small_button("💾", Message::SavePlaylist),
            small_button("🗑", Message::ClearPlaylist),
        ]
        .spacing(3)
    } else {
        row![
            small_button("➕ Files", Message::AddFiles),
            small_button("📁 Folder", Message::AddFolder),
            small_button("📂 Open", Message::LoadPlaylist),
            small_button("💾 Save", Message::SavePlaylist),
            small_button("🗑 Clear", Message::ClearPlaylist),
        ]
        .spacing(4)
    };

    // Library entry-point in its own group so the accented styling reads
    // as a content-discovery affordance separate from the system toggles.
    let library_group: Element<'a, Message> = if compact {
        accent_button("📚", Message::ToggleHvscBrowser)
    } else {
        accent_button("📚 Library", Message::ToggleHvscBrowser)
    };

    // Panel-toggles sub-group (history / SID panel / device config / settings).
    let panel_toggles = if compact {
        row![
            recent_btn,
            sid_btn,
            small_button("🔧", Message::ToggleDeviceConfig),
            small_button("⚙", Message::ToggleSettings),
        ]
        .spacing(3)
    } else {
        row![
            recent_btn,
            sid_btn,
            small_button("🔧 Device", Message::ToggleDeviceConfig),
            small_button("⚙ Settings", Message::ToggleSettings),
        ]
        .spacing(4)
    };

    let update_badge = |version: &str| -> Element<'a, Message> {
        button(
            text(format!("⬆ {version}"))
                .size(font::sized(if compact { 11.0 } else { 12.0 }))
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

    // Two rows, always — same logical split wide or narrow.
    //   Row 1: PLAYBACK (transport ┃ subtune ┃ mode)
    //   Row 2: LIBRARY  (file ops ┃ panel toggles)  + optional update badge
    let row_spacing = if compact { 6 } else { 8 };
    let top_row = row![transport, sep(), subtune_controls, sep(), mode_controls]
        .spacing(row_spacing)
        .align_y(Alignment::Center);

    let mut bottom_row = row![
        file_ops,
        sep(),
        library_group,
        sep(),
        panel_toggles,
        Space::new().width(Length::Fill)
    ]
    .spacing(row_spacing)
    .align_y(Alignment::Center);
    if let Some(info) = new_version {
        bottom_row = bottom_row.push(update_badge(&info.version));
    }

    let bar: Element<'a, Message> = column![top_row, bottom_row]
        .spacing(if compact { 4 } else { 6 })
        .padding(Padding::from([bar_pad, if compact { 12 } else { 16 }]))
        .into();

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
        .size(font::sized(13.0))
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

    let fav_btn = button(text(fav_label).size(font::sized(12.0)))
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
        text("🔍 ")
            .size(font::sized(13.0))
            .color(Color::from_rgb(0.5, 0.5, 0.6)),
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
            text(count_text).size(font::sized(12.0)).color(count_color)
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
    loading_text: &str,
    tick: u32,
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
        button(text(display).size(font::sized(11.0)).color(text_color))
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
                .size(font::sized(11.0))
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
        if !loading_text.is_empty() {
            // ── Demoscene-style loading scroller (Canvas) ────────────────
            // Uses the same chunky C64 pixel font and sine-wave colour
            // cycling as the expanded visualiser.
            let loading_owned = loading_text.to_string();
            let scroller = Canvas::new(LoadingScroller {
                text: loading_owned,
                tick,
            })
            .width(Length::Fill)
            .height(Length::Fixed(120.0));
            rows = rows.push(
                container(scroller)
                    .width(Length::Fill)
                    .padding(Padding::from([40, 0])),
            );
        } else {
            let msg = if playlist.is_empty() {
                "Drag .sid files here or click \"+ Files\" / \"+ Folder\""
            } else {
                "No matching tracks"
            };
            rows = rows.push(
                container(
                    text(msg)
                        .size(font::sized(14.0))
                        .color(Color::from_rgb(0.4, 0.4, 0.5)),
                )
                .padding(40)
                .center_x(Length::Fill),
            );
        }
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
        button(text(icon_label).size(font::sized(13.0)))
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

    // Transparent full-screen dismiss area — captures clicks but NOT scroll events,
    // so the playlist underneath can still be scrolled while the menu is open.
    // Using mouse_area instead of button means wheel events fall through to the
    // scrollable, keeping playlist_scroll_offset_y in sync with the visual position.
    // on_press only — no on_right_press. If we also dismissed on right-press,
    // the same ButtonReleased(Right) that opened the menu would immediately
    // close it again via this backdrop in the same event dispatch cycle.
    let dismiss = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::DismissContextMenu);

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
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::Fixed(40.0)),
        text("Title")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(4)),
        text("Author")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(3)),
        text("Released")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.5, 0.5, 0.6))
            .width(Length::FillPortion(2)),
        text("Played")
            .size(font::sized(11.0))
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
                .size(font::sized(12.0))
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
                    .size(font::sized(14.0))
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
                    .size(font::sized(13.0))
                    .color(color)
                    .width(Length::Fixed(40.0)),
                text(entry.title.clone())
                    .size(font::sized(13.0))
                    .color(color)
                    .width(Length::FillPortion(4)),
                text(entry.author.clone())
                    .size(font::sized(13.0))
                    .color(color)
                    .width(Length::FillPortion(3)),
                text(entry.released.clone())
                    .size(font::sized(13.0))
                    .color(color)
                    .width(Length::FillPortion(2)),
                text(format_played_at(entry.played_at))
                    .size(font::sized(12.0))
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
    let is_mus = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("mus"))
        .unwrap_or(false);
    let type_label = if is_mus {
        "MUS"
    } else if entry.is_rsid {
        "RSID"
    } else {
        "PSID"
    }
    .to_string();
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

    let heart_btn = button(text(heart_label).size(font::sized(13.0)).color(heart_color))
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
        entry.has_wds,
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
    has_wds: bool,
    author: String,
    released: String,
    time: String,
    sid_type: String,
    sids: String,
    is_current: bool,
) -> Element<'a, Message> {
    let size: f32 = 13.0;
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

    let title_cell: Element<'a, Message> = if has_wds {
        row![
            text(title).size(font::sized(size)).color(color),
            text(" (Karaoke)")
                .size(font::sized(10.0))
                .color(Color::from_rgb(0.30, 0.75, 0.45)),
        ]
        .spacing(4)
        .width(Length::FillPortion(4))
        .into()
    } else {
        text(title)
            .size(font::sized(size))
            .color(color)
            .width(Length::FillPortion(4))
            .into()
    };

    row![
        text(format!("{indicator}{num:>3}"))
            .size(font::sized(size))
            .color(color)
            .width(Length::Fixed(50.0)),
        title_cell,
        text(author)
            .size(font::sized(size))
            .color(color)
            .width(Length::FillPortion(3)),
        text(released)
            .size(font::sized(size))
            .color(color)
            .width(Length::FillPortion(2)),
        text(time)
            .size(font::sized(size))
            .color(color)
            .width(Length::Fixed(55.0)),
        text(sid_type)
            .size(font::sized(size))
            .color(type_color)
            .width(Length::Fixed(42.0)),
        text(sids)
            .size(font::sized(size))
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
/// Dispatcher for the Browse panel — renders a source toggle at the top
/// (Local HVSC | Assembly64) and the appropriate sub-view below it.
pub fn browser_view<'a>(
    source: crate::hvsc_browser::BrowserSource,
    hvsc: &'a crate::hvsc_browser::HvscBrowser,
    a64: &'a crate::assembly64_browser::Assembly64Browser,
    pub_pls: &'a crate::published_playlists_browser::PublishedPlaylistsBrowser,
    hvsc_root_known: bool,
    hvsc_update_available: bool,
    hvsc_sync_in_progress: bool,
    hvsc_sync_status: &'a str,
    session_mode: &'a crate::SessionMode,
) -> Element<'a, Message> {
    use crate::hvsc_browser::BrowserSource;

    let source_btn = |s: BrowserSource| -> Element<'a, Message> {
        let active = source == s;
        let label = if active {
            format!("✓ {}", s.label())
        } else {
            s.label().to_string()
        };
        tool_button(
            Box::leak(label.into_boxed_str()),
            Message::BrowserSourceChanged(s),
        )
    };

    let header = container(
        row![
            text("Source:")
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
            source_btn(BrowserSource::LocalHvsc),
            source_btn(BrowserSource::Assembly64),
            source_btn(BrowserSource::PublishedPlaylists),
            Space::new().width(Length::Fill),
        ]
        .spacing(6)
        .padding(Padding::from([6, 12]))
        .align_y(Alignment::Center),
    )
    .style(|_t: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.11, 0.12, 0.14))),
        ..Default::default()
    });

    let body: Element<'a, Message> = match source {
        BrowserSource::LocalHvsc => hvsc_browser_view(
            hvsc,
            hvsc_update_available,
            hvsc_sync_in_progress,
            hvsc_sync_status,
        ),
        BrowserSource::Assembly64 => assembly64_browser_view(a64),
        BrowserSource::PublishedPlaylists => {
            published_playlists_view(pub_pls, hvsc_root_known, session_mode)
        }
    };

    container(column![header, rule::horizontal(1), body])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Assembly64 browser — search bar on top, results list below, each
/// result expandable to show its `.sid` files.
pub fn assembly64_browser_view<'a>(
    a64: &'a crate::assembly64_browser::Assembly64Browser,
) -> Element<'a, Message> {
    use crate::assembly64_browser::ExpansionState;

    // ── Search bar + status line ──────────────────────────────────────────
    let search_input = text_input(
        r#"AQL query, e.g. name:"commando"  or  handle:"hubbard" category:music"#,
        a64.query(),
    )
    .on_input(Message::Assembly64QueryChanged)
    .on_submit(Message::Assembly64SearchSubmit)
    .size(font::sized(12.0))
    .padding(Padding::from([6, 10]))
    .width(Length::Fill)
    .style(|_t: &Theme, _st| text_input::Style {
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

    let status_text: Element<'a, Message> = if let Some(err) = a64.last_error() {
        text(format!("⚠ {err}"))
            .size(font::sized(12.0))
            .color(Color::from_rgb(1.0, 0.45, 0.45))
            .into()
    } else if a64.search_in_flight() {
        text("Searching…")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    } else if !a64.results_query().is_empty() {
        let total = a64.results().len();
        let hidden = a64
            .results()
            .iter()
            .filter(|e| a64.is_hidden(&e.id))
            .count();
        let visible = total - hidden;
        let pending = a64.prefetch_pending();
        let msg = if pending > 0 {
            format!("{visible}/{total} releases with playable SIDs — checking {pending} more…")
        } else if hidden > 0 {
            format!("{visible} releases with playable SIDs ({hidden} hidden — no .sid files)")
        } else {
            format!("{visible} releases with playable SIDs")
        };
        text(msg)
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    } else {
        text("Press ENTER or click 🔎 to search.")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.55))
            .into()
    };

    let search_row = row![
        search_input,
        tool_button("🔎 Search", Message::Assembly64SearchSubmit),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    // ── Results list ──────────────────────────────────────────────────────
    let mut results_col: Column<'a, Message> = column![].spacing(2);

    if a64.results().is_empty() && !a64.search_in_flight() && a64.last_error().is_none() {
        results_col = results_col.push(
            container(
                text(
                    r#"Search Assembly64 for SID releases.
Bare terms (e.g. commando) are filtered to category:music.
For full AQL, include a colon — e.g. handle:"hubbard",
group:Hubbard year:1985, or category:demos commando."#,
                )
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
            )
            .padding(Padding::from([20, 24])),
        );
    }

    for entry in a64.results() {
        if a64.is_hidden(&entry.id) {
            continue;
        }
        let expanded = a64.expansion(&entry.id).is_some();
        let chev = if expanded { "▾" } else { "▸" };
        let toggle = tool_button(
            Box::leak(format!("{} {}", chev, entry.name).into_boxed_str()),
            Message::Assembly64ToggleExpand(entry.id.clone(), entry.category),
        );

        let mut sub_parts: Vec<String> = Vec::new();
        if !entry.handle.is_empty() {
            sub_parts.push(entry.handle.clone());
        }
        if !entry.group.is_empty() {
            sub_parts.push(format!("/{}", entry.group));
        }
        if entry.year > 0 {
            sub_parts.push(format!("· {}", entry.year));
        }
        if entry.rating > 0 {
            sub_parts.push(format!("· ★ {}", entry.rating));
        }
        let subline = sub_parts.join(" ");

        let entry_block = column![
            toggle,
            text(subline)
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
        ]
        .spacing(2);

        results_col = results_col.push(
            container(entry_block)
                .padding(Padding::from([4, 10]))
                .width(Length::Fill),
        );

        if expanded {
            let exp = a64.expansion(&entry.id).unwrap();
            let sub: Element<'a, Message> = match exp {
                ExpansionState::Loading => text("  Loading files…")
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .into(),
                ExpansionState::Failed(msg) => text(format!("  ⚠ {msg}"))
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(1.0, 0.45, 0.45))
                    .into(),
                ExpansionState::Loaded(files) => {
                    let sid_files: Vec<&crate::assembly64::AsmFile> =
                        files.iter().filter(|f| f.is_sid()).collect();
                    if sid_files.is_empty() {
                        text(format!(
                            "  No .sid files in this release ({} other files).",
                            files.len()
                        ))
                        .size(font::sized(12.0))
                        .color(Color::from_rgb(0.55, 0.57, 0.62))
                        .into()
                    } else {
                        let mut sub_col: Column<'a, Message> = column![].spacing(1);
                        for f in sid_files {
                            sub_col = sub_col.push(
                                row![
                                    Space::new().width(Length::Fixed(24.0)),
                                    text(&f.path)
                                        .size(font::sized(12.0))
                                        .color(Color::from_rgb(0.85, 0.87, 0.9))
                                        .width(Length::Fill)
                                        .wrapping(text::Wrapping::None),
                                    text(format!("{} B", f.size))
                                        .size(font::sized(11.0))
                                        .color(Color::from_rgb(0.55, 0.57, 0.62))
                                        .width(Length::Fixed(80.0)),
                                    tool_button(
                                        "▶",
                                        Message::Assembly64PlayFile(
                                            entry.id.clone(),
                                            entry.category,
                                            f.id,
                                            f.path.clone(),
                                        ),
                                    ),
                                    Space::new().width(Length::Fixed(4.0)),
                                    tool_button(
                                        "➕",
                                        Message::Assembly64AddFile(
                                            entry.id.clone(),
                                            entry.category,
                                            f.id,
                                            f.path.clone(),
                                        ),
                                    ),
                                ]
                                .padding(Padding::from([2, 10]))
                                .spacing(8)
                                .align_y(Alignment::Center),
                            );
                        }
                        sub_col.into()
                    }
                }
            };
            results_col = results_col.push(sub);
        }
        results_col = results_col.push(rule::horizontal(1));
    }

    if a64.more_available() {
        results_col = results_col.push(
            container(tool_button("⬇ Load more", Message::Assembly64SearchMore))
                .padding(Padding::from([8, 12])),
        );
    }

    let body = column![
        search_row,
        status_text,
        scrollable(results_col).height(Length::Fill),
    ]
    .spacing(6)
    .padding(Padding::from([8, 8]));

    // Footer: Close
    let footer = container(
        row![
            Space::new().width(Length::Fill),
            tool_button("✕ Close", Message::ToggleHvscBrowser),
        ]
        .padding(Padding::from([6, 12]))
        .align_y(Alignment::Center),
    );

    container(column![body, rule::horizontal(1), footer])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

/// Published Playlists view: list of curated M3Us synced from the
/// phosphor GitHub repo, with ▶ Load and ▾ Preview affordances.
pub fn published_playlists_view<'a>(
    b: &'a crate::published_playlists_browser::PublishedPlaylistsBrowser,
    hvsc_root_known: bool,
    session_mode: &'a crate::SessionMode,
) -> Element<'a, Message> {
    use crate::published_playlists_browser::PreviewState;
    use crate::SessionMode;

    // ── Header: Sync button + status line ──────────────────────────
    let pending = b.download_pending();
    let last_synced_label = b.last_synced_unix().map(format_relative_time);

    let status_text: Element<'a, Message> = if let Some(err) = b.last_error() {
        text(format!("⚠ Sync failed: {err}"))
            .size(font::sized(12.0))
            .color(Color::from_rgb(1.0, 0.45, 0.45))
            .into()
    } else if b.sync_in_flight() {
        text("Syncing manifest…")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    } else if pending > 0 {
        let total = b.playlists().len();
        text(format!("Updating {pending} of {total} playlists…"))
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    } else if let Some(ago) = last_synced_label {
        text(format!("Last synced: {ago}"))
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    } else {
        text("Click Sync to fetch published playlists from GitHub.")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into()
    };

    let header_row = row![
        tool_button("⟳ Sync now", Message::PublishedPlaylistsSyncStart),
        status_text,
        Space::new().width(Length::Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // ── Active-banner ──────────────────────────────────────────────
    let active_banner: Option<Element<'a, Message>> = match session_mode {
        SessionMode::PublishedReadOnly { file } => {
            let name = b
                .meta_for(file)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| file.clone());
            Some(
                container(
                    row![
                        text(format!("📌 Playing: {name}"))
                            .size(font::sized(12.0))
                            .color(Color::from_rgb(0.85, 0.87, 0.9)),
                        Space::new().width(Length::Fill),
                        tool_button(
                            "↺ Restore my playlist",
                            Message::PublishedPlaylistsRestoreDefault,
                        ),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding(Padding::from([6, 10]))
                .style(|_t: &Theme| container::Style {
                    background: Some(iced::Background::Color(Color::from_rgb(0.16, 0.13, 0.20))),
                    border: iced::Border {
                        radius: 3.0.into(),
                        width: 1.0,
                        color: Color::from_rgb(0.35, 0.28, 0.45),
                    },
                    ..Default::default()
                })
                .into(),
            )
        }
        SessionMode::Default => None,
    };

    // ── HVSC root warning ─────────────────────────────────────────
    let hvsc_warning: Option<Element<'a, Message>> = if hvsc_root_known {
        None
    } else {
        Some(
            container(
                text("⚠ Configure HVSC root in Settings before loading playlists — these reference HVSC paths.")
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.95, 0.80, 0.40)),
            )
            .padding(Padding::from([6, 10]))
            .style(|_t: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.14, 0.08))),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.40, 0.30, 0.15),
                },
                ..Default::default()
            })
            .into(),
        )
    };

    // ── Playlist rows ─────────────────────────────────────────────
    let mut list_col: Column<'a, Message> = column![].spacing(2);

    if b.playlists().is_empty() && !b.sync_in_flight() && b.last_error().is_none() {
        list_col = list_col.push(
            container(
                text("No playlists yet. Click ⟳ Sync now to fetch the latest set.")
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62)),
            )
            .padding(Padding::from([20, 24])),
        );
    }

    for meta in b.playlists() {
        let expanded = b.is_expanded(&meta.file);
        let chev = if expanded { "▾" } else { "▸" };
        let toggle = tool_button(
            Box::leak(format!("{chev} {}", meta.name).into_boxed_str()),
            Message::PublishedPlaylistsToggleExpand(meta.file.clone()),
        );

        let load_msg = Message::PublishedPlaylistsLoad(meta.file.clone());
        let load_btn: Element<'a, Message> = if hvsc_root_known {
            tool_button("▶ Load", load_msg)
        } else {
            // Disabled button: render as a dim label so users see it's intentional.
            container(
                text("▶ Load (set HVSC root)")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.45, 0.47, 0.55)),
            )
            .padding(Padding::from([4, 8]))
            .into()
        };

        let top_row = row![toggle, Space::new().width(Length::Fill), load_btn]
            .spacing(6)
            .align_y(Alignment::Center);

        let sub_parts = {
            let mut parts: Vec<String> = Vec::new();
            if !meta.description.is_empty() {
                parts.push(meta.description.clone());
            }
            parts.push(format!("({} tracks)", meta.tracks));
            parts.join(" · ")
        };

        let mut entry_col = column![
            top_row,
            text(sub_parts)
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
        ]
        .spacing(2)
        .width(Length::Fill);

        if expanded {
            let preview_block: Element<'a, Message> = match b.preview(&meta.file) {
                Some(PreviewState::Loading) => text("Loading preview…")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .into(),
                Some(PreviewState::Failed(msg)) => text(format!("Preview failed: {msg}"))
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(1.0, 0.45, 0.45))
                    .into(),
                Some(PreviewState::Ready(tracks)) => {
                    let mut col: Column<'a, Message> = column![].spacing(0);
                    for t in tracks {
                        let dur = t
                            .duration_secs
                            .map(|s| format!("  [{}:{:02}]", s / 60, s % 60))
                            .unwrap_or_default();
                        let author = t
                            .author
                            .as_ref()
                            .map(|a| format!("{} — ", a.replace('_', " ")))
                            .unwrap_or_default();
                        let title = t.title.replace('_', " ");
                        col = col.push(
                            text(format!("• {author}{title}{dur}"))
                                .size(font::sized(11.0))
                                .line_height(iced::widget::text::LineHeight::Absolute(
                                    iced::Pixels(14.0),
                                ))
                                .color(Color::from_rgb(0.80, 0.82, 0.85))
                                .width(Length::Fill),
                        );
                    }
                    scrollable(col.width(Length::Fill))
                        .width(Length::Fill)
                        .height(Length::Fixed(240.0))
                        .into()
                }
                None => text("Loading preview…")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .into(),
            };
            entry_col = entry_col.push(
                container(preview_block)
                    .padding(Padding {
                        top: 4.0,
                        right: 0.0,
                        bottom: 4.0,
                        left: 18.0,
                    })
                    .width(Length::Fill),
            );
        }

        // Right padding leaves clearance for the scrollable's vertical
        // scrollbar — otherwise the ▶ Load button gets clipped behind it.
        list_col = list_col.push(container(entry_col).width(Length::Fill).padding(Padding {
            top: 0.0,
            right: 16.0,
            bottom: 0.0,
            left: 0.0,
        }));
        list_col = list_col.push(rule::horizontal(1));
    }

    let mut body = column![header_row]
        .spacing(8)
        .padding(Padding::from([8, 8]));
    if let Some(b) = active_banner {
        body = body.push(b);
    }
    if let Some(w) = hvsc_warning {
        body = body.push(w);
    }
    body = body.push(scrollable(list_col).height(Length::Fill));

    let footer = container(
        row![
            Space::new().width(Length::Fill),
            tool_button("✕ Close", Message::ToggleHvscBrowser),
        ]
        .padding(Padding::from([6, 12]))
        .align_y(Alignment::Center),
    );

    container(column![body, rule::horizontal(1), footer])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

fn format_relative_time(unix_secs: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix_secs);
    let delta = (now - unix_secs).max(0);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        format!("{} min ago", delta / 60)
    } else if delta < 86400 {
        format!("{} h ago", delta / 3600)
    } else {
        format!("{} days ago", delta / 86400)
    }
}

/// Two-column HVSC browser panel. Left: author list (alphabetical, sticky
/// letter headers). Right: tunes belonging to the selected author. Footer
/// has Add-all + category segmented control + Close.
pub fn hvsc_browser_view<'a>(
    browser: &'a crate::hvsc_browser::HvscBrowser,
    update_available: bool,
    sync_in_progress: bool,
    sync_status: &'a str,
) -> Element<'a, Message> {
    use crate::hvsc_browser::HvscCategory;

    // Sync row: button (or progress) + status text. Reused in the empty
    // state and as a top banner when an update is available.
    let sync_button: Element<'a, Message> = if sync_in_progress {
        container(
            text(if sync_status.is_empty() {
                "Syncing…".to_string()
            } else {
                format!("Syncing… {sync_status}")
            })
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
        )
        .padding(Padding::from([4, 8]))
        .into()
    } else {
        tool_button("⬇ Sync HVSC now", Message::HvscRsyncStart)
    };

    // ── Empty state: no hvsc_root set ──────────────────────────────────────
    if browser.is_empty_state() {
        let body = column![
            text("HVSC browser")
                .size(font::sized(22.0))
                .color(Color::from_rgb(0.85, 0.87, 0.9)),
            text("No HVSC tree found.")
                .size(font::sized(14.0))
                .color(Color::from_rgb(0.75, 0.77, 0.82)),
            text(
                "Sync the High Voltage SID Collection to start browsing. \
                 Or point Settings → HVSC root at an existing copy."
            )
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62)),
            row![
                sync_button,
                tool_button("⚙ Open Settings", Message::ToggleSettings),
                tool_button("✕ Close", Message::ToggleHvscBrowser),
            ]
            .spacing(8),
        ]
        .spacing(10)
        .padding(Padding::from([24, 24]));
        return container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_t: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
                ..Default::default()
            })
            .into();
    }

    // ── Update-available banner (only when sync is NOT already running) ────
    let update_banner: Option<Element<'a, Message>> = if update_available || sync_in_progress {
        let label: Element<'a, Message> = if sync_in_progress {
            text(if sync_status.is_empty() {
                "Syncing HVSC…".to_string()
            } else {
                format!("Syncing HVSC… {sync_status}")
            })
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9))
            .into()
        } else {
            text("🆕 New HVSC release available")
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.85, 0.87, 0.9))
                .into()
        };
        Some(
            container(
                row![label, Space::new().width(Length::Fill), sync_button]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .padding(Padding::from([6, 10]))
            .style(|_t: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.13, 0.16, 0.20))),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.25, 0.35, 0.45),
                },
                ..Default::default()
            })
            .into(),
        )
    } else {
        None
    };

    // ── Left column: search + author list ──────────────────────────────────
    let search_input = text_input("Search authors / tunes", browser.search())
        .on_input(Message::HvscBrowserSearchChanged)
        .size(font::sized(12.0))
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .style(|_theme: &Theme, _st| text_input::Style {
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

    let mut author_col: Column<'a, Message> = column![].spacing(1);
    let mut last_letter: Option<char> = None;
    let filtered_authors = browser.filtered_authors();
    let total_authors = browser.authors().len();
    for &idx in &filtered_authors {
        let a = &browser.authors()[idx];
        // Sticky-ish letter header (just an inline divider row).
        if Some(a.letter) != last_letter {
            last_letter = Some(a.letter);
            author_col = author_col.push(
                container(
                    text(a.letter.to_string())
                        .size(font::sized(11.0))
                        .color(Color::from_rgb(0.45, 0.47, 0.55)),
                )
                .padding(Padding::from([4, 10])),
            );
        }
        let is_selected = browser.selected_author_idx() == Some(idx);
        let row_bg = if is_selected {
            Color::from_rgb(0.20, 0.25, 0.35)
        } else {
            Color::from_rgba(0.0, 0.0, 0.0, 0.0)
        };
        let label = button(
            text(&a.display_name)
                .size(font::sized(13.0))
                .color(Color::from_rgb(0.85, 0.87, 0.9)),
        )
        .on_press(Message::HvscBrowserAuthorSelected(idx))
        .padding(Padding::from([4, 12]))
        .width(Length::Fill)
        .style(move |_t: &Theme, st| button::Style {
            background: Some(iced::Background::Color(match st {
                button::Status::Hovered => Color::from_rgb(0.16, 0.18, 0.22),
                _ => row_bg,
            })),
            text_color: Color::from_rgb(0.85, 0.87, 0.9),
            border: iced::Border::default(),
            ..Default::default()
        });
        author_col = author_col.push(label);
    }

    // Label depends on category — DEMOS/GAMES list "sections" (0-9, A-F,
    // Commodore, …), MUSICIANS lists "authors".
    let unit_label = match browser.category() {
        crate::hvsc_browser::HvscCategory::Musicians => "authors",
        _ => "sections",
    };
    let author_count_label = if filtered_authors.len() == total_authors {
        format!("{} {}", total_authors, unit_label)
    } else {
        format!(
            "{} / {} {}",
            filtered_authors.len(),
            total_authors,
            unit_label
        )
    };

    let left_col = column![
        search_input,
        text(author_count_label)
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.55)),
        scrollable(author_col).height(Length::Fill),
    ]
    .spacing(6)
    .padding(Padding::from([8, 8]))
    .width(Length::Fixed(320.0));

    // When the search box has text, prefer the flat global tune search
    // over the per-author view. This is the "search song doesn't work"
    // path the user hit: typing a song name without first selecting an
    // author should still surface matches.
    let has_search = !browser.search().trim().is_empty();
    let flat_results: Vec<usize> = if has_search {
        browser.filtered_flat()
    } else {
        Vec::new()
    };
    let show_flat_results = has_search && !flat_results.is_empty();

    // ── Right column: tune list ─────────────────────────────────────────────
    let right_header: Element<'a, Message> = if show_flat_results {
        let total = browser.flat_index().len();
        let label = if browser.flat_index_loaded() {
            format!(
                "{} matches across all (showing up to 500 of {})",
                flat_results.len(),
                total
            )
        } else {
            "Indexing tunes…".to_string()
        };
        row![
            text("🔍 Search results")
                .size(font::sized(15.0))
                .color(Color::from_rgb(0.85, 0.87, 0.9)),
            Space::new().width(Length::Fixed(8.0)),
            text(label)
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
        ]
        .align_y(Alignment::Center)
        .into()
    } else {
        match browser.selected_author() {
            Some(a) => {
                let n = browser.tunes().len();
                row![
                    text(&a.display_name)
                        .size(font::sized(15.0))
                        .color(Color::from_rgb(0.85, 0.87, 0.9)),
                    Space::new().width(Length::Fixed(8.0)),
                    text(format!("— {n} tunes"))
                        .size(font::sized(12.0))
                        .color(Color::from_rgb(0.55, 0.57, 0.62)),
                ]
                .align_y(Alignment::Center)
                .into()
            }
            None => text(if has_search {
                "No matches — try a different query."
            } else {
                "Select an author on the left, or type in the search box to find tunes globally."
            })
            .size(font::sized(13.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .into(),
        }
    };

    // Tune rows (no virtualisation for MVP — typical author has <50 tunes).
    let mut tune_col: Column<'a, Message> = column![].spacing(1);
    if show_flat_results {
        // Global search results: each row shows filename + author/section
        // attribution, since hits can come from anywhere in the category.
        let col_author_w = Length::FillPortion(3);
        let col_actions_pad = Length::Fixed(72.0);
        tune_col = tune_col.push(
            row![
                text("Filename")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(Length::FillPortion(5)),
                text("Author / section")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(col_author_w),
                Space::new().width(col_actions_pad),
            ]
            .padding(Padding::from([2, 10]))
            .spacing(8)
            .align_y(Alignment::Center),
        );
        for &fi in &flat_results {
            let f = &browser.flat_index()[fi];
            let row_widget = row![
                text(&f.stem)
                    .size(font::sized(13.0))
                    .color(Color::from_rgb(0.85, 0.87, 0.9))
                    .width(Length::FillPortion(5))
                    .wrapping(text::Wrapping::None),
                text(&f.author_raw)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.65, 0.67, 0.72))
                    .width(col_author_w)
                    .wrapping(text::Wrapping::None),
                tool_button("▶", Message::HvscBrowserPlayFlat(fi)),
                Space::new().width(Length::Fixed(4.0)),
                tool_button("➕", Message::HvscBrowserAddFlat(fi)),
            ]
            .padding(Padding::from([2, 10]))
            .spacing(8)
            .align_y(Alignment::Center);
            tune_col = tune_col.push(row_widget);
        }
    } else if browser.selected_author().is_some() {
        // Column widths. Title + Released share the flexible space with
        // FillPortion so short titles don't leave a giant gap before the
        // metadata, and long publisher strings in `released` don't wrap
        // to multiple lines. The right-side metric columns stay fixed so
        // they align across rows.
        let col_subs_w = Length::Fixed(40.0);
        let col_len_w = Length::Fixed(60.0);
        let col_stil_w = Length::Fixed(40.0);
        // Column header
        tune_col = tune_col.push(
            row![
                text("Title")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(Length::FillPortion(5)),
                text("Released")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(Length::FillPortion(3)),
                text("#")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(col_subs_w),
                text("Len")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(col_len_w),
                text("STIL")
                    .size(font::sized(11.0))
                    .color(Color::from_rgb(0.55, 0.57, 0.62))
                    .width(col_stil_w),
                Space::new().width(Length::Fixed(72.0)),
            ]
            .padding(Padding::from([2, 10]))
            .spacing(8)
            .align_y(Alignment::Center),
        );
        let filtered_tunes = browser.filtered_tunes();
        for &idx in &filtered_tunes {
            let t = &browser.tunes()[idx];
            let e = &t.entry;
            let duration_label = match e.duration_secs {
                Some(s) => format!("{}:{:02}", s / 60, s % 60),
                None => "—".to_string(),
            };
            let stil_marker = if t.has_stil { "✓" } else { "" };
            let row_widget = row![
                text(&e.title)
                    .size(font::sized(13.0))
                    .color(Color::from_rgb(0.85, 0.87, 0.9))
                    .width(Length::FillPortion(5))
                    .wrapping(text::Wrapping::None),
                text(&e.released)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.65, 0.67, 0.72))
                    .width(Length::FillPortion(3))
                    .wrapping(text::Wrapping::None),
                text(e.songs.to_string())
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.65, 0.67, 0.72))
                    .width(col_subs_w),
                text(duration_label)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.65, 0.67, 0.72))
                    .width(col_len_w),
                text(stil_marker)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.4, 0.85, 0.5))
                    .width(col_stil_w),
                tool_button("▶", Message::HvscBrowserPlayTune(idx)),
                Space::new().width(Length::Fixed(4.0)),
                tool_button("➕", Message::HvscBrowserAddTune(idx)),
            ]
            .padding(Padding::from([2, 10]))
            .spacing(8)
            .align_y(Alignment::Center);
            tune_col = tune_col.push(row_widget);
        }
    }

    let right_col = column![right_header, scrollable(tune_col).height(Length::Fill),]
        .spacing(8)
        .padding(Padding::from([8, 8]))
        .width(Length::Fill);

    // ── Footer: add-all + category segmented + close ───────────────────────
    let category_btn = |cat: HvscCategory| -> Element<'a, Message> {
        let active = browser.category() == cat;
        let label = if active {
            format!("✓ {}", cat.label())
        } else {
            cat.label().to_string()
        };
        tool_button(
            Box::leak(label.into_boxed_str()),
            Message::HvscBrowserCategoryChanged(cat),
        )
    };

    let add_all_label = match browser.selected_author() {
        Some(_) => format!("⬇ Add all ({})", browser.tunes().len()),
        None => "⬇ Add all".to_string(),
    };
    let add_all_btn: Element<'a, Message> = if browser.selected_author().is_some() {
        tool_button(
            Box::leak(add_all_label.into_boxed_str()),
            Message::HvscBrowserAddAllFromAuthor,
        )
    } else {
        // Inert placeholder: same layout, dimmed look (no on_press).
        text("⬇ Add all")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.35, 0.36, 0.40))
            .into()
    };

    let footer = row![
        add_all_btn,
        Space::new().width(Length::Fill),
        category_btn(HvscCategory::Musicians),
        category_btn(HvscCategory::Demos),
        category_btn(HvscCategory::Games),
        Space::new().width(Length::Fixed(8.0)),
        tool_button("✕ Close", Message::ToggleHvscBrowser),
    ]
    .spacing(6)
    .padding(Padding::from([6, 12]))
    .align_y(Alignment::Center);

    let body = row![left_col, rule::vertical(1), right_col];

    let mut outer: Column<'a, Message> = column![];
    if let Some(banner) = update_banner {
        outer = outer.push(banner).push(rule::horizontal(1));
    }
    outer = outer.push(body).push(rule::horizontal(1)).push(footer);

    container(outer)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

pub fn settings_panel<'a>(
    config: &Config,
    default_length_text: &'a str,
    download_status: &'a str,
    stil_status: &'a str,
    http_remote_running: bool,
    http_port_text: &'a str,
    base_font_size_text: &'a str,
    // `hvsc_sync_active` true while a HVSC rsync sync is running (swaps
    // Sync/Cancel button + reveals progress bar).
    hvsc_sync_active: bool,
    // Status line for the HVSC sync section.
    hvsc_sync_status: &'a str,
    // Optional (files_done, files_total) — rendered as a progress bar.
    hvsc_sync_progress: Option<(u32, u32)>,
) -> Element<'a, Message> {
    let header = row![
        text("Settings")
            .size(font::sized(18.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
        Space::new().width(Length::Fill),
        tool_button("✕ Close", Message::ToggleSettings),
    ]
    .align_y(Alignment::Center);

    // ── Output Engine ────────────────────────────────────────────
    let engines = crate::sid_device::available_engines();
    let current_engine = &config.output_engine;

    let mut engine_col = column![text("Audio output engine:")
        .size(font::sized(14.0))
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
            .size(font::sized(12.0)),
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
            "sidlite" => "🎶 SIDLite Emulation (libsidplayfp)",
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
            button(text(label).size(font::sized(12.0)))
                .on_press(Message::SetOutputEngine(name.to_string()))
                .padding(Padding::from([4, 10]))
                .width(Length::Fill)
                .style(move |_theme: &Theme, st| engine_btn_style(is_active, st)),
        );
    }

    engine_col = engine_col
        .push(
            text("Playback will restart automatically on the new engine.")
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        )
        .push(rule::horizontal(1))
        .push(
            text("Ultimate 64 connection:")
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.65, 0.67, 0.72)),
        )
        .push(
            text_input("IP address (e.g. 192.168.1.64)", &config.u64_address)
                .on_input(Message::SetU64Address)
                .size(font::sized(12.0))
                .padding(Padding::from([4, 8]))
                .width(Length::Fill),
        )
        .push(
            text_input("Password (leave empty if none)", &config.u64_password)
                .on_input(Message::SetU64Password)
                .size(font::sized(12.0))
                .padding(Padding::from([4, 8]))
                .width(Length::Fill),
        )
        .push(
            text("Set IP/hostname of your Ultimate 64 or Ultimate-II+ device.")
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        )
        .push(rule::horizontal(1))
        .push(
            text("U64 audio streaming:")
                .size(font::sized(12.0))
                .color(Color::from_rgb(0.65, 0.67, 0.72)),
        )
        .push(
            tool_button(
                if config.u64_audio_enabled {
                    "✓ Stream U64 audio to this machine"
                } else {
                    "✗ U64 audio streaming disabled"
                },
                Message::ToggleU64Audio,
            ),
        )
        .push(
            row![
                text("UDP port:").size(font::sized(11.0)).color(Color::from_rgb(0.65, 0.67, 0.72)),
                Space::new().width(6),
                text_input("11001", &config.u64_audio_port.to_string())
                    .on_input(Message::U64AudioPortChanged)
                    .size(font::sized(12.0))
                    .padding(Padding::from([4, 8]))
                    .width(Length::Fixed(80.0)),
            ]
            .align_y(Alignment::Center),
        )
        .push(
            text("When enabled, the U64 streams its SID audio over UDP to this machine. Use a different port to the video stream.")
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.45, 0.47, 0.52)),
        );

    // ── Skip RSID ────────────────────────────────────────────────
    let rsid_section = column![
        text("Skip RSID tunes:")
            .size(font::sized(14.0))
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
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
    ]
    .spacing(6);

    // ── macOS USB transport (bridge daemon vs direct libusb) ─────
    // Only meaningful on macOS — on Linux/Windows there is no daemon and
    // we always use the direct path. We render the picker as an empty
    // Space on those platforms so the layout below doesn't have to be
    // conditionally compiled.
    #[cfg(target_os = "macos")]
    let macos_usb_section: Element<'a, Message> = {
        let is_direct = config.macos_usb_mode == "direct";
        column![
            text("macOS USB transport:")
                .size(font::sized(14.0))
                .color(Color::from_rgb(0.75, 0.77, 0.82)),
            iced::widget::row![
                tool_button(
                    if !is_direct {
                        "✓ Bridge daemon"
                    } else {
                        "  Bridge daemon"
                    },
                    Message::SetMacosUsbMode("bridge".to_string()),
                ),
                tool_button(
                    if is_direct {
                        "✓ Direct (no daemon)"
                    } else {
                        "  Direct (no daemon)"
                    },
                    Message::SetMacosUsbMode("direct".to_string()),
                ),
            ]
            .spacing(8),
            text(
                "Bridge runs the USB driver as a root LaunchDaemon (default — needed if your \
                 user account doesn't have USB access). Direct opens the device in-process \
                 with libusb — no daemon."
            )
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
        ]
        .spacing(6)
        .into()
    };
    #[cfg(not(target_os = "macos"))]
    let macos_usb_section: Element<'a, Message> = Space::new().into();

    // ── Force stereo ─────────────────────────────────────────────
    let stereo_section = column![
        text("Force stereo for 2SID tunes:").size(font::sized(14.0)).color(Color::from_rgb(0.75, 0.77, 0.82)),
        tool_button(
            if config.force_stereo_2sid { "✓ Yes — mirror SID1 to both channels" } else { "✗ No — true dual-SID (L=SID1, R=SID2)" },
            Message::ToggleForceStereo2sid,
        ),
        text("When enabled, 2SID tunes ignore the second SID and mirror SID1 to both speakers (same as mono).").size(font::sized(11.0)).color(Color::from_rgb(0.45, 0.47, 0.52)),
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

    // ── Base font size ─────────────────────────────────────────
    let font_size_section = column![
        text("Base font size (pt):")
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text_input("12.0", base_font_size_text)
            .on_input(Message::BaseFontSizeChanged)
            .size(font::sized(14.0))
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
        text("All UI text scales relative to this. Default 12. Range 8–32.")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
    ]
    .spacing(6);

    let length_section = column![
        text("Default song length (seconds):")
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text_input("0 = disabled", default_length_text)
            .on_input(Message::DefaultSongLengthChanged)
            .size(font::sized(14.0))
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
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
        text("Fallback duration for songs not found in Songlength DB. Set to 0 to disable.")
            .size(font::sized(11.0))
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
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text("Fetched from <HVSC base>/DOCUMENTS/Songlengths.md5 — set the HVSC URL above.")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62)),
        tool_button(
            "⬇ Download / Refresh Songlength.md5",
            Message::DownloadSonglength
        ),
        tool_button("📂 Load Songlength.md5 from file…", Message::LoadSonglength),
        text(download_status)
            .size(font::sized(12.0))
            .color(dl_color),
    ]
    .spacing(6);

    // ── STIL database ────────────────────────────────────────────
    let stil_color = if stil_status.contains("Error") || stil_status.contains("fail") {
        Color::from_rgb(1.0, 0.4, 0.4)
    } else if stil_status.contains("success") || stil_status.contains("Loaded") {
        Color::from_rgb(0.4, 0.9, 0.5)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };

    let stil_section = column![
        text("HVSC STIL.txt (song info & comments):")
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text("Fetched from <HVSC base>/DOCUMENTS/STIL.txt — set the HVSC URL above.")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62)),
        tool_button("⬇ Download / Refresh STIL.txt", Message::DownloadStil),
        tool_button("📂 Load STIL.txt from file…", Message::LoadStil),
        text("HVSC root directory (optional — improves lookup accuracy):")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62)),
        text_input(
            "e.g. /home/user/C64Music",
            config.hvsc_root.as_deref().unwrap_or(""),
        )
        .on_input(Message::HvscRootChanged)
        .on_submit(Message::SetHvscRoot(
            config.hvsc_root.clone().unwrap_or_default(),
        ))
        .size(font::sized(12.0))
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .style(|_theme: &Theme, _st| text_input::Style {
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
        }),
        text(stil_status).size(font::sized(12.0)).color(stil_color),
    ]
    .spacing(6);

    // ── HVSC rsync sync (experimental) ──────────────────────────
    let hvsc_color = if hvsc_sync_status.contains("Error")
        || hvsc_sync_status.contains("fail")
        || hvsc_sync_status.contains("Cancelled")
    {
        Color::from_rgb(1.0, 0.4, 0.4)
    } else if hvsc_sync_status.contains("Done") || hvsc_sync_status.contains("Last synced") {
        Color::from_rgb(0.4, 0.9, 0.5)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };
    let hvsc_progress_widget: Element<'a, Message> = if let Some((done, total)) = hvsc_sync_progress
    {
        let pct = if total > 0 {
            (done as f32) / (total as f32)
        } else {
            0.0
        };
        iced::widget::progress_bar(0.0..=1.0, pct.clamp(0.0, 1.0)).into()
    } else {
        Space::new().into()
    };
    let hvsc_sync_button: Element<'a, Message> = if hvsc_sync_active {
        tool_button("✗ Cancel sync", Message::HvscRsyncCancel)
    } else {
        tool_button("⬇ Sync HVSC now", Message::HvscRsyncStart)
    };
    let hvsc_section = column![
        text("HVSC tunes (HTTPS mirror):")
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        text_input(
            "HTTPS URL of an HVSC mirror's directory index",
            &config.hvsc_rsync_url
        )
        .on_input(Message::HvscRsyncUrlChanged)
        .size(font::sized(12.0))
        .padding(Padding::from([6, 10]))
        .width(Length::Fill)
        .style(|_theme: &Theme, _st| text_input::Style {
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
        }),
        text(
            "Destination: the HVSC root folder set above. \
             If unset, defaults to your app-data dir."
        )
        .size(font::sized(11.0))
        .color(Color::from_rgb(0.55, 0.57, 0.62)),
        hvsc_sync_button,
        hvsc_progress_widget,
        text(hvsc_sync_status)
            .size(font::sized(12.0))
            .color(hvsc_color),
    ]
    .spacing(6);

    // ── HTTP Remote Control ─────────────────────────────────────
    let remote_status = if http_remote_running {
        let ip = local_ip_address().unwrap_or_else(|| "localhost".to_string());
        format!("● Running on http://{}:{}", ip, config.http_remote_port)
    } else {
        "○ Stopped".to_string()
    };
    let remote_status_color = if http_remote_running {
        Color::from_rgb(0.4, 0.9, 0.5)
    } else {
        Color::from_rgb(0.5, 0.5, 0.6)
    };
    let remote_section = column![
        text("Remote control (HTTP):")
            .size(font::sized(14.0))
            .color(Color::from_rgb(0.75, 0.77, 0.82)),
        tool_button(
            if http_remote_running {
                "■ Stop remote server"
            } else {
                "▶ Start remote server"
            },
            Message::ToggleHttpRemote,
        ),
        row![
            text("Port:")
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.65, 0.67, 0.72)),
            Space::new().width(6),
            text_input("8364", http_port_text)
                .on_input(Message::HttpRemotePortChanged)
                .size(font::sized(12.0))
                .padding(Padding::from([4, 8]))
                .width(Length::Fixed(80.0)),
        ]
        .align_y(Alignment::Center),
        text(remote_status)
            .size(font::sized(12.0))
            .color(remote_status_color),
        text("Control Phosphor from any browser on the same network.")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.52)),
    ]
    .spacing(6);

    // ── Keyboard shortcuts ───────────────────────────────────────
    let mut kb_col = column![text("Keyboard shortcuts:")
        .size(font::sized(14.0))
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
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.75, 0.88, 1.0))
                    .width(Length::Fixed(100.0)),
                text(desc)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.65, 0.67, 0.72)),
            ]
            .spacing(8),
        );
    }

    let content = column![
        header,
        rule::horizontal(1),
        engine_col,
        rule::horizontal(1),
        macos_usb_section,
        rule::horizontal(1),
        rsid_section,
        rule::horizontal(1),
        stereo_section,
        rule::horizontal(1),
        length_section,
        rule::horizontal(1),
        font_size_section,
        rule::horizontal(1),
        dl_section,
        rule::horizontal(1),
        stil_section,
        rule::horizontal(1),
        hvsc_section,
        rule::horizontal(1),
        remote_section,
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
//  STIL info overlay
// ─────────────────────────────────────────────────────────────────────────────

/// Fullscreen karaoke overlay for MUS files without FLAG sync.
/// Shows lyrics as static scrollable text with CRT-style background.
#[allow(dead_code)]
pub fn karaoke_static_overlay(lyrics: &str) -> Element<'_, Message> {
    use iced::widget::scrollable;

    let body = scrollable(
        container(
            text(lyrics)
                .size(font::sized(18.0))
                .font(iced::Font::MONOSPACE)
                .color(Color::from_rgb(0.35, 0.90, 0.60)),
        )
        .width(Length::Fill)
        .padding(Padding::from([20, 60])),
    )
    .width(Length::Fill)
    .height(Length::Fill);

    let panel = container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgba(
                0.04, 0.04, 0.06, 0.97,
            ))),
            ..Default::default()
        });

    mouse_area(panel)
        .on_press(Message::Noop) // prevent click-through
        .into()
}

/// Build the STIL info overlay panel.  Rendered via `iced::widget::stack!`
/// on top of the normal UI — clicking outside or the × button dismisses it.
pub fn stil_overlay<'a>(text_content: &'a str, subtune: u16) -> Element<'a, Message> {
    use iced::widget::scrollable;

    let header = row![
        text(format!("ⓘ  Song Info  (subtune {})", subtune))
            .size(font::sized(13.0))
            .color(Color::from_rgb(0.45, 0.75, 1.0))
            .width(Length::Fill),
        button(
            text("✕")
                .size(font::sized(13.0))
                .color(Color::from_rgb(0.7, 0.7, 0.8))
        )
        .on_press(Message::DismissStilOverlay)
        .padding(Padding::from([2, 8]))
        .style(|_theme: &Theme, _st| button::Style {
            background: None,
            ..Default::default()
        }),
    ]
    .align_y(Alignment::Center)
    .spacing(8);

    let body = scrollable(
        container(
            text(text_content)
                .size(font::sized(12.0))
                .font(iced::Font::MONOSPACE)
                .color(Color::from_rgb(0.80, 0.83, 0.88)),
        )
        .width(Length::Fill)
        .padding(Padding::from([0, 40])),
    )
    .width(Length::Fill)
    .height(Length::Fill);

    let panel = container(
        column![header, rule::horizontal(1), body]
            .spacing(8)
            .padding(Padding::from([12, 16])),
    )
    .max_width(700)
    .height(Length::FillPortion(7)) // ~70% of available space
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgba(
            0.07, 0.09, 0.12, 0.97,
        ))),
        border: iced::Border {
            color: Color::from_rgb(0.20, 0.35, 0.55),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    });

    // Semi-transparent backdrop that dismisses when clicked.
    let backdrop = button(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::DismissStilOverlay)
    .padding(0)
    .style(|_theme: &Theme, _st| button::Style {
        background: Some(iced::Background::Color(Color::from_rgba(
            0.0, 0.0, 0.0, 0.55,
        ))),
        ..Default::default()
    })
    .width(Length::Fill)
    .height(Length::Fill);

    iced::widget::stack![
        backdrop,
        container(panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Small utility button used throughout the settings panel and toolbars.
fn tool_button<'a>(label: &'a str, msg: Message) -> Element<'a, Message> {
    button(text(label).size(font::sized(12.0)))
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
            let is_mus = entry
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mus"))
                .unwrap_or(false);
            let type_str = if is_mus {
                "mus"
            } else if entry.is_rsid {
                "rsid"
            } else {
                "psid"
            };
            entry.title.to_lowercase().contains(&q)
                || entry.author.to_lowercase().contains(&q)
                || entry.released.to_lowercase().contains(&q)
                || entry.path.to_string_lossy().to_lowercase().contains(&q)
                || type_str.contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Status bar — thin footer strip shown at the very bottom of the main window
// ─────────────────────────────────────────────────────────────────────────────

/// Thin right-aligned footer bar showing HVSC completion stats.
/// Mimics the foobar2000 status bar style.
pub fn status_bar<'a>(
    heard_text: &'a str,
    hvsc_version_text: &'a str,
    hvsc_update_available: bool,
) -> Element<'a, Message> {
    let help_btn = button(text("?").size(font::sized(10.0)))
        .on_press(Message::ShowHelp)
        .padding(Padding::from([1, 6]))
        .style(|_theme: &Theme, st| button::Style {
            background: Some(iced::Background::Color(match st {
                button::Status::Hovered => Color::from_rgb(0.18, 0.20, 0.24),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.0),
            })),
            text_color: Color::from_rgb(0.38, 0.40, 0.50),
            border: iced::Border::default(),
            ..Default::default()
        });

    // HVSC version indicator. Dim grey when up-to-date; amber when an
    // update is available. Empty string → render an empty Space so the
    // status bar layout doesn't reflow before the boot-time check returns.
    let hvsc_color = if hvsc_update_available {
        Color::from_rgb(0.95, 0.70, 0.30) // amber
    } else {
        Color::from_rgb(0.42, 0.44, 0.52) // same dim grey as heard_text
    };
    let hvsc_element: Element<'a, Message> = if hvsc_version_text.is_empty() {
        Space::new().into()
    } else {
        text(hvsc_version_text)
            .size(font::sized(11.0))
            .color(hvsc_color)
            .into()
    };

    container(
        row![
            Space::new().width(Length::Fixed(4.0)),
            help_btn,
            Space::new().width(Length::Fill),
            hvsc_element,
            Space::new().width(Length::Fixed(16.0)),
            text(heard_text)
                .size(font::sized(11.0))
                .color(Color::from_rgb(0.42, 0.44, 0.52)),
            Space::new().width(Length::Fixed(12.0)),
        ]
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(Padding::from([1, 0]))
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.08, 0.09, 0.11))),
        ..Default::default()
    })
    .into()
}

/// Full-screen keyboard shortcut reference + author notice.
/// Dismissed by clicking anywhere or pressing Escape / ?.
pub fn help_overlay<'a>() -> Element<'a, Message> {
    let dismiss = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::DismissHelp);

    let shortcuts: &[(&str, &str)] = &[
        ("Space", "Play / Pause"),
        ("← →", "Previous / Next track"),
        ("↑ ↓", "Select track in playlist"),
        ("F", "Toggle full-screen visualiser"),
        (
            "V",
            "Cycle visualiser mode (Bars / Scope / Tracker / Karaoke)",
        ),
        ("K", "Toggle karaoke lyrics (MUS files)"),
        ("H", "Toggle favourite for current track"),
        ("M", "Toggle mini player"),
        ("Ctrl+F", "Focus search"),
        ("Delete", "Remove selected track"),
        ("Escape / ?", "Close this overlay"),
    ];

    let row_height = 22.0_f32;
    let mut rows: Vec<Element<'_, Message>> = Vec::new();

    // Title
    rows.push(
        text("Phosphor — Keyboard Shortcuts")
            .size(font::sized(15.0))
            .color(Color::from_rgb(0.35, 0.90, 0.60))
            .into(),
    );
    rows.push(Space::new().height(Length::Fixed(10.0)).into());

    for (key, action) in shortcuts {
        rows.push(
            row![
                container(
                    text(*key)
                        .size(font::sized(12.0))
                        .color(Color::from_rgb(0.80, 0.82, 0.90))
                )
                .width(Length::Fixed(180.0)),
                text(*action)
                    .size(font::sized(12.0))
                    .color(Color::from_rgb(0.60, 0.62, 0.70)),
            ]
            .height(Length::Fixed(row_height))
            .align_y(Alignment::Center)
            .into(),
        );
    }

    rows.push(Space::new().height(Length::Fixed(16.0)).into());

    // Author notice
    rows.push(
        text("Phosphor — SID music player")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.35, 0.37, 0.45))
            .into(),
    );
    rows.push(
        text("Built with Rust + Iced  •  USBSID-Pico / reSID / Ultimate 64")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.30, 0.32, 0.40))
            .into(),
    );

    let panel = container(
        iced::widget::Column::with_children(rows)
            .spacing(0)
            .padding(Padding::from([24, 32])),
    )
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgba(
            0.07, 0.08, 0.12, 0.97,
        ))),
        border: iced::Border {
            radius: 8.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.20, 0.22, 0.30),
        },
        ..Default::default()
    });

    // Centre the panel with fixed width
    let centred = container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill);

    iced::widget::stack![dismiss, centred]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Mini player view — compact 420×90 window
// ─────────────────────────────────────────────────────────────────────────────

pub const MINI_WIDTH: f32 = 420.0;
pub const MINI_HEIGHT: f32 = 90.0;

/// Compact single-window view shown in mini-player mode.
/// Fits in 420×90 logical pixels — just title, author, transport + progress.
pub fn mini_player_view<'a>(
    status: &'a PlayerStatus,
    current_duration: Option<u32>,
    is_favorite: bool,
) -> Element<'a, Message> {
    // ── Transport buttons ─────────────────────────────────────────────────────
    let mk_btn = |label: &'a str, msg: Message| -> Element<'a, Message> {
        button(text(label).size(font::sized(13.0)))
            .on_press(msg)
            .padding(Padding::from([4, 10]))
            .style(|_theme: &Theme, st| button::Style {
                background: Some(iced::Background::Color(match st {
                    button::Status::Hovered => Color::from_rgb(0.22, 0.24, 0.30),
                    button::Status::Pressed => Color::from_rgb(0.16, 0.18, 0.22),
                    _ => Color::from_rgb(0.15, 0.16, 0.20),
                })),
                text_color: Color::from_rgb(0.85, 0.87, 0.92),
                border: iced::Border {
                    radius: 3.0.into(),
                    width: 1.0,
                    color: Color::from_rgb(0.25, 0.27, 0.32),
                },
                ..Default::default()
            })
            .into()
    };

    let play_label = match status.state {
        PlayState::Playing => "❚❚",
        _ => "▶",
    };

    let fav_label = if is_favorite { "♥" } else { "♡" };
    let fav_color = if is_favorite {
        Color::from_rgb(0.95, 0.35, 0.45)
    } else {
        Color::from_rgb(0.55, 0.58, 0.65)
    };

    let transport = row![
        mk_btn("◀◀", Message::PrevTrack),
        mk_btn(play_label, Message::PlayPause),
        mk_btn("■", Message::Stop),
        mk_btn("▶▶", Message::NextTrack),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    // ── Title + author ────────────────────────────────────────────────────────
    let (title, author) = match &status.track_info {
        Some(info) => (info.name.as_str(), info.author.as_str()),
        None => ("No track loaded", "—"),
    };

    let song_info = column![
        text(title)
            .size(font::sized(13.0))
            .color(Color::from_rgb(0.88, 0.90, 0.95)),
        text(author)
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.52, 0.55, 0.65)),
    ]
    .spacing(2)
    .width(Length::Fill);

    // ── Progress bar (thin strip) ─────────────────────────────────────────────
    let elapsed_secs = status.elapsed.as_secs();
    let total_secs = current_duration.unwrap_or(0) as u64;
    let fraction = if total_secs > 0 {
        (elapsed_secs as f32 / total_secs as f32).min(1.0)
    } else {
        0.0
    };
    let bar_pct = (fraction * 100.0) as u16;

    let progress_strip = row![
        container(Space::new().height(Length::Fixed(3.0)))
            .width(Length::FillPortion(bar_pct.max(1)))
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.30, 0.70, 0.50))),
                ..Default::default()
            }),
        container(Space::new().height(Length::Fixed(3.0)))
            .width(Length::FillPortion(100u16.saturating_sub(bar_pct).max(1)))
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.20, 0.24))),
                ..Default::default()
            }),
    ]
    .width(Length::Fill);

    // ── Expand + fav buttons ──────────────────────────────────────────────────
    let expand_btn = button(text("⤢").size(font::sized(12.0)))
        .on_press(Message::ToggleMiniPlayer)
        .padding(Padding::from([4, 8]))
        .style(|_theme: &Theme, st| button::Style {
            background: Some(iced::Background::Color(match st {
                button::Status::Hovered => Color::from_rgb(0.22, 0.24, 0.30),
                _ => Color::from_rgba(0.0, 0.0, 0.0, 0.0),
            })),
            text_color: Color::from_rgb(0.45, 0.48, 0.58),
            border: iced::Border::default(),
            ..Default::default()
        });

    let fav_btn = button(text(fav_label).size(font::sized(12.0)))
        .on_press(Message::ToggleFavoriteCurrent)
        .padding(Padding::from([4, 8]))
        .style(move |_theme: &Theme, _st| button::Style {
            background: None,
            text_color: fav_color,
            border: iced::Border::default(),
            ..Default::default()
        });

    // ── Main row ──────────────────────────────────────────────────────────────
    let main_row = row![
        transport,
        Space::new().width(Length::Fixed(8.0)),
        song_info,
        fav_btn,
        expand_btn,
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .padding(Padding::from([8, 10]));

    container(column![main_row, progress_strip,])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Demoscene-style loading scroller (Canvas program)
// ─────────────────────────────────────────────────────────────────────────────

/// Demoscene-style loading screen with rainbow pixel text and a spinning cube.
/// The `tick` field changes every frame (33ms) which forces iced to redraw.
struct LoadingScroller {
    text: String,
    tick: u32,
}

impl<Message> canvas::Program<Message> for LoadingScroller {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let w = bounds.width;
        let h = bounds.height;
        let t = self.tick as f32;

        // ── Spinning wireframe cube (left side) ─────────────────────────
        let cube_cx = 50.0;
        let cube_cy = h * 0.5;
        let cube_r = 18.0;
        let angle = t * 0.06;
        let cos_a = angle.cos();
        let sin_a = angle.sin();
        let angle_b = t * 0.04;
        let cos_b = angle_b.cos();
        let sin_b = angle_b.sin();

        // 8 cube vertices, 3D → 2D projection with two-axis rotation
        let verts: Vec<(f32, f32)> = [
            (-1.0, -1.0, -1.0),
            (1.0, -1.0, -1.0),
            (1.0, 1.0, -1.0),
            (-1.0, 1.0, -1.0),
            (-1.0, -1.0, 1.0),
            (1.0, -1.0, 1.0),
            (1.0, 1.0, 1.0),
            (-1.0, 1.0, 1.0),
        ]
        .iter()
        .map(|&(x, y, z)| {
            // Rotate Y axis
            let x2 = x * cos_a - z * sin_a;
            let z2 = x * sin_a + z * cos_a;
            // Rotate X axis
            let y2 = y * cos_b - z2 * sin_b;
            let z3 = y * sin_b + z2 * cos_b;
            // Simple perspective
            let scale = 1.0 / (3.0 - z3);
            (
                cube_cx + x2 * cube_r * scale * 2.0,
                cube_cy + y2 * cube_r * scale * 2.0,
            )
        })
        .collect();

        let edges = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 0), // front
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 4), // back
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7), // connecting
        ];
        let cube_hue = (t * 0.008 % 1.0).abs();
        let cube_color = visualizer::hue_to_rgb(cube_hue, 0.85, 0.90);
        let stroke = iced::widget::canvas::Stroke::default()
            .with_color(cube_color)
            .with_width(1.5);
        for &(a, b) in &edges {
            let path = iced::widget::canvas::Path::line(
                Point::new(verts[a].0, verts[a].1),
                Point::new(verts[b].0, verts[b].1),
            );
            frame.stroke(&path, stroke.clone());
        }

        // ── Row 1: status text with sine-wave bounce (scale 4, rainbow) ─
        let clean = self
            .text
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let row1_str = clean.to_uppercase();
        let row1_chars: Vec<char> = row1_str.chars().collect();
        let scale1: f32 = 4.0;
        let char_w1 = 3.0 * scale1 + scale1;
        let row1_total_w = row1_chars.len() as f32 * char_w1;
        let row1_x = (w - row1_total_w) * 0.5;
        let row1_y = 10.0;
        let wave_amp = 5.0;

        for (ci, ch) in row1_chars.iter().enumerate() {
            let cx = row1_x + ci as f32 * char_w1;
            let phase = t * 0.08 + ci as f32 * 0.4;
            let bob = phase.sin() * wave_amp;
            let hue_t = ((ci as f32 * 0.06 + t * 0.005) % 1.0).abs();
            let color = visualizer::hue_to_rgb(hue_t, 0.90, 0.95);

            if let Some(rows) = visualizer::glyph(*ch) {
                for (ri, row) in rows.iter().enumerate() {
                    for (pi, &on) in row.iter().enumerate() {
                        if on {
                            let px = cx + pi as f32 * scale1;
                            let py = row1_y + bob + ri as f32 * scale1;
                            if px >= 0.0 && px + scale1 <= w {
                                frame.fill_rectangle(
                                    Point::new(px, py),
                                    Size::new(scale1, scale1),
                                    color,
                                );
                            }
                        }
                    }
                }
            }
        }

        // ── Row 2: subtitle (scale 2, rainbow) ─────────────────────────
        let row2_str = "PLEASE WAIT . . .  LOADING SID FILES";
        let row2_chars: Vec<char> = row2_str.chars().collect();
        let scale2: f32 = 2.0;
        let char_w2 = 3.0 * scale2 + scale2;
        let row2_total_w = row2_chars.len() as f32 * char_w2;
        let row2_x = (w - row2_total_w) * 0.5;
        let row2_y = row1_y + 5.0 * scale1 + wave_amp + 16.0;

        for (ci, ch) in row2_chars.iter().enumerate() {
            let cx = row2_x + ci as f32 * char_w2;
            let hue_t = ((ci as f32 * 0.04 + t * 0.007) % 1.0).abs();
            let color = visualizer::hue_to_rgb(hue_t, 0.80, 0.82);

            if let Some(rows) = visualizer::glyph(*ch) {
                for (ri, row) in rows.iter().enumerate() {
                    for (pi, &on) in row.iter().enumerate() {
                        if on {
                            let px = cx + pi as f32 * scale2;
                            let py = row2_y + ri as f32 * scale2;
                            if px >= 0.0 && px + scale2 <= w {
                                frame.fill_rectangle(
                                    Point::new(px, py),
                                    Size::new(scale2, scale2),
                                    color,
                                );
                            }
                        }
                    }
                }
            }
        }

        // ── Spinning wireframe cube (right side, mirrored) ──────────────
        let cube_cx2 = w - 50.0;
        let cube_hue2 = ((t * 0.008 + 0.5) % 1.0).abs();
        let cube_color2 = visualizer::hue_to_rgb(cube_hue2, 0.85, 0.90);
        let stroke2 = iced::widget::canvas::Stroke::default()
            .with_color(cube_color2)
            .with_width(1.5);
        let verts2: Vec<(f32, f32)> = [
            (-1.0, -1.0, -1.0),
            (1.0, -1.0, -1.0),
            (1.0, 1.0, -1.0),
            (-1.0, 1.0, -1.0),
            (-1.0, -1.0, 1.0),
            (1.0, -1.0, 1.0),
            (1.0, 1.0, 1.0),
            (-1.0, 1.0, 1.0),
        ]
        .iter()
        .map(|&(x, y, z)| {
            let x2 = x * cos_a + z * sin_a; // opposite rotation
            let z2 = -x * sin_a + z * cos_a;
            let y2 = y * cos_b - z2 * sin_b;
            let z3 = y * sin_b + z2 * cos_b;
            let scale = 1.0 / (3.0 - z3);
            (
                cube_cx2 + x2 * cube_r * scale * 2.0,
                cube_cy + y2 * cube_r * scale * 2.0,
            )
        })
        .collect();
        for &(a, b) in &edges {
            let path = iced::widget::canvas::Path::line(
                Point::new(verts2[a].0, verts2[a].1),
                Point::new(verts2[b].0, verts2[b].1),
            );
            frame.stroke(&path, stroke2.clone());
        }

        vec![frame.into_geometry()]
    }
}

/// Get the first non-loopback IPv4 address of this machine.
fn local_ip_address() -> Option<String> {
    use std::net::UdpSocket;
    // Connect to a public IP (doesn't actually send data) to discover
    // which local interface the OS would route through.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

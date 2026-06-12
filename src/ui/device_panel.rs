//! "Device" top-level panel — USBSID-Pico configuration controls.
//!
//! Shown when `App.show_device_config` is true. Rendered as a scrollable
//! column with sections matching the web Configtool's layout (firmware,
//! sockets, clock, presets, actions).

use iced::widget::{button, column, container, pick_list, row, scrollable, text, Space};
use iced::{Alignment, Color, Element, Length, Padding, Theme};

use usbsid_pico_config::{ChipType, ClockRate, Preset, SidType};

use super::font;
use super::DeviceConfigSnapshot;
use super::Message;
use crate::player::DeviceConfigEdit;

/// Build the Device tab UI.
pub fn device_panel<'a>(
    snapshot: Option<&'a DeviceConfigSnapshot>,
    status: &'a str,
) -> Element<'a, Message> {
    let header = row![
        text("USBSID-Pico Device")
            .size(font::sized(18.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
        Space::new().width(Length::Fill),
        small_button("↻ Refresh", Message::DeviceConfigRefresh),
        Space::new().width(Length::Fixed(8.0)),
        small_button("✕ Close", Message::ToggleDeviceConfig),
    ]
    .align_y(Alignment::Center);

    let status_line = text(status)
        .size(font::sized(12.0))
        .color(if status.starts_with("Error") {
            Color::from_rgb(0.95, 0.45, 0.45)
        } else {
            Color::from_rgb(0.55, 0.85, 0.55)
        });

    let body: Element<'a, Message> = match snapshot {
        Some(snap) => build_loaded(snap),
        None => container(
            text("No device data loaded. Click ↻ Refresh to read the device.")
                .size(font::sized(13.0))
                .color(Color::from_rgb(0.55, 0.57, 0.62)),
        )
        .padding(Padding::from([24, 16]))
        .into(),
    };

    let content = column![header, status_line, body]
        .spacing(12)
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

fn build_loaded<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    column![
        section_about(snap),
        section_clock(snap),
        section_socket("Socket One", &snap.config.socket1, 1),
        section_socket("Socket Two", &snap.config.socket2, 2),
        section_audio(snap),
        section_protocols(snap),
        section_presets(),
        section_actions(),
    ]
    .spacing(16)
    .into()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn section_title<'a>(s: &'a str) -> Element<'a, Message> {
    text(s)
        .size(font::sized(14.0))
        .color(Color::from_rgb(0.75, 0.77, 0.82))
        .into()
}

fn kv_row<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label)
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        text(value)
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
    ]
    .spacing(8)
    .into()
}

fn small_button<'a>(label: &'a str, msg: Message) -> Element<'a, Message> {
    button(text(label).size(font::sized(12.0)))
        .on_press(msg)
        .padding(Padding::from([4, 10]))
        .style(phosphor_button_style)
        .into()
}

/// Phosphor's standard dark button look — matches `ui::mod::tool_button`
/// so the Device tab fits visually with the rest of the app.
fn phosphor_button_style(_t: &Theme, st: button::Status) -> button::Style {
    button::Style {
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
    }
}

/// Row with a label on the left and a toggle button on the right that
/// flips the underlying bool field via `DeviceConfigEdit`.
fn toggle_row<'a>(
    label: &'a str,
    current: bool,
    make_edit: impl Fn(bool) -> DeviceConfigEdit + 'static,
) -> Element<'a, Message> {
    let btn_label = if current { "Enabled" } else { "Disabled" };
    row![
        text(label)
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        button(text(btn_label).size(font::sized(12.0)))
            .on_press(Message::DeviceConfigEdit(make_edit(!current)))
            .padding(Padding::from([3, 12]))
            .style(phosphor_button_style),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// Row with a label + dropdown for an enum field.
fn picker_row<'a, T: Clone + PartialEq + Send + Sync + 'static + ToString>(
    label: &'a str,
    options: Vec<T>,
    current: T,
    make_edit: impl Fn(T) -> DeviceConfigEdit + 'static,
) -> Element<'a, Message> {
    let labels: Vec<String> = options.iter().map(|t| t.to_string()).collect();
    let selected = current.to_string();
    let options_for_cb = options.clone();
    let picker = pick_list(labels, Some(selected), move |chosen| {
        let idx = options_for_cb
            .iter()
            .position(|t| t.to_string() == chosen)
            .unwrap_or(0);
        Message::DeviceConfigEdit(make_edit(options_for_cb[idx].clone()))
    })
    .text_size(font::sized(12.0));

    row![
        text(label)
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        picker,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

// Newtype wrappers so we can `impl ToString` for the enum labels iced
// expects (the upstream enums only have `label() -> &str`).
#[derive(Debug, Clone, PartialEq)]
struct ChipChoice(ChipType);
impl ToString for ChipChoice {
    fn to_string(&self) -> String {
        self.0.label().to_string()
    }
}
#[derive(Debug, Clone, PartialEq)]
struct SidChoice(SidType);
impl ToString for SidChoice {
    fn to_string(&self) -> String {
        self.0.label().to_string()
    }
}

// ── Sections ────────────────────────────────────────────────────────────────

fn section_about<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    column![
        section_title("About"),
        kv_row("Firmware version", format!("v{}", snap.firmware_version)),
        kv_row("PCB version", format!("v{}", snap.pcb_version)),
    ]
    .spacing(4)
    .into()
}

fn section_clock<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    let current = snap.config.clock_rate;
    let options: &[ClockRate] = &[
        ClockRate::Default,
        ClockRate::Pal,
        ClockRate::Ntsc,
        ClockRate::Drean,
        ClockRate::Ntsc2,
    ];
    let labels: Vec<String> = options.iter().map(|r| r.label().to_string()).collect();
    let selected = current.label().to_string();
    let options_for_callback = options.to_vec();
    let picker = pick_list(labels, Some(selected), move |chosen| {
        let idx = options_for_callback
            .iter()
            .position(|r| r.label() == chosen)
            .unwrap_or(0);
        Message::DeviceConfigSetClock(options_for_callback[idx])
    })
    .text_size(font::sized(12.0));

    column![
        section_title("Clock"),
        row![
            text("Rate")
                .size(font::sized(12.0))
                .width(Length::Fixed(180.0)),
            picker,
        ]
        .align_y(Alignment::Center)
        .spacing(8),
        toggle_row("Lock clockrate", snap.config.lock_clockrate, |v| {
            DeviceConfigEdit::LockClockrate(v)
        }),
        toggle_row("External clock", snap.config.external_clock, |v| {
            DeviceConfigEdit::ExternalClock(v)
        }),
    ]
    .spacing(4)
    .into()
}

fn section_socket<'a>(
    title: &'a str,
    sock: &'a usbsid_pico_config::SocketConfig,
    n: u8,
) -> Element<'a, Message> {
    let chips: Vec<ChipChoice> = [
        ChipType::Real,
        ChipType::Unknown,
        ChipType::SkPico,
        ChipType::ArmSid,
        ChipType::Arm2Sid,
        ChipType::FpgaSid,
        ChipType::RedipSid,
        ChipType::PdSid,
        ChipType::BackSid,
        ChipType::SidEmu,
    ]
    .into_iter()
    .map(ChipChoice)
    .collect();
    let sids: Vec<SidChoice> = [
        SidType::Unknown,
        SidType::Na,
        SidType::Mos8580,
        SidType::Mos6581,
        SidType::FmOpl,
    ]
    .into_iter()
    .map(SidChoice)
    .collect();

    column![
        section_title(title),
        toggle_row("Enabled", sock.enabled, move |v| {
            DeviceConfigEdit::SocketEnabled(n, v)
        }),
        toggle_row("Dual SID", sock.dualsid, move |v| {
            DeviceConfigEdit::SocketDualSid(n, v)
        }),
        picker_row("Chip type", chips, ChipChoice(sock.chip_type), move |c| {
            DeviceConfigEdit::SocketChipType(n, c.0)
        }),
        picker_row(
            "SID One",
            sids.clone(),
            SidChoice(sock.sid1.kind),
            move |c| { DeviceConfigEdit::SocketSidType(n, 1, c.0) }
        ),
        picker_row("SID Two", sids, SidChoice(sock.sid2.kind), move |c| {
            DeviceConfigEdit::SocketSidType(n, 2, c.0)
        }),
    ]
    .spacing(4)
    .into()
}

fn section_audio<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    column![
        section_title("Audio routing"),
        toggle_row("Lock audio switch", snap.config.lock_audio_switch, |v| {
            DeviceConfigEdit::LockAudioSwitch(v)
        }),
        toggle_row("Mirrored", snap.config.mirrored, |v| {
            DeviceConfigEdit::Mirrored(v)
        }),
        toggle_row("Flipped", snap.config.flipped, |v| {
            DeviceConfigEdit::Flipped(v)
        }),
        toggle_row("Mixed", snap.config.mixed, |v| DeviceConfigEdit::Mixed(v)),
        toggle_row("Stereo (vs Mono)", snap.config.stereo_enabled, |v| {
            DeviceConfigEdit::StereoEnabled(v)
        }),
    ]
    .spacing(4)
    .into()
}

fn section_protocols<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    let p = &snap.config.protocols;
    // CDC/WebUSB/ASID/MIDI are firmware-managed (the Clojure tool's
    // config->commands writer doesn't emit them either), so they stay
    // read-only. FMOpl is the only writable protocol toggle.
    column![
        section_title("Protocols"),
        kv_row("CDC", bool_label(p.cdc)),
        kv_row("WebUSB", bool_label(p.webusb)),
        kv_row("ASID", bool_label(p.asid)),
        kv_row("MIDI", bool_label(p.midi)),
        toggle_row("FMOpl", p.fmopl_enabled, |v| {
            DeviceConfigEdit::FmoplEnabled(v)
        }),
        kv_row("FMOpl SID", p.fmopl_sidno.to_string()),
    ]
    .spacing(4)
    .into()
}

fn section_presets<'a>() -> Element<'a, Message> {
    let presets: &[Preset] = &[
        Preset::SingleS1,
        Preset::SingleS2,
        Preset::DualBoth,
        Preset::DualS1,
        Preset::DualS2,
        Preset::TripleS1,
        Preset::TripleS2,
        Preset::Quad,
        Preset::Mirrored,
        Preset::MirroredDual,
        Preset::DualFlipped,
        Preset::QuadFlipped,
        Preset::QuadMixed,
        Preset::QuadFlipMixed,
    ];

    let mut grid = column![section_title("Presets")].spacing(4);
    // Two columns per row.
    for chunk in presets.chunks(2) {
        let mut r = row![].spacing(8);
        for p in chunk {
            r = r.push(
                button(text(p.label()).size(font::sized(12.0)))
                    .on_press(Message::DeviceConfigApplyPreset(*p))
                    .padding(Padding::from([4, 10]))
                    .width(Length::Fixed(260.0))
                    .style(phosphor_button_style),
            );
        }
        grid = grid.push(r);
    }
    grid.into()
}

fn section_actions<'a>() -> Element<'a, Message> {
    column![
        section_title("Actions"),
        row![
            button(text("💾 Save to flash").size(font::sized(13.0)))
                .on_press(Message::DeviceConfigSave)
                .padding(Padding::from([6, 14]))
                .style(phosphor_button_style),
            button(text("🔍 Auto-detect SIDs").size(font::sized(13.0)))
                .on_press(Message::DeviceConfigAutoDetect)
                .padding(Padding::from([6, 14]))
                .style(phosphor_button_style),
            button(text("↺ Reset to defaults").size(font::sized(13.0)))
                .on_press(Message::DeviceConfigReset)
                .padding(Padding::from([6, 14]))
                .style(phosphor_button_style),
        ]
        .spacing(10),
    ]
    .spacing(8)
    .into()
}

// ── Tiny utilities ──────────────────────────────────────────────────────────

fn bool_label(b: bool) -> String {
    if b {
        "Enabled".into()
    } else {
        "Disabled".into()
    }
}

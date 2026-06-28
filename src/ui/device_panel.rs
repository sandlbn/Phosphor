//! "Device" top-level panel — USBSID-Pico configuration controls.
//!
//! Shown when `App.show_device_config` is true. Rendered as a scrollable
//! column with sections matching the web Configtool's layout (firmware,
//! sockets, clock, presets, actions).

use iced::widget::{button, column, container, pick_list, row, scrollable, slider, text, Space};
use iced::{Alignment, Color, Element, Length, Padding, Theme};

use usbsid_pico_config::{ChipType, ClockRate, Preset, SidType};

use super::font;
use super::DeviceConfigSnapshot;
use super::Message;
use crate::player::{DeviceConfigCmd, DeviceConfigEdit};

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
    let mut col = column![
        section_about(snap),
        section_clock(snap),
        section_socket("Socket One", &snap.config.socket1, 1),
        section_socket("Socket Two", &snap.config.socket2, 2),
        section_audio(snap),
        section_leds(snap),
        section_protocols(snap),
    ]
    .spacing(16);

    // FPGASID stub: shown when any socket actually has a FPGASID configured.
    if snap.config.socket1.chip_type == ChipType::FpgaSid
        || snap.config.socket2.chip_type == ChipType::FpgaSid
    {
        col = col.push(section_fpgasid_stub());
    }

    col.push(section_advanced(snap))
        .push(section_presets())
        .push(section_tools())
        .push(section_actions())
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
    let mut col = column![
        section_title("Protocols"),
        kv_row("CDC", bool_label(p.cdc)),
        kv_row("WebUSB", bool_label(p.webusb)),
        kv_row("ASID", bool_label(p.asid)),
        kv_row("MIDI", bool_label(p.midi)),
        toggle_row("FMOpl", p.fmopl_enabled, |v| {
            DeviceConfigEdit::FmoplEnabled(v)
        }),
    ]
    .spacing(4);

    if p.fmopl_enabled {
        col = col.push(fmopl_sid_picker(p.fmopl_sidno));
    }

    if p.midi {
        col = col.push(midi_state_row());
    }
    col.into()
}

/// Segmented control for FMopl SID Number — 0=auto, 1..=4=explicit SID.
fn fmopl_sid_picker<'a>(current: u8) -> Element<'a, Message> {
    let mut r = row![text("FMopl SID")
        .size(font::sized(12.0))
        .color(Color::from_rgb(0.55, 0.57, 0.62))
        .width(Length::Fixed(180.0))]
    .spacing(6)
    .align_y(Alignment::Center);
    for (val, label) in [
        (0u8, "Auto"),
        (1, "SID1"),
        (2, "SID2"),
        (3, "SID3"),
        (4, "SID4"),
    ] {
        let active = current == val;
        let btn = button(text(label).size(font::sized(12.0)))
            .on_press(Message::DeviceConfigEdit(DeviceConfigEdit::FmoplSidno(val)))
            .padding(Padding::from([3, 10]))
            .style(move |_t: &Theme, st| {
                if active {
                    button::Style {
                        background: Some(iced::Background::Color(Color::from_rgb(
                            0.30, 0.40, 0.55,
                        ))),
                        text_color: Color::from_rgb(0.95, 0.97, 1.0),
                        border: iced::Border {
                            radius: 3.0.into(),
                            width: 1.0,
                            color: Color::from_rgb(0.45, 0.55, 0.70),
                        },
                        ..Default::default()
                    }
                } else {
                    phosphor_button_style(_t, st)
                }
            });
        r = r.push(btn);
    }
    r.into()
}

/// MIDI state save/load/reset buttons (only meaningful when MIDI enabled).
fn midi_state_row<'a>() -> Element<'a, Message> {
    row![
        text("MIDI state")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        small_button(
            "Load",
            Message::DeviceConfigAction(DeviceConfigCmd::MidiLoadState)
        ),
        small_button(
            "Save",
            Message::DeviceConfigAction(DeviceConfigCmd::MidiSaveState)
        ),
        small_button(
            "Reset",
            Message::DeviceConfigAction(DeviceConfigCmd::MidiResetState)
        ),
        text("(persists in flash)")
            .size(font::sized(11.0))
            .color(Color::from_rgb(0.45, 0.47, 0.55)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn section_leds<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    let led = &snap.config.led;
    let rgb = &snap.config.rgb_led;
    column![
        section_title("LED"),
        toggle_row("Status LED", led.enabled, |v| {
            DeviceConfigEdit::LedEnabled(v)
        }),
        toggle_row("Status LED idle breathe", led.idle_breathe, |v| {
            DeviceConfigEdit::LedIdleBreathe(v)
        }),
        toggle_row("RGB LED", rgb.enabled, |v| {
            DeviceConfigEdit::RgbLedEnabled(v)
        }),
        toggle_row("RGB LED idle breathe", rgb.idle_breathe, |v| {
            DeviceConfigEdit::RgbLedIdleBreathe(v)
        }),
        rgb_brightness_row(rgb.brightness),
        rgb_sid_picker(rgb.sid_to_use),
    ]
    .spacing(4)
    .into()
}

fn rgb_brightness_row<'a>(current: u8) -> Element<'a, Message> {
    // iced slider emits f32 values; round to u8 before dispatching.
    let sl = slider(0.0..=255.0, current as f32, |v| {
        Message::DeviceConfigEdit(DeviceConfigEdit::RgbLedBrightness(v as u8))
    })
    .step(1.0_f32)
    .width(Length::Fixed(220.0));
    row![
        text("RGB brightness")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        sl,
        text(format!("{current}"))
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.85, 0.87, 0.9)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn rgb_sid_picker<'a>(current: i8) -> Element<'a, Message> {
    let mut r = row![text("RGB driven by")
        .size(font::sized(12.0))
        .color(Color::from_rgb(0.55, 0.57, 0.62))
        .width(Length::Fixed(180.0))]
    .spacing(6)
    .align_y(Alignment::Center);
    for (val, label) in [
        (-1_i8, "Off"),
        (1, "SID1"),
        (2, "SID2"),
        (3, "SID3"),
        (4, "SID4"),
    ] {
        let active = current == val;
        let btn = button(text(label).size(font::sized(12.0)))
            .on_press(Message::DeviceConfigEdit(DeviceConfigEdit::RgbLedSidToUse(
                val,
            )))
            .padding(Padding::from([3, 10]))
            .style(move |_t: &Theme, st| {
                if active {
                    button::Style {
                        background: Some(iced::Background::Color(Color::from_rgb(
                            0.30, 0.40, 0.55,
                        ))),
                        text_color: Color::from_rgb(0.95, 0.97, 1.0),
                        border: iced::Border {
                            radius: 3.0.into(),
                            width: 1.0,
                            color: Color::from_rgb(0.45, 0.55, 0.70),
                        },
                        ..Default::default()
                    }
                } else {
                    phosphor_button_style(_t, st)
                }
            });
        r = r.push(btn);
    }
    r.into()
}

fn section_advanced<'a>(snap: &'a DeviceConfigSnapshot) -> Element<'a, Message> {
    let cfg = &snap.config;
    let mut col = column![
        section_title("Advanced (PCB v1.5+)"),
        toggle_row("Need confirmation", cfg.need_confirmation, |v| {
            DeviceConfigEdit::NeedConfirmation(v)
        }),
        toggle_row(
            "Disable socket change-detect",
            cfg.disable_changedetect,
            |v| DeviceConfigEdit::DisableChangeDetect(v),
        ),
    ]
    .spacing(4);
    if cfg.need_confirmation {
        col = col.push(
            row![
                Space::new().width(Length::Fixed(180.0)),
                small_button(
                    "✓ Confirm config",
                    Message::DeviceConfigAction(DeviceConfigCmd::Confirm)
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    col.into()
}

fn section_fpgasid_stub<'a>() -> Element<'a, Message> {
    column![
        section_title("FPGASID"),
        text(
            "FPGASID-specific config (revision, address-decoder, etc.) is not yet \
              supported in Phosphor. Use the upstream USBSID-Pico-Configtool for now."
        )
        .size(font::sized(12.0))
        .color(Color::from_rgb(0.85, 0.75, 0.45)),
    ]
    .spacing(4)
    .into()
}

fn section_tools<'a>() -> Element<'a, Message> {
    let detect_row = row![
        small_button(
            "🔎 Detect SIDs",
            Message::DeviceConfigAction(DeviceConfigCmd::DetectSids)
        ),
        small_button(
            "🧬 Detect Clones",
            Message::DeviceConfigAction(DeviceConfigCmd::DetectClones)
        ),
        small_button(
            "🔌 Socket detect",
            Message::DeviceConfigAction(DeviceConfigCmd::SocketDetect)
        ),
    ]
    .spacing(8);

    let test_row = row![
        text("Test tones")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        small_button(
            "All",
            Message::DeviceConfigAction(DeviceConfigCmd::TestSid(0))
        ),
        small_button(
            "SID1",
            Message::DeviceConfigAction(DeviceConfigCmd::TestSid(1))
        ),
        small_button(
            "SID2",
            Message::DeviceConfigAction(DeviceConfigCmd::TestSid(2))
        ),
        small_button(
            "SID3",
            Message::DeviceConfigAction(DeviceConfigCmd::TestSid(3))
        ),
        small_button(
            "SID4",
            Message::DeviceConfigAction(DeviceConfigCmd::TestSid(4))
        ),
        small_button(
            "■ Stop",
            Message::DeviceConfigAction(DeviceConfigCmd::StopTests)
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let hw_row = row![
        text("Hardware")
            .size(font::sized(12.0))
            .color(Color::from_rgb(0.55, 0.57, 0.62))
            .width(Length::Fixed(180.0)),
        small_button(
            "⚠ Reset USBSID",
            Message::DeviceConfigAction(DeviceConfigCmd::ResetUsbsid)
        ),
        small_button(
            "Restart bus",
            Message::DeviceConfigAction(DeviceConfigCmd::RestartBus)
        ),
        small_button(
            "Restart bus + CLK",
            Message::DeviceConfigAction(DeviceConfigCmd::RestartBusClk)
        ),
        small_button(
            "Sync PIOs",
            Message::DeviceConfigAction(DeviceConfigCmd::SyncPios)
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    column![section_title("Tools"), detect_row, test_row, hw_row]
        .spacing(6)
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

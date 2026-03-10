// Displays per-voice state decoded from the raw SID register shadow:
//   • Frequency  — Hz and nearest note name (e.g. "A-4  440 Hz")
//   • Waveform   — triangle / sawtooth / pulse / noise (+ test/sync/ring bits)
//   • ADSR       — attack / decay / sustain / release values
//   • Gate       — on / off (lit when voice is gated)
//   • Pulse width — shown only when pulse waveform is active
//
// Also shows the global filter routing and volume for each SID chip.
//
// The panel is purely read-only; all values come from `PlayerStatus::sid_regs`
// which is updated every tick (~33 ms) by the player thread.

use iced::widget::{column, container, row, rule, text, Column, Space};
use iced::{Alignment, Color, Element, Length, Padding, Theme};

use super::Message;

// ─────────────────────────────────────────────────────────────────────────────
//  SID constants
// ─────────────────────────────────────────────────────────────────────────────

/// Bytes allocated per SID chip in the shadow array.
const SID_STRIDE: usize = 0x20;

/// PAL clock frequency in Hz.
const PAL_CLOCK: f64 = 985_248.0;

/// NTSC clock frequency in Hz.
const NTSC_CLOCK: f64 = 1_022_727.0;

// ─────────────────────────────────────────────────────────────────────────────
//  Register decoding helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Decode the 16-bit frequency word for a voice and return Hz.
fn freq_to_hz(lo: u8, hi: u8, is_pal: bool) -> f64 {
    let word = ((hi as u32) << 8) | lo as u32;
    let clock = if is_pal { PAL_CLOCK } else { NTSC_CLOCK };
    word as f64 * clock / 16_777_216.0
}

/// Convert a frequency in Hz to the nearest note name (e.g. "A-4") and
/// the cents deviation from equal temperament.  Returns "---" below 16 Hz.
fn hz_to_note(hz: f64) -> String {
    if hz < 16.0 {
        return "---".to_string();
    }
    let midi = (12.0 * (hz / 440.0).log2() + 69.0).round() as i32;
    let names = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let name = names[midi.rem_euclid(12) as usize];
    let octave = (midi / 12) - 1;
    format!("{}-{}", name, octave)
}

/// Decode the waveform bits from the control register into a short label.
fn waveform_label(ctrl: u8) -> &'static str {
    match (ctrl >> 4) & 0x0F {
        0b0001 => "TRI",
        0b0010 => "SAW",
        0b0100 => "PUL",
        0b1000 => "NOI",
        0b0011 => "TRI+SAW",
        0b0101 => "TRI+PUL",
        0b0110 => "SAW+PUL",
        0b1001 => "NOI+TRI",
        0b1010 => "NOI+SAW",
        0b1100 => "NOI+PUL",
        0b0000 => "---",
        _ => "MULTI",
    }
}

/// Decode ADSR nibble values into readable time labels.
/// The SID ADSR lookup table (approximate ms values).
const ATTACK_TIMES: [&str; 16] = [
    "2ms", "8ms", "16ms", "24ms", "38ms", "56ms", "68ms", "80ms", "100ms", "250ms", "500ms",
    "800ms", "1s", "3s", "5s", "8s",
];
const DECAY_TIMES: [&str; 16] = [
    "6ms", "24ms", "48ms", "72ms", "114ms", "168ms", "204ms", "240ms", "300ms", "750ms", "1.5s",
    "2.4s", "3s", "9s", "15s", "24s",
];
const RELEASE_TIMES: [&str; 16] = [
    "6ms", "24ms", "48ms", "72ms", "114ms", "168ms", "204ms", "240ms", "300ms", "750ms", "1.5s",
    "2.4s", "3s", "9s", "15s", "24s",
];

// ─────────────────────────────────────────────────────────────────────────────
//  Colours
// ─────────────────────────────────────────────────────────────────────────────

/// Per-SID accent colours — same palette as the visualiser.
const SID_ACCENT: [Color; 4] = [
    Color {
        r: 0.30,
        g: 0.85,
        b: 0.55,
        a: 1.0,
    }, // SID1 – green
    Color {
        r: 0.40,
        g: 0.60,
        b: 0.95,
        a: 1.0,
    }, // SID2 – blue
    Color {
        r: 0.90,
        g: 0.55,
        b: 0.30,
        a: 1.0,
    }, // SID3 – orange
    Color {
        r: 0.85,
        g: 0.35,
        b: 0.55,
        a: 1.0,
    }, // SID4 – pink
];

fn dim(c: Color, factor: f32) -> Color {
    Color {
        r: c.r * factor,
        g: c.g * factor,
        b: c.b * factor,
        a: c.a,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Build the SID register info panel.
///
/// `sid_regs`  — 128-byte register shadow from `PlayerStatus`.
///               Empty slice → "no tune playing" placeholder.
/// `num_sids`  — number of SID chips active in the current tune (1–4).
/// `is_pal`    — PAL (true) or NTSC (false), used for Hz calculation.
pub fn sid_panel<'a>(sid_regs: &[u8], num_sids: usize, is_pal: bool) -> Element<'a, Message> {
    // Nothing playing yet — show a friendly placeholder.
    if sid_regs.is_empty() || sid_regs.iter().all(|&b| b == 0) {
        return container(
            text("Load a tune to see SID register state")
                .size(13)
                .color(Color::from_rgb(0.4, 0.4, 0.5)),
        )
        .padding(40)
        .center_x(Length::Fill)
        .into();
    }

    let n = num_sids.clamp(1, 4);
    let mut chips: Vec<Element<'a, Message>> = Vec::with_capacity(n);

    for sid in 0..n {
        chips.push(sid_chip_panel(sid_regs, sid, is_pal));
    }

    // Lay chips out side by side; each gets equal width.
    let content = iced::widget::Row::with_children(
        chips
            .into_iter()
            .enumerate()
            .flat_map(|(i, chip)| {
                if i == 0 {
                    vec![chip]
                } else {
                    vec![rule::vertical(1).into(), chip]
                }
            })
            .collect::<Vec<_>>(),
    )
    .spacing(0)
    .height(Length::Fill);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::from([8, 12]))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.07, 0.08, 0.10))),
            ..Default::default()
        })
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Per-chip panel
// ─────────────────────────────────────────────────────────────────────────────

fn sid_chip_panel<'a>(regs: &[u8], sid: usize, is_pal: bool) -> Element<'a, Message> {
    let base = sid * SID_STRIDE;
    let accent = SID_ACCENT.get(sid).copied().unwrap_or(Color::WHITE);
    let label = if sid == 0 {
        "SID 1".to_string()
    } else {
        format!("SID {}", sid + 1)
    };

    // ── Per-voice rows ───────────────────────────────────────────────────────
    let mut col = Column::new().spacing(6);

    // Chip heading
    col = col.push(text(label).size(12).color(accent));
    col = col.push(rule::horizontal(1));

    for voice in 0..3 {
        col = col.push(voice_row(regs, base, voice, accent, is_pal));
        if voice < 2 {
            col = col.push(rule::horizontal(1));
        }
    }

    // ── Global registers (filter + volume) ──────────────────────────────────
    col = col.push(rule::horizontal(1));
    col = col.push(global_row(regs, base, accent));

    container(col)
        .width(Length::Fill)
        .padding(Padding::from([4, 10]))
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Per-voice row
// ─────────────────────────────────────────────────────────────────────────────

fn voice_row<'a>(
    regs: &[u8],
    base: usize,
    voice: usize,
    accent: Color,
    is_pal: bool,
) -> Element<'a, Message> {
    let vo = base + voice * 7;
    let safe = |i: usize| regs.get(i).copied().unwrap_or(0);

    let freq_lo = safe(vo);
    let freq_hi = safe(vo + 1);
    let pw_lo = safe(vo + 2);
    let pw_hi = safe(vo + 3) & 0x0F;
    let ctrl = safe(vo + 4);
    let ad = safe(vo + 5);
    let sr = safe(vo + 6);

    let gate = ctrl & 0x01 != 0;
    let sync = ctrl & 0x02 != 0;
    let ring = ctrl & 0x04 != 0;
    let test = ctrl & 0x08 != 0;
    let wave = waveform_label(ctrl);

    let hz = freq_to_hz(freq_lo, freq_hi, is_pal);
    let note = hz_to_note(hz);
    let hz_str = if hz < 1.0 {
        "0 Hz".to_string()
    } else {
        format!("{:.0} Hz", hz)
    };

    let attack = (ad >> 4) as usize;
    let decay = (ad & 0x0F) as usize;
    let sustain = (sr >> 4) as usize;
    let release = (sr & 0x0F) as usize;

    let pw_val = ((pw_hi as u16) << 8) | pw_lo as u16;
    let pw_pct = pw_val as f32 / 40.95; // 0..4095 → 0..100%

    // Gate indicator colour
    let gate_color = if gate { accent } else { dim(accent, 0.25) };
    let gate_label = if gate { "GATE" } else { "    " };

    // Voice label colour: dim when silent
    let label_color = if gate { accent } else { dim(accent, 0.4) };
    let voice_label = ["V1", "V2", "V3"][voice];

    // Modifier flags
    let mut flags = String::new();
    if sync {
        flags.push_str(" SYN");
    }
    if ring {
        flags.push_str(" RNG");
    }
    if test {
        flags.push_str(" TST");
    }

    let dim_color = Color::from_rgb(0.45, 0.48, 0.52);
    let val_color = Color::from_rgb(0.85, 0.88, 0.92);

    // Always render the pulse width row at fixed height to prevent layout
    // jumping when the waveform switches. Show value only when pulse is active.
    let pulse_active = (ctrl >> 4) & 0x04 != 0;
    let pulse_row: Element<'a, Message> = row![
        Space::new().width(26),
        label_text(
            "PW",
            if pulse_active {
                dim_color
            } else {
                Color::TRANSPARENT
            }
        ),
        Space::new().width(4),
        label_text(
            if pulse_active {
                format!("{:.0}%  (${:03X})", pw_pct, pw_val)
            } else {
                String::new()
            },
            val_color,
        ),
    ]
    .align_y(Alignment::Center)
    .into();

    column![
        // Row 1: voice label  |  waveform  |  flags  |  GATE
        row![
            text(voice_label)
                .size(12)
                .color(label_color)
                .width(Length::Fixed(20.0)),
            Space::new().width(6),
            text(wave)
                .size(12)
                .color(val_color)
                .width(Length::Fixed(68.0)),
            text(flags).size(11).color(dim_color).width(Length::Fill),
            text(gate_label).size(11).color(gate_color),
        ]
        .align_y(Alignment::Center),
        // Row 2: note name  |  Hz
        row![
            Space::new().width(26),
            text(note)
                .size(12)
                .color(val_color)
                .width(Length::Fixed(40.0)),
            text(hz_str).size(11).color(dim_color),
        ]
        .align_y(Alignment::Center),
        // Row 3: ADSR
        row![
            Space::new().width(26),
            label_text("A", dim_color),
            Space::new().width(2),
            label_text(ATTACK_TIMES[attack], val_color),
            Space::new().width(8),
            label_text("D", dim_color),
            Space::new().width(2),
            label_text(DECAY_TIMES[decay], val_color),
            Space::new().width(8),
            label_text("S", dim_color),
            Space::new().width(2),
            label_text(format!("{}", sustain), val_color),
            Space::new().width(8),
            label_text("R", dim_color),
            Space::new().width(2),
            label_text(RELEASE_TIMES[release], val_color),
        ]
        .align_y(Alignment::Center),
        // Row 4: pulse width (conditional)
        pulse_row,
    ]
    .spacing(3)
    .padding(Padding::from([4, 0]))
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Global registers row (filter + volume)
// ─────────────────────────────────────────────────────────────────────────────

fn global_row<'a>(regs: &[u8], base: usize, accent: Color) -> Element<'a, Message> {
    let safe = |i: usize| regs.get(i).copied().unwrap_or(0);

    // $15 = filter lo (bits 0-2), $16 = filter hi
    let flt_lo = safe(base + 0x15) & 0x07;
    let flt_hi = safe(base + 0x16);
    let flt_word = ((flt_hi as u16) << 3) | flt_lo as u16; // 0..2047

    // $17 = resonance (hi nibble) + voice routing (lo nibble)
    let flt_ctrl = safe(base + 0x17);
    let resonance = (flt_ctrl >> 4) as usize;
    let route_v1 = flt_ctrl & 0x01 != 0;
    let route_v2 = flt_ctrl & 0x02 != 0;
    let route_v3 = flt_ctrl & 0x04 != 0;
    let route_ext = flt_ctrl & 0x08 != 0;

    // $18 = vol (lo nibble) + filter mode (hi nibble)
    let mode_vol = safe(base + 0x18);
    let volume = mode_vol & 0x0F;
    let lp = mode_vol & 0x10 != 0;
    let bp = mode_vol & 0x20 != 0;
    let hp = mode_vol & 0x40 != 0;
    let v3_off = mode_vol & 0x80 != 0;

    // Build filter mode string
    let mut mode = String::new();
    if lp {
        mode.push_str("LP ");
    }
    if bp {
        mode.push_str("BP ");
    }
    if hp {
        mode.push_str("HP ");
    }
    if mode.is_empty() {
        mode.push_str("---");
    }
    let mode = mode.trim_end().to_string();

    // Build routing string
    let mut routing = String::new();
    if route_v1 {
        routing.push_str("V1 ");
    }
    if route_v2 {
        routing.push_str("V2 ");
    }
    if route_v3 {
        routing.push_str("V3 ");
    }
    if route_ext {
        routing.push_str("EXT");
    }
    if routing.is_empty() {
        routing.push_str("none");
    }
    let routing = routing.trim_end().to_string();

    let dim_color = Color::from_rgb(0.45, 0.48, 0.52);
    let val_color = Color::from_rgb(0.85, 0.88, 0.92);
    let hdr_color = dim(accent, 0.7);

    // Always reserve space for the V3OFF badge so the row width stays stable.
    let v3off_badge: Element<'a, Message> = row![
        Space::new().width(10),
        label_text(
            if v3_off { "V3OFF" } else { "     " },
            if v3_off {
                Color::from_rgb(0.9, 0.5, 0.3)
            } else {
                Color::TRANSPARENT
            },
        ),
    ]
    .into();

    column![
        text("GLOBAL").size(11).color(hdr_color),
        row![
            label_text("VOL", dim_color),
            Space::new().width(4),
            label_text(&format!("{}/15", volume), val_color),
            Space::new().width(10),
            label_text("FLT", dim_color),
            Space::new().width(4),
            label_text(&format!("${:03X}", flt_word), val_color),
            Space::new().width(10),
            label_text("RES", dim_color),
            Space::new().width(4),
            label_text(&format!("{}/15", resonance), val_color),
        ]
        .align_y(Alignment::Center),
        row![
            label_text("MODE", dim_color),
            Space::new().width(4),
            label_text(&mode, val_color),
            Space::new().width(10),
            label_text("ROUTE", dim_color),
            Space::new().width(4),
            label_text(&routing, val_color),
            v3off_badge,
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(3)
    .padding(Padding::from([4, 0]))
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper
// ─────────────────────────────────────────────────────────────────────────────

/// Tiny helper: coloured text at size 11, no extra allocations.
fn label_text<'a>(s: impl ToString, color: Color) -> Element<'a, Message> {
    text(s.to_string()).size(11).color(color).into()
}

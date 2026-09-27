//! "Pedalboard" tab: Quad-Cortex-style grid of effect lanes.
//!
//! One row per SID lane plus MASTER; each block is a pedal card with a
//! bypass footswitch. Clicking a card opens its knobs in the editor
//! below. All state lives in `App` (`LiveBoard` + preset store); this
//! module only renders and emits [`PedalMsg`].

use iced::widget::{
    button, column, container, mouse_area, pick_list, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Theme};

use super::knob::knob;
use super::{font, Message};
use crate::dsp::presets::PedalboardStore;
use crate::dsp::{BlockKind, Category, LiveBoard, ParamDef, MASTER, SID_LANES};

#[derive(Debug, Clone)]
pub enum PedalMsg {
    Toggle,
    Select(u64),
    OpenPicker(usize),
    ClosePicker,
    Add(usize, BlockKind),
    Remove(u64),
    Move(u64, isize),
    Bypass(u64, bool),
    BypassAll(bool),
    Param(u64, usize, f32),
    LaneGain(usize, f32),
    LanePan(usize, Option<f32>),
    PresetPicked(String),
    PresetNameChanged(String),
    SavePreset,
    DeletePreset,
}

fn m(p: PedalMsg) -> Message {
    Message::Pedal(p)
}

pub struct PedalView<'a> {
    pub board: &'a LiveBoard,
    pub store: &'a PedalboardStore,
    pub selected: Option<u64>,
    pub picker: Option<usize>,
    pub preset_name: &'a str,
    /// Some(true) = effects apply, Some(false) = hardware engine,
    /// None = "auto" (applies only if it falls back to software).
    pub engine_supported: Option<bool>,
    /// SID chips used by the current tune (lanes beyond are dimmed).
    pub active_sids: usize,
}

const MUTED: Color = Color {
    r: 0.55,
    g: 0.57,
    b: 0.62,
    a: 1.0,
};
const FG: Color = Color {
    r: 0.85,
    g: 0.87,
    b: 0.9,
    a: 1.0,
};

pub fn category_color(c: Category) -> Color {
    match c {
        Category::Core => Color::from_rgb(0.35, 0.60, 0.95),
        Category::Time => Color::from_rgb(0.35, 0.80, 0.55),
        Category::Character => Color::from_rgb(0.95, 0.60, 0.25),
        Category::Stereo => Color::from_rgb(0.70, 0.50, 0.95),
    }
}

fn lane_name(lane: usize) -> String {
    if lane == MASTER {
        "MASTER".into()
    } else {
        format!("SID {}", lane + 1)
    }
}

fn btn<'a>(label: impl Into<String>, msg: Option<Message>) -> Element<'a, Message> {
    let mut b = button(text(label.into()).size(font::sized(12.0)))
        .padding(Padding::from([4, 10]))
        .style(|_t: &Theme, st| button::Style {
            background: Some(Background::Color(match st {
                button::Status::Hovered => Color::from_rgb(0.25, 0.27, 0.32),
                button::Status::Pressed => Color::from_rgb(0.18, 0.20, 0.24),
                button::Status::Disabled => Color::from_rgb(0.13, 0.14, 0.16),
                _ => Color::from_rgb(0.18, 0.19, 0.22),
            })),
            text_color: if matches!(st, button::Status::Disabled) {
                MUTED
            } else {
                FG
            },
            border: Border {
                radius: 3.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            ..Default::default()
        });
    if let Some(msg) = msg {
        b = b.on_press(msg);
    }
    b.into()
}

/// Round footswitch LED: lit = effect engaged.
fn footswitch<'a>(on: bool, color: Color, msg: Message) -> Element<'a, Message> {
    button(
        Space::new()
            .width(Length::Fixed(10.0))
            .height(Length::Fixed(10.0)),
    )
    .padding(0)
    .on_press(msg)
    .style(move |_t: &Theme, _st| button::Style {
        background: Some(Background::Color(if on {
            color
        } else {
            Color::from_rgb(0.2, 0.2, 0.22)
        })),
        border: Border {
            radius: 5.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.05, 0.05, 0.06),
        },
        ..Default::default()
    })
    .into()
}

fn meter<'a>(level: f32) -> Element<'a, Message> {
    let h = 44.0;
    let lvl = level.clamp(0.0, 1.0);
    let color = if level > 0.98 {
        Color::from_rgb(0.95, 0.3, 0.3)
    } else if level > 0.7 {
        Color::from_rgb(0.95, 0.8, 0.3)
    } else {
        Color::from_rgb(0.35, 0.85, 0.5)
    };
    container(column![
        Space::new().height(Length::Fixed(h * (1.0 - lvl))),
        container(
            Space::new()
                .width(Length::Fixed(4.0))
                .height(Length::Fixed(h * lvl))
        )
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(color)),
            ..Default::default()
        }),
    ])
    .style(|_t: &Theme| container::Style {
        background: Some(Background::Color(Color::from_rgb(0.08, 0.08, 0.1))),
        ..Default::default()
    })
    .into()
}

pub fn pedalboard_panel<'a>(v: PedalView<'a>) -> Element<'a, Message> {
    let spec = v.board.spec();

    // ── Header: presets + bypass all ────────────────────────────────────
    let names: Vec<String> = v.store.all().into_iter().map(|p| p.name).collect();
    let can_delete = v
        .store
        .active_name
        .as_deref()
        .is_some_and(|n| v.store.presets.iter().any(|p| p.name == n));
    let load = v.board.dsp_load.value() * 100.0;
    let load_color = if load > 60.0 {
        Color::from_rgb(0.95, 0.35, 0.35)
    } else if load > 25.0 {
        Color::from_rgb(0.95, 0.75, 0.30)
    } else {
        MUTED
    };
    let header = row![
        text("🎛 Pedalboard").size(font::sized(18.0)).color(FG),
        text(format!("DSP {load:.1}%"))
            .size(font::sized(11.0))
            .color(load_color),
        Space::new().width(Length::Fixed(16.0)),
        pick_list(names, v.store.active_name.clone(), |n| m(
            PedalMsg::PresetPicked(n)
        ))
        .placeholder("Preset…")
        .text_size(font::sized(12.0)),
        text_input("Preset name", v.preset_name)
            .on_input(|s| m(PedalMsg::PresetNameChanged(s)))
            .on_submit(m(PedalMsg::SavePreset))
            .size(font::sized(12.0))
            .width(Length::Fixed(150.0)),
        btn(
            "💾 Save",
            (!v.preset_name.trim().is_empty()).then(|| m(PedalMsg::SavePreset))
        ),
        btn("Delete", can_delete.then(|| m(PedalMsg::DeletePreset))),
        Space::new().width(Length::Fill),
        btn(
            if spec.bypass_all {
                "⏻ All bypassed"
            } else {
                "⏻ Bypass all"
            },
            Some(m(PedalMsg::BypassAll(!spec.bypass_all))),
        ),
        btn("✕ Close", Some(m(PedalMsg::Toggle))),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut body = column![header].spacing(10);

    let note = match v.engine_supported {
        Some(false) => Some((
            "Effects apply to the software engines (SIDLite / reSID) only — this engine outputs \
             audio from hardware. Your board is kept and will apply when you switch engine.",
            Color::from_rgb(0.85, 0.65, 0.30),
        )),
        None => Some((
            "Engine is Auto: effects apply when playback falls back to a software engine.",
            MUTED,
        )),
        Some(true) if cfg!(debug_assertions) => Some((
            "Debug build — effects use more CPU than in a release build.",
            MUTED,
        )),
        Some(true) => None,
    };
    if let Some((msg, color)) = note {
        body = body.push(text(msg).size(font::sized(11.0)).color(color));
    }

    // ── Lanes ───────────────────────────────────────────────────────────
    for lane in (0..SID_LANES).chain(std::iter::once(MASTER)) {
        let used = lane == MASTER || lane < v.active_sids.max(1);
        body = body.push(lane_row(&v, lane, used));
        if v.picker == Some(lane) {
            body = body.push(picker(lane));
        }
    }

    // ── Editor ──────────────────────────────────────────────────────────
    body = body.push(iced::widget::rule::horizontal(1));
    body = body.push(editor(&v));

    container(scrollable(body.padding(Padding::from([16, 24]))))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

fn lane_row<'a>(v: &PedalView<'a>, lane: usize, used: bool) -> Element<'a, Message> {
    let spec = v.board.spec().lane(lane);
    let label_color = if used { FG } else { MUTED };
    let level = v.board.meters[lane].value();

    let label = column![
        text(lane_name(lane))
            .size(font::sized(13.0))
            .color(label_color),
        text(if used { "" } else { "unused" })
            .size(font::sized(10.0))
            .color(MUTED),
    ]
    .width(Length::Fixed(72.0));

    let mut chain = row![].spacing(6).align_y(Alignment::Center);
    for (i, b) in spec.blocks.iter().enumerate() {
        if i > 0 {
            chain = chain.push(text("→").color(MUTED));
        }
        chain = chain.push(pedal_card(
            b.id,
            b.kind,
            b.bypass,
            v.selected == Some(b.id),
            used,
        ));
    }
    if !spec.blocks.is_empty() {
        chain = chain.push(text("→").color(MUTED));
    }
    let open = v.picker == Some(lane);
    chain = chain.push(btn(
        if open { "✕" } else { "+" },
        Some(m(if open {
            PedalMsg::ClosePicker
        } else {
            PedalMsg::OpenPicker(lane)
        })),
    ));

    // Lane-out: gain + pan (auto routing unless the user takes over).
    let gain_norm = (spec.gain_db + 24.0) / 36.0;
    let pan_auto = spec.pan.is_none();
    let pan_norm = (spec.pan.unwrap_or(0.0) + 1.0) / 2.0;
    let out_color = Color::from_rgb(0.75, 0.77, 0.82);
    let lane_out = row![
        column![
            knob(gain_norm, 24.0 / 36.0, out_color, !used, 34.0, move |n| m(
                PedalMsg::LaneGain(lane, n * 36.0 - 24.0)
            )),
            text(format!("{:+.1} dB", spec.gain_db))
                .size(font::sized(10.0))
                .color(MUTED),
        ]
        .align_x(Alignment::Center),
        column![
            knob(
                pan_norm,
                0.5,
                out_color,
                pan_auto || !used,
                34.0,
                move |n| m(PedalMsg::LanePan(lane, Some(n * 2.0 - 1.0)))
            ),
            if pan_auto {
                Element::from(text("auto").size(font::sized(10.0)).color(MUTED))
            } else {
                mouse_area(
                    text(pan_label(spec.pan.unwrap_or(0.0)) + " ↺")
                        .size(font::sized(10.0))
                        .color(MUTED),
                )
                .on_press(m(PedalMsg::LanePan(lane, None)))
                .into()
            },
        ]
        .align_x(Alignment::Center),
        meter(level),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    container(
        row![
            label,
            scrollable(chain)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new().width(4).scroller_width(4)
                ))
                .width(Length::Fill),
            lane_out
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([8, 10]))
    .style(|_t: &Theme| container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.13, 0.15))),
        border: Border {
            radius: 6.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.18, 0.19, 0.22),
        },
        ..Default::default()
    })
    .into()
}

fn pan_label(p: f32) -> String {
    if p.abs() < 0.02 {
        "C".into()
    } else if p < 0.0 {
        format!("L{:.0}", -p * 100.0)
    } else {
        format!("R{:.0}", p * 100.0)
    }
}

fn pedal_card<'a>(
    id: u64,
    kind: BlockKind,
    bypass: bool,
    selected: bool,
    used: bool,
) -> Element<'a, Message> {
    let color = category_color(kind.category());
    let engaged = !bypass;
    let title_color = if engaged && used { FG } else { MUTED };
    let card = container(
        column![
            container(Space::new().width(Length::Fill).height(Length::Fixed(4.0))).style(
                move |_t: &Theme| {
                    container::Style {
                        background: Some(Background::Color(if engaged {
                            color
                        } else {
                            Color { a: 0.3, ..color }
                        })),
                        border: Border {
                            radius: 2.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                }
            ),
            text(kind.label())
                .size(font::sized(12.0))
                .color(title_color),
            row![
                Space::new().width(Length::Fill),
                footswitch(engaged, color, m(PedalMsg::Bypass(id, engaged))),
                Space::new().width(Length::Fill),
            ],
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .width(Length::Fixed(104.0))
    .padding(Padding::from([6, 8]))
    .style(move |_t: &Theme| container::Style {
        background: Some(Background::Color(if selected {
            Color::from_rgb(0.19, 0.20, 0.24)
        } else {
            Color::from_rgb(0.15, 0.16, 0.19)
        })),
        border: Border {
            radius: 6.0.into(),
            width: if selected { 2.0 } else { 1.0 },
            color: if selected {
                color
            } else {
                Color::from_rgb(0.24, 0.25, 0.29)
            },
        },
        ..Default::default()
    });
    mouse_area(card).on_press(m(PedalMsg::Select(id))).into()
}

fn picker<'a>(lane: usize) -> Element<'a, Message> {
    let mut cols = row![text(format!("Add to {}:", lane_name(lane)))
        .size(font::sized(12.0))
        .color(MUTED)]
    .spacing(16);
    for cat in [
        Category::Core,
        Category::Time,
        Category::Character,
        Category::Stereo,
    ] {
        let color = category_color(cat);
        let mut col = column![text(cat.label()).size(font::sized(11.0)).color(color)].spacing(4);
        for kind in BlockKind::ALL.into_iter().filter(|k| k.category() == cat) {
            col = col.push(btn(kind.label(), Some(m(PedalMsg::Add(lane, kind)))));
        }
        cols = cols.push(col);
    }
    container(cols)
        .padding(10)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb(0.11, 0.12, 0.14))),
            border: Border {
                radius: 6.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.25, 0.27, 0.30),
            },
            ..Default::default()
        })
        .into()
}

fn norm(d: &ParamDef, v: f32) -> f32 {
    if d.log && d.min > 0.0 {
        (v / d.min).ln() / (d.max / d.min).ln()
    } else {
        (v - d.min) / (d.max - d.min)
    }
}

fn denorm(d: &ParamDef, n: f32) -> f32 {
    let n = n.clamp(0.0, 1.0);
    if d.log && d.min > 0.0 {
        d.min * (d.max / d.min).powf(n)
    } else {
        d.min + (d.max - d.min) * n
    }
}

fn fmt_value(d: &ParamDef, v: f32) -> String {
    match d.unit {
        "Hz" if v >= 1000.0 => format!("{:.1} kHz", v / 1000.0),
        "Hz" => format!("{v:.0} Hz"),
        "ms" if v >= 100.0 => format!("{v:.0} ms"),
        "ms" => format!("{v:.1} ms"),
        "dB" => format!("{v:+.1} dB"),
        "" if d.min >= 0.0 && d.max <= 1.0 => format!("{:.0}%", v * 100.0),
        "" => format!("{v:.2}"),
        u => format!("{v:.1} {u}"),
    }
}

fn editor<'a>(v: &PedalView<'a>) -> Element<'a, Message> {
    let Some((id, lane, b)) = v.selected.and_then(|id| {
        let (lane, _) = v.board.locate(id)?;
        Some((id, lane, v.board.block(id)?))
    }) else {
        return text("Select a pedal to edit it, or press + on a lane to add one. Drag a knob up/down (or scroll); double-click resets it.")
            .size(font::sized(12.0))
            .color(MUTED)
            .into();
    };
    let color = category_color(b.kind.category());
    let engaged = !b.bypass;

    let head = row![
        text(b.kind.label()).size(font::sized(16.0)).color(color),
        text(format!("· {}", lane_name(lane)))
            .size(font::sized(13.0))
            .color(MUTED),
        Space::new().width(Length::Fixed(12.0)),
        footswitch(engaged, color, m(PedalMsg::Bypass(id, engaged))),
        text(if engaged { "On" } else { "Bypassed" })
            .size(font::sized(11.0))
            .color(MUTED),
        Space::new().width(Length::Fill),
        btn("◀ Move", Some(m(PedalMsg::Move(id, -1)))),
        btn("Move ▶", Some(m(PedalMsg::Move(id, 1)))),
        btn("🗑 Remove", Some(m(PedalMsg::Remove(id)))),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut knobs = row![].spacing(18).align_y(Alignment::Start);
    for (i, d) in b.kind.params().iter().enumerate() {
        let val = b.params[i];
        let control: Element<'a, Message> = if let Some(steps) = d.steps {
            let mut opts = column![].spacing(4);
            for s in 0..steps {
                let label = b.kind.step_label(i, s as f32).unwrap_or("?");
                let active = val.round() as u8 == s;
                opts = opts.push(btn(
                    if active {
                        format!("● {label}")
                    } else {
                        label.to_string()
                    },
                    Some(m(PedalMsg::Param(id, i, s as f32))),
                ));
            }
            opts.into()
        } else {
            let d2 = *d;
            column![
                knob(
                    norm(d, val),
                    norm(d, d.default),
                    color,
                    !engaged,
                    56.0,
                    move |n| { m(PedalMsg::Param(id, i, denorm(&d2, n))) }
                ),
                text(fmt_value(d, val)).size(font::sized(11.0)).color(FG),
            ]
            .spacing(2)
            .align_x(Alignment::Center)
            .into()
        };
        knobs = knobs.push(
            column![text(d.name).size(font::sized(11.0)).color(MUTED), control]
                .spacing(4)
                .align_x(Alignment::Center)
                .width(Length::Fixed(76.0)),
        );
    }

    column![head, knobs].spacing(14).into()
}

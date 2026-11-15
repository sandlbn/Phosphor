//! "Cast" panel: pick a network speaker to send Phosphor's audio to.

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Theme};

use super::{font, Message};
use crate::cast::{CastState, CastTarget, Protocol};

#[derive(Debug, Clone)]
pub enum CastMsg {
    TogglePanel,
    Start(String),
    Stop,
    Reconnect,
    ToggleMuteLocal,
    ToggleLossless,
}

fn m(c: CastMsg) -> Message {
    Message::Cast(c)
}

pub struct CastView<'a> {
    pub devices: &'a [CastTarget],
    pub state: &'a CastState,
    pub mute_local: bool,
    pub lossless: bool,
    pub last_device: Option<&'a str>,
    pub server_port: u16,
    /// Some(true) software engine, Some(false) hardware, None = auto.
    pub engine_supported: Option<bool>,
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
const OK: Color = Color {
    r: 0.45,
    g: 0.85,
    b: 0.55,
    a: 1.0,
};
const WARN: Color = Color {
    r: 0.85,
    g: 0.65,
    b: 0.30,
    a: 1.0,
};
const ERR: Color = Color {
    r: 0.95,
    g: 0.45,
    b: 0.45,
    a: 1.0,
};

fn btn<'a>(label: impl Into<String>, msg: Option<Message>) -> Element<'a, Message> {
    let mut b = button(text(label.into()).size(font::sized(12.0)))
        .padding(Padding::from([4, 12]))
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

pub fn cast_panel<'a>(v: CastView<'a>) -> Element<'a, Message> {
    let header = row![
        text("📡 Cast to a speaker")
            .size(font::sized(18.0))
            .color(FG),
        Space::new().width(Length::Fill),
        btn("✕ Close", Some(m(CastMsg::TogglePanel))),
    ]
    .align_y(Alignment::Center);

    let (status, color): (String, Color) = match v.state {
        CastState::Idle => ("Not casting — audio plays on this computer.".into(), MUTED),
        CastState::Connecting { name, .. } => (format!("Connecting to {name}…"), WARN),
        CastState::Casting { name, .. } => (format!("Casting to {name}"), OK),
        CastState::Error(e) => (format!("Couldn't cast — {e}"), ERR),
    };
    // Text takes the free width and wraps; the button keeps its size.
    let mut status_row = row![text(status)
        .size(font::sized(13.0))
        .color(color)
        .width(Length::Fill)]
    .spacing(10)
    .align_y(Alignment::Center);
    if matches!(
        v.state,
        CastState::Casting { .. } | CastState::Connecting { .. }
    ) {
        status_row = status_row.push(btn("■ Stop casting", Some(m(CastMsg::Stop))));
    } else if let Some(last) = v.last_device {
        let available = v.devices.iter().any(|d| d.name == last);
        if available {
            status_row = status_row.push(btn(
                format!("↻ Reconnect to {last}"),
                Some(m(CastMsg::Reconnect)),
            ));
        }
    }

    let mut body = column![header, status_row].spacing(12);

    match v.engine_supported {
        Some(false) => {
            body = body.push(
                text(
                    "Casting sends the software engines' audio (SIDLite / reSID). The current \
                     engine plays on hardware, so there is nothing to cast — switch engine in Settings.",
                )
                .size(font::sized(11.0))
                .color(WARN),
            )
        }
        None => {
            body = body.push(
                text("Engine is Auto: casting works when playback uses a software engine.")
                    .size(font::sized(11.0))
                    .color(MUTED),
            )
        }
        Some(true) => {}
    }

    // ── Devices ─────────────────────────────────────────────────────────
    if v.devices.is_empty() {
        body = body.push(
            text(
                "Searching for speakers on your network… Chromecast, Nest, Google TV, AirPlay \
                 (HomePod, AirPlay TVs), Sonos and DLNA speakers appear here. Make sure they are on the same network as \
                 this computer.",
            )
            .size(font::sized(12.0))
            .color(MUTED),
        );
    } else {
        let current = match v.state {
            CastState::Casting { id, .. } | CastState::Connecting { id, .. } => Some(id.as_str()),
            _ => None,
        };
        let mut protocols: Vec<Protocol> = v.devices.iter().map(|d| d.protocol).collect();
        protocols.sort();
        protocols.dedup();
        for proto in protocols {
            body = body.push(text(proto.label()).size(font::sized(12.0)).color(MUTED));
            for d in v.devices.iter().filter(|d| d.protocol == proto) {
                let active = current == Some(d.id.as_str());
                let action = if active {
                    btn("■ Stop", Some(m(CastMsg::Stop)))
                } else {
                    btn("Cast", Some(m(CastMsg::Start(d.id.clone()))))
                };
                let row = row![
                    text(if active { "●" } else { "○" }).color(if active { OK } else { MUTED }),
                    column![
                        text(d.name.clone()).size(font::sized(13.0)).color(FG),
                        text(format!("{}  ·  {}", d.model, d.addr))
                            .size(font::sized(10.0))
                            .color(MUTED),
                    ]
                    .width(Length::Fill),
                    action,
                ]
                .spacing(10)
                .align_y(Alignment::Center);
                body = body.push(container(row).padding(Padding::from([6, 10])).style(
                    move |_t: &Theme| container::Style {
                        background: Some(Background::Color(Color::from_rgb(0.12, 0.13, 0.15))),
                        border: Border {
                            radius: 6.0.into(),
                            width: 1.0,
                            color: if active {
                                OK
                            } else {
                                Color::from_rgb(0.18, 0.19, 0.22)
                            },
                        },
                        ..Default::default()
                    },
                ));
            }
        }
    }

    // ── Options + notes ─────────────────────────────────────────────────
    body = body.push(iced::widget::rule::horizontal(1));
    body = body.push(
        row![
            text("Mute this computer while casting")
                .size(font::sized(12.0))
                .color(MUTED),
            Space::new().width(Length::Fixed(12.0)),
            btn(
                if v.mute_local { "On" } else { "Off" },
                Some(m(CastMsg::ToggleMuteLocal))
            ),
        ]
        .align_y(Alignment::Center),
    );
    body = body.push(
        row![
            text("Lossless (FLAC) for Google Cast & DLNA")
                .size(font::sized(12.0))
                .color(MUTED),
            Space::new().width(Length::Fixed(12.0)),
            btn(
                if v.lossless { "On" } else { "Off" },
                Some(m(CastMsg::ToggleLossless))
            ),
        ]
        .align_y(Alignment::Center),
    );
    body = body.push(
        text(
            "Experimental: uses the next time you start casting. Sonos always gets MP3 (its \
             radio streaming only takes MP3); AirPlay is always lossless.",
        )
        .size(font::sized(11.0))
        .color(MUTED),
    );
    body = body.push(
        text(format!(
            "Google Cast, Sonos and DLNA speakers pull a live stream from Phosphor's web \
             server on port {} (started automatically); AirPlay gets lossless audio pushed \
             directly. Expect a 2–6 second delay: pause, skip and effect changes reach \
             the speaker a few seconds late. While casting, the volume slider controls the \
             speaker. If nothing plays, allow Phosphor through your firewall.",
            v.server_port
        ))
        .size(font::sized(11.0))
        .color(MUTED),
    );

    container(scrollable(body.padding(Padding::from([16, 24]))))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb(0.09, 0.10, 0.12))),
            ..Default::default()
        })
        .into()
}

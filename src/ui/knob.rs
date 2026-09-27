//! Rotary knob (iced has none). Drag vertically or scroll to turn;
//! double-click resets to the default. Value is normalised 0..1; the
//! caller maps it to the parameter range.

use std::time::Instant;

use iced::widget::canvas::{self, path, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Radians, Rectangle, Renderer, Theme};

/// Pixels of vertical drag for a full sweep.
const DRAG_RANGE: f32 = 160.0;
const START: f32 = 0.75 * std::f32::consts::PI; // 135°
const SWEEP: f32 = 1.5 * std::f32::consts::PI; // 270°

pub struct Knob<M> {
    value: f32,
    default: f32,
    color: Color,
    dimmed: bool,
    on_change: Box<dyn Fn(f32) -> M>,
}

#[derive(Default)]
pub struct KnobState {
    drag: Option<(f32, f32)>,
    last_click: Option<Instant>,
}

pub fn knob<'a, M: 'a>(
    value: f32,
    default: f32,
    color: Color,
    dimmed: bool,
    size: f32,
    on_change: impl Fn(f32) -> M + 'static,
) -> Element<'a, M> {
    Canvas::new(Knob {
        value: value.clamp(0.0, 1.0),
        default,
        color,
        dimmed,
        on_change: Box::new(on_change),
    })
    .width(Length::Fixed(size))
    .height(Length::Fixed(size))
    .into()
}

impl<M> canvas::Program<M> for Knob<M> {
    type State = KnobState;

    fn update(
        &self,
        state: &mut KnobState,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<M>> {
        let publish =
            |v: f32| canvas::Action::publish((self.on_change)(v.clamp(0.0, 1.0))).and_capture();
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let pos = cursor.position_over(bounds)?;
                let now = Instant::now();
                if state
                    .last_click
                    .is_some_and(|t| now.duration_since(t).as_millis() < 350)
                {
                    state.last_click = None;
                    state.drag = None;
                    return Some(publish(self.default));
                }
                state.last_click = Some(now);
                state.drag = Some((pos.y, self.value));
                Some(canvas::Action::capture())
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let (y0, v0) = state.drag?;
                Some(publish(v0 + (y0 - position.y) / DRAG_RANGE))
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.drag.take().map(|_| canvas::Action::capture())
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_over(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y * 0.03,
                    mouse::ScrollDelta::Pixels { y, .. } => *y * 0.002,
                };
                Some(publish(self.value + dy))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &KnobState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let c = frame.center();
        let r = bounds.width.min(bounds.height) * 0.5 - 3.0;
        let accent = if self.dimmed {
            Color {
                a: 0.35,
                ..self.color
            }
        } else {
            self.color
        };

        let arc = |from: f32, to: f32, radius: f32| {
            Path::new(|b| {
                b.arc(path::Arc {
                    center: c,
                    radius,
                    start_angle: Radians(from),
                    end_angle: Radians(to),
                })
            })
        };
        let track = arc(START, START + SWEEP, r);
        frame.stroke(
            &track,
            Stroke::default()
                .with_width(3.0)
                .with_color(Color::from_rgb(0.22, 0.23, 0.27)),
        );
        if self.value > 0.001 {
            let val = arc(START, START + SWEEP * self.value, r);
            frame.stroke(&val, Stroke::default().with_width(3.0).with_color(accent));
        }

        // Knob body with a subtle ring when being dragged.
        let body_r = r - 5.0;
        frame.fill(&Path::circle(c, body_r), Color::from_rgb(0.14, 0.15, 0.18));
        let ring = if state.drag.is_some() {
            accent
        } else {
            Color::from_rgb(0.28, 0.30, 0.34)
        };
        frame.stroke(
            &Path::circle(c, body_r),
            Stroke::default().with_width(1.0).with_color(ring),
        );

        let a = START + SWEEP * self.value;
        let tip = Point::new(
            c.x + a.cos() * (body_r - 2.0),
            c.y + a.sin() * (body_r - 2.0),
        );
        let base = Point::new(c.x + a.cos() * body_r * 0.35, c.y + a.sin() * body_r * 0.35);
        frame.stroke(
            &Path::line(base, tip),
            Stroke::default()
                .with_width(2.0)
                .with_color(Color::from_rgb(0.9, 0.91, 0.94)),
        );
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &KnobState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.drag.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}

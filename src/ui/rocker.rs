//! A tiny pixel-art rocker for the Pedalboard. Headbangs, strums and
//! taps his foot in time with the master level; idles when playback
//! stops. Click him for a jump + windmill. Pure canvas rects, no assets.
//!
//! Animation is driven by wall-clock time; the app's 33 ms Tick already
//! rebuilds the view, so he runs at ~30 fps with no extra plumbing.

use std::sync::OnceLock;
use std::time::Instant;

use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

/// Pixel size of one art "unit".
const U: f32 = 4.0;
const W: f32 = 22.0 * U;
const H: f32 = 28.0 * U;
/// Top margin (art units) so the jump trick isn't clipped.
const TOP: f32 = 2.0 * U;
const TRICK_SECS: f32 = 1.0;

fn clock() -> f32 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

pub struct Rocker {
    level: f32,
    playing: bool,
}

#[derive(Default)]
pub struct RockerState {
    trick: Option<Instant>,
}

pub fn rocker<'a, M: 'a>(level: f32, playing: bool) -> Element<'a, M> {
    Canvas::new(Rocker { level, playing })
        .width(Length::Fixed(W))
        .height(Length::Fixed(H))
        .into()
}

/// Draws in art units relative to an origin, snapping to the pixel grid
/// so motion stays chunky and retro.
struct Px<'f> {
    f: &'f mut Frame,
    ox: f32,
    oy: f32,
}

impl Px<'_> {
    fn r(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.f.fill_rectangle(
            Point::new(self.ox + x.round() * U, self.oy + y.round() * U),
            Size::new(w * U, h * U),
            c,
        );
    }
}

impl<M> canvas::Program<M> for Rocker {
    type State = RockerState;

    fn update(
        &self,
        state: &mut RockerState,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<M>> {
        if let canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            cursor.position_over(bounds)?;
            state.trick = Some(Instant::now());
            return Some(canvas::Action::request_redraw().and_capture());
        }
        None
    }

    fn draw(
        &self,
        state: &RockerState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let t = clock();
        let tau = std::f32::consts::TAU;

        // How hard he rocks: 0 idle … 1 full headbang.
        let drive = if self.playing {
            (self.level * 1.8).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let speed = 1.2 + drive * 1.6; // beats per second
        let beat = t * speed;
        let wave = (beat * tau).sin();

        // Jump + windmill trick on click.
        let trick = state
            .trick
            .map(|s| s.elapsed().as_secs_f32() / TRICK_SECS)
            .filter(|p| *p < 1.0);
        let jump = trick.map_or(0.0, |p| -(p * std::f32::consts::PI).sin() * 4.0);

        let bang = if drive > 0.05 {
            wave.max(0.0) * 2.0 * drive
        } else if (t * 0.4).fract() < 0.08 {
            1.0
        } else {
            0.0
        };
        let swing = if drive > 0.05 {
            (beat * tau).cos() * drive * 1.5
        } else {
            0.0
        };
        let strum_down = drive > 0.05 && (beat * 2.0).fract() < 0.5;
        let tap = self.playing && beat.fract() < 0.3;

        let (skin, hair, shirt, jeans, shoe) = (
            rgb(238, 190, 150),
            rgb(92, 56, 28),
            rgb(28, 28, 34),
            rgb(64, 90, 170),
            rgb(12, 12, 14),
        );
        let (gtr, gtr_dark, neck) = (rgb(210, 40, 50), rgb(120, 20, 28), rgb(150, 100, 50));

        // ── Amp (right) ─────────────────────────────────────────────────
        {
            let mut p = Px {
                f: &mut frame,
                ox: 0.0,
                oy: TOP,
            };
            p.r(15.0, 16.0, 7.0, 10.0, rgb(40, 40, 46));
            p.r(15.0, 16.0, 7.0, 1.0, rgb(70, 70, 78));
            for k in 0..3 {
                p.r(16.0 + k as f32 * 2.0, 16.0, 1.0, 1.0, rgb(200, 200, 210));
            }
            p.r(16.0, 18.0, 5.0, 7.0, rgb(24, 24, 28));
        }
        let pulse = if self.playing {
            7.0 + self.level.min(1.0) * 4.0
        } else {
            7.0
        };
        frame.fill(
            &Path::circle(Point::new(18.5 * U, TOP + 21.5 * U), pulse),
            rgb(58, 58, 66),
        );
        frame.fill(
            &Path::circle(Point::new(18.5 * U, TOP + 21.5 * U), 3.0),
            rgb(90, 90, 100),
        );

        // ── Notes floating up from the amp ──────────────────────────────
        if drive > 0.1 {
            for k in 0..3 {
                let ph = (t * 0.55 + k as f32 / 3.0).fract();
                let x = 17.0 + (ph * 7.0 + k as f32 * 2.1).sin() * 1.5 + k as f32 * 1.2;
                let y = 14.0 - ph * 13.0;
                let c = Color {
                    a: (1.0 - ph) * drive.min(1.0),
                    ..rgb(255, 230, 90)
                };
                let mut p = Px {
                    f: &mut frame,
                    ox: 0.0,
                    oy: TOP,
                };
                p.r(x, y + 2.0, 2.0, 1.0, c); // head
                p.r(x + 1.0, y, 1.0, 2.0, c); // stem
                p.r(x + 2.0, y, 1.0, 1.0, c); // flag
            }
        }

        // ── Rocker ──────────────────────────────────────────────────────
        let mut p = Px {
            f: &mut frame,
            ox: 0.0,
            oy: TOP + jump * U,
        };

        // Legs + shoes (right foot taps).
        p.r(5.0, 18.0, 2.0, 7.0, jeans);
        p.r(9.0, 18.0, 2.0, 7.0 - if tap { 1.0 } else { 0.0 }, jeans);
        p.r(4.0, 25.0, 3.0, 1.0, shoe);
        p.r(9.0, if tap { 24.0 } else { 25.0 }, 3.0, 1.0, shoe);

        // Torso.
        p.r(4.0, 10.0, 8.0, 8.0, shirt);
        p.r(7.0, 12.0, 1.0, 1.0, rgb(250, 210, 60)); // lightning bolt tee
        p.r(8.0, 13.0, 1.0, 1.0, rgb(250, 210, 60));

        // Head (bangs forward + swings), hair, shades.
        let (hx, hy) = (swing * 0.5, bang);
        p.r(7.0 + hx, 9.0, 2.0, 1.0, skin); // neck
        p.r(5.0 + hx, 3.0 + hy, 6.0, 6.0, skin);
        p.r(4.0 + hx, 2.0 + hy, 8.0, 2.0, hair);
        let flow = 6.0 + (hy * 0.8).round();
        p.r(4.0 + hx - swing.max(0.0), 3.0 + hy, 1.0, flow, hair);
        p.r(11.0 + hx - swing.min(0.0), 3.0 + hy, 1.0, flow, hair);
        p.r(5.0 + hx, 5.0 + hy, 6.0, 1.0, shoe); // shades
        p.r(8.0 + hx, 7.0 + hy, 2.0, 1.0, rgb(150, 60, 60)); // grin

        // Guitar body across the torso, neck to the upper left.
        p.r(6.0, 14.0, 5.0, 3.0, gtr);
        p.r(7.0, 13.0, 3.0, 1.0, gtr);
        p.r(8.0, 15.0, 1.0, 1.0, gtr_dark);
        for i in 0..5 {
            p.r(5.0 - i as f32, 13.0 - i as f32, 1.0, 1.0, neck);
        }
        p.r(-0.0, 8.0, 1.0, 1.0, rgb(230, 230, 235)); // headstock

        // Fretting arm.
        p.r(3.0, 11.0, 1.0, 2.0, shirt);
        p.r(2.0, 11.0, 1.0, 1.0, skin);

        // Strumming arm: windmill during the trick, else up/down strums.
        p.r(12.0, 10.0, 1.0, 2.0, shirt);
        match trick {
            Some(ph) => {
                let a = ph * tau * 2.0;
                p.r(12.0 + a.cos() * 2.0, 11.0 + a.sin() * 3.0, 1.0, 1.0, skin);
            }
            None if strum_down => {
                p.r(12.0, 12.0, 1.0, 2.0, skin);
                p.r(11.0, 15.0, 1.0, 1.0, skin);
            }
            None => {
                p.r(12.0, 12.0, 1.0, 1.0, skin);
                p.r(11.0, 13.0, 1.0, 1.0, skin);
            }
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _s: &RockerState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

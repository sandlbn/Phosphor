// visualizer.rs — SID voice level visualiser with two display modes:
//
//   Bar mode      — vertical bars per voice with peak-hold indicators,
//                   coloured by SID chip (green / blue / orange / pink).
//
//   Scope mode    — oscilloscope lines drawn from a rolling history of
//                   level samples.  Each voice gets its own lane; the
//                   waveform scrolls left as new data arrives, giving a
//                   natural attack / sustain / release envelope shape.
//
// Click anywhere on the widget to toggle between modes.
// The chosen mode is remembered for the lifetime of the session.

use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Size, Theme};

// ─────────────────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum bars / voices we will ever show (3 voices × 4 SIDs max).
const MAX_BARS: usize = 12;

/// Number of history samples kept per voice for the oscilloscope.
/// At ~30 fps this gives roughly 3 seconds of scrolling history.
const SCOPE_HISTORY: usize = 128;

/// Bar-mode: decay factor applied each frame when the level is falling.
const DECAY: f32 = 0.92;

/// Bar-mode: slower decay for the peak-hold dot.
const PEAK_DECAY: f32 = 0.985;

/// Bar-mode: minimum bar height (fraction of full height) so silent voices
/// show a faint slot rather than disappearing entirely.
const MIN_BAR_HEIGHT: f32 = 0.02;

/// Scope-mode: line stroke width in logical pixels.
const SCOPE_LINE_WIDTH: f32 = 1.5;

/// Scope-mode: fraction of the total widget height allocated to each voice
/// lane (voices share height equally, but this constant makes it explicit).
const SCOPE_LANE_PADDING: f32 = 0.08;

// ─────────────────────────────────────────────────────────────────────────────
//  Display mode
// ─────────────────────────────────────────────────────────────────────────────

/// Which visualiser style is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisMode {
    /// Vertical bar chart with peak-hold indicators.
    Bars,
    /// Scrolling oscilloscope lines, one lane per voice.
    Scope,
}

impl VisMode {
    /// Toggle between the two modes.
    pub fn toggle(self) -> Self {
        match self {
            Self::Bars => Self::Scope,
            Self::Scope => Self::Bars,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Colour palette
// ─────────────────────────────────────────────────────────────────────────────

/// Per-SID, per-voice colour palette (C64-ish hues).
/// Indexed as `SID_COLORS[sid_index][voice_index]`.
const SID_COLORS: [[Color; 3]; 4] = [
    [
        Color {
            r: 0.30,
            g: 0.85,
            b: 0.55,
            a: 1.0,
        }, // SID1 V1 – green
        Color {
            r: 0.25,
            g: 0.75,
            b: 0.50,
            a: 1.0,
        }, // SID1 V2
        Color {
            r: 0.20,
            g: 0.65,
            b: 0.45,
            a: 1.0,
        }, // SID1 V3
    ],
    [
        Color {
            r: 0.40,
            g: 0.60,
            b: 0.95,
            a: 1.0,
        }, // SID2 V1 – blue
        Color {
            r: 0.35,
            g: 0.55,
            b: 0.85,
            a: 1.0,
        },
        Color {
            r: 0.30,
            g: 0.50,
            b: 0.75,
            a: 1.0,
        },
    ],
    [
        Color {
            r: 0.90,
            g: 0.55,
            b: 0.30,
            a: 1.0,
        }, // SID3 V1 – orange
        Color {
            r: 0.80,
            g: 0.50,
            b: 0.25,
            a: 1.0,
        },
        Color {
            r: 0.70,
            g: 0.45,
            b: 0.20,
            a: 1.0,
        },
    ],
    [
        Color {
            r: 0.85,
            g: 0.35,
            b: 0.55,
            a: 1.0,
        }, // SID4 V1 – pink
        Color {
            r: 0.75,
            g: 0.30,
            b: 0.50,
            a: 1.0,
        },
        Color {
            r: 0.65,
            g: 0.25,
            b: 0.45,
            a: 1.0,
        },
    ],
];

/// Retrieve the colour for a given flat voice index (0–11).
#[inline]
fn voice_color(flat_idx: usize) -> Color {
    let sid = flat_idx / 3;
    let voice = flat_idx % 3;
    SID_COLORS
        .get(sid)
        .and_then(|s| s.get(voice))
        .copied()
        .unwrap_or(Color::from_rgb(0.5, 0.5, 0.5))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Visualizer struct
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Visualizer {
    // ── Bar mode state ──────────────────────────────────────────────────────
    /// Smoothed bar heights (0.0–1.0), one per voice.
    bars: Vec<f32>,
    /// Peak-hold values (0.0–1.0), one per voice.
    peaks: Vec<f32>,

    // ── Scope mode state ────────────────────────────────────────────────────
    /// Circular history buffer of raw level samples, one ring per voice.
    /// `scope_history[voice][frame]` = level at that frame.
    scope_history: Vec<Vec<f32>>,
    /// Write cursor into each voice's ring buffer.
    scope_cursor: usize,

    // ── Shared state ────────────────────────────────────────────────────────
    /// Number of SID chips in the current tune (1–4).
    /// Determines how many bars / lanes are drawn: `num_sids × 3`.
    num_sids: usize,
    /// Current display mode (bar or scope).
    pub mode: VisMode,
    /// iced canvas cache — cleared whenever data changes.
    cache: Cache,
}

impl Visualizer {
    /// Create a new visualiser starting in Bar mode.
    pub fn new() -> Self {
        Self {
            bars: vec![0.0; MAX_BARS],
            peaks: vec![0.0; MAX_BARS],
            scope_history: vec![vec![0.0; SCOPE_HISTORY]; MAX_BARS],
            scope_cursor: 0,
            num_sids: 1,
            mode: VisMode::Bars,
            cache: Cache::new(),
        }
    }

    /// Set the number of SIDs for the current tune.
    /// Call this whenever a new track starts so the layout updates.
    pub fn set_num_sids(&mut self, n: usize) {
        self.num_sids = n.clamp(1, 4);
        self.cache.clear();
    }

    /// Feed a new frame of voice levels from the player.
    /// `levels` is a flat slice: [SID1V1, SID1V2, SID1V3, SID2V1, …].
    /// Values are expected in the range 0.0–1.0.
    pub fn update(&mut self, levels: &[f32]) {
        let n = self.bar_count();
        for i in 0..MAX_BARS {
            let new_val = levels.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);

            // ── Bar mode bookkeeping ────────────────────────────────────────
            // Rise immediately on a new peak, decay gradually when falling.
            if new_val > self.bars[i] {
                self.bars[i] = new_val;
            } else {
                self.bars[i] *= DECAY;
            }
            // Peak-hold: rise instantly, decay slowly.
            if self.bars[i] > self.peaks[i] {
                self.peaks[i] = self.bars[i];
            } else {
                self.peaks[i] *= PEAK_DECAY;
            }

            // ── Scope mode bookkeeping ──────────────────────────────────────
            // Only record voices that are active in the current tune; the
            // rest stay at zero so their lanes render as a flat baseline.
            let sample = if i < n { new_val } else { 0.0 };
            self.scope_history[i][self.scope_cursor] = sample;
        }

        // Advance the shared write cursor (all voices share the same timeline).
        self.scope_cursor = (self.scope_cursor + 1) % SCOPE_HISTORY;
        self.cache.clear();
    }

    /// Reset all state to silence (call on Stop or track change).
    pub fn reset(&mut self) {
        self.bars.fill(0.0);
        self.peaks.fill(0.0);
        for lane in &mut self.scope_history {
            lane.fill(0.0);
        }
        self.scope_cursor = 0;
        self.num_sids = 1;
        self.cache.clear();
    }

    /// Toggle between Bar and Scope display modes.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        self.cache.clear();
    }

    /// Number of voices (bars / lanes) to draw for the current tune.
    fn bar_count(&self) -> usize {
        self.num_sids * 3
    }

    /// Return the iced Element to embed in the track-info bar.
    /// Clicking the widget toggles between Bar and Scope modes.
    pub fn view(&self) -> Element<'_, super::Message> {
        Canvas::new(self)
            .width(Length::Fill)
            .height(Length::Fixed(60.0))
            .into()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Canvas program implementation
// ─────────────────────────────────────────────────────────────────────────────

impl canvas::Program<super::Message> for &Visualizer {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geom = self
            .cache
            .draw(renderer, bounds.size(), |frame| match self.mode {
                VisMode::Bars => draw_bars(self, frame, bounds),
                VisMode::Scope => draw_scope(self, frame, bounds),
            });
        vec![geom]
    }

    /// Clicking the canvas fires a ToggleVisMode message.
    fn update(
        &self,
        _state: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<super::Message>> {
        if let canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) = event {
            if cursor.is_over(bounds) {
                return Some(canvas::Action::publish(super::Message::ToggleVisMode));
            }
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Bar mode drawing
// ─────────────────────────────────────────────────────────────────────────────

/// Draw the vertical bar chart with peak-hold indicators.
fn draw_bars(vis: &Visualizer, frame: &mut Frame, bounds: Rectangle) {
    let n = vis.bar_count();
    if n == 0 {
        return;
    }

    let w = bounds.width;
    let h = bounds.height;
    let gap = 2.0_f32;
    let bar_w = ((w - gap * (n as f32 - 1.0)) / n as f32).max(4.0);

    // Background
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.08, 0.08, 0.10),
    );

    for i in 0..n {
        let x = i as f32 * (bar_w + gap);
        let level = vis.bars[i].clamp(0.0, 1.0);
        let color = voice_color(i);

        // Dimmed background slot so silent bars are visible
        let dim = Color {
            r: color.r * 0.2,
            g: color.g * 0.2,
            b: color.b * 0.2,
            a: 0.5,
        };
        let min_h = MIN_BAR_HEIGHT * (h - 4.0);
        frame.fill_rectangle(Point::new(x, h - 2.0 - min_h), Size::new(bar_w, min_h), dim);

        // Active bar
        let bar_h = level * (h - 4.0);
        if bar_h > min_h {
            frame.fill_rectangle(
                Point::new(x, h - 2.0 - bar_h),
                Size::new(bar_w, bar_h),
                color,
            );
        }

        // Peak-hold indicator (thin horizontal line above the bar)
        let peak = vis.peaks[i].clamp(0.0, 1.0);
        if peak > 0.01 {
            let peak_y = h - 2.0 - peak * (h - 4.0);
            frame.fill_rectangle(
                Point::new(x, peak_y),
                Size::new(bar_w, 2.0),
                Color { a: 0.85, ..color },
            );
        }
    }

    // Mode hint label (bottom-right, very faint)
    draw_mode_hint(frame, bounds, "SCOPE ▶");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Scope mode drawing
// ─────────────────────────────────────────────────────────────────────────────

/// Draw the scrolling oscilloscope, one lane per active voice.
fn draw_scope(vis: &Visualizer, frame: &mut Frame, bounds: Rectangle) {
    let n = vis.bar_count();
    if n == 0 {
        return;
    }

    let w = bounds.width;
    let h = bounds.height;

    // Background
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.06, 0.07, 0.09),
    );

    // Height allocated to each voice lane
    let lane_h = h / n as f32;

    for i in 0..n {
        let lane_top = i as f32 * lane_h;
        let lane_mid = lane_top + lane_h * 0.5;
        let amplitude = lane_h * (0.5 - SCOPE_LANE_PADDING); // max swing up/down

        let color = voice_color(i);

        // Faint lane separator line
        if i > 0 {
            let sep_color = Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.05,
            };
            frame.fill_rectangle(Point::new(0.0, lane_top), Size::new(w, 1.0), sep_color);
        }

        // Faint centre line for each lane
        let centre_color = Color {
            r: color.r * 0.25,
            g: color.g * 0.25,
            b: color.b * 0.25,
            a: 0.6,
        };
        frame.fill_rectangle(
            Point::new(0.0, lane_mid - 0.5),
            Size::new(w, 1.0),
            centre_color,
        );

        // Build the waveform path.
        // We read `SCOPE_HISTORY` samples out of the ring buffer starting
        // just after the current write cursor (oldest first → left of screen).
        let path = Path::new(|builder| {
            for sample_idx in 0..SCOPE_HISTORY {
                // Oldest sample is at (cursor) after one full wrap.
                let ring_pos = (vis.scope_cursor + sample_idx) % SCOPE_HISTORY;
                let level = vis.scope_history[i][ring_pos].clamp(0.0, 1.0);

                // Map level (0–1) to a y offset centred in the lane.
                // level 0   → centre line (no signal)
                // level 1   → top of amplitude range
                let y_offset = -(level * amplitude);
                let x = (sample_idx as f32 / (SCOPE_HISTORY - 1) as f32) * w;
                let y = lane_mid + y_offset;

                if sample_idx == 0 {
                    builder.move_to(Point::new(x, y));
                } else {
                    builder.line_to(Point::new(x, y));
                }
            }
        });

        frame.stroke(
            &path,
            Stroke::default()
                .with_color(Color { a: 0.90, ..color })
                .with_width(SCOPE_LINE_WIDTH),
        );

        // Tiny voice label at left edge of each lane (V1 / V2 / V3 …)
        // We skip iced's text API (requires font loading) and instead draw
        // a small coloured dot so the lane is identifiable without text.
        let dot_r = 2.5_f32;
        frame.fill_rectangle(
            Point::new(3.0, lane_mid - dot_r),
            Size::new(dot_r * 2.0, dot_r * 2.0),
            Color { a: 0.7, ..color },
        );
    }

    // Mode hint label (bottom-right, very faint)
    draw_mode_hint(frame, bounds, "BARS ▶");
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Draw a very faint "click to switch mode" indicator dot in the bottom-right
/// corner.  We use a tiny rectangle rather than text to avoid font deps.
fn draw_mode_hint(frame: &mut Frame, bounds: Rectangle, _label: &str) {
    // Three small dots — a universal "clickable" affordance hint.
    let x = bounds.width - 12.0;
    let y = bounds.height - 6.0;
    for dot in 0..3_u8 {
        frame.fill_rectangle(
            Point::new(x + dot as f32 * 4.0, y),
            Size::new(2.5, 2.5),
            Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.18,
            },
        );
    }
}

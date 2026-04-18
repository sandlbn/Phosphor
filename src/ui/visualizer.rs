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
// Single-click anywhere on the widget to toggle between Bar and Scope modes.
// Double-click to expand the visualiser to fill the whole window.

use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Size, Theme};
use std::time::Instant;

use super::sid_panel::TrackerHistory;

/// References to the tracker state passed into the visualiser when mode == Tracker.
pub struct TrackerRef<'a> {
    pub history: &'a TrackerHistory,
    pub num_sids: usize,
}

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

/// Maximum interval between two clicks that counts as a double-click (ms).
const DOUBLE_CLICK_MS: u128 = 400;

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
    /// SIDdump-style tracker view (note / waveform / ADSR per voice).
    Tracker,
    /// Fullscreen karaoke lyrics display (MUS + WDS files).
    Karaoke,
}

impl VisMode {
    /// Cycle through modes: Bars → Scope → Tracker → Karaoke → Bars…
    pub fn toggle(self) -> Self {
        match self {
            Self::Bars => Self::Scope,
            Self::Scope => Self::Tracker,
            Self::Tracker => Self::Karaoke,
            Self::Karaoke => Self::Bars,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Per-canvas interaction state
// ─────────────────────────────────────────────────────────────────────────────

/// Managed by iced between redraws; used to detect double-clicks.
#[derive(Debug, Default)]
pub struct VisState {
    last_click: Option<Instant>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Track info for the expanded overlay
// ─────────────────────────────────────────────────────────────────────────────

pub struct ExpandedInfo {
    pub name: String,
    pub author: String,
    pub released: String,
    pub sid_type: String,
    pub current_song: u16,
    pub songs: u16,
    pub is_pal: bool,
    pub is_rsid: bool,
    pub is_mus: bool,
    pub num_sids: usize,
    pub elapsed_secs: f32,
    pub duration_secs: Option<f32>,
    /// First non-empty line of STIL info for the current track/subtune (empty if none).
    pub stil_text: String,
    /// Horizontal scroll offset for the STIL demoscene ticker (advances each tick).
    pub stil_scroll_x: f32,
    /// Full karaoke lyrics text (from WDS file, newline-separated lines).
    pub karaoke_text: String,
    /// Current karaoke line index from real-time FLAG events.
    pub karaoke_line: usize,
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
    /// Separate cache for the full-screen expanded overlay.
    /// Must be distinct from `cache` because the two render at different
    /// sizes — sharing a cache between sizes causes wgpu StagingBelt panics.
    expanded_cache: Cache,
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
            expanded_cache: Cache::new(),
        }
    }

    /// Set the number of SIDs for the current tune.
    /// Call this whenever a new track starts so the layout updates.
    pub fn set_num_sids(&mut self, n: usize) {
        self.num_sids = n.clamp(1, 4);
        self.cache.clear();
        self.expanded_cache.clear();
    }

    /// Feed a new frame of voice levels from the player.
    /// `levels` is a flat slice: [SID1V1, SID1V2, SID1V3, SID2V1, …].
    /// Values are expected in the range 0.0–1.0.
    pub fn update(&mut self, levels: &[f32]) {
        let n = self.bar_count();
        for i in 0..MAX_BARS {
            let new_val = levels.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
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
            // Only record voices active in the current tune; rest stay at zero.
            self.scope_history[i][self.scope_cursor] = if i < n { new_val } else { 0.0 };
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
        self.expanded_cache.clear();
    }

    /// Invalidate the expanded-view cache (call when scroll offset changes).
    pub fn invalidate_expanded(&mut self) {
        self.expanded_cache.clear();
    }

    /// Toggle between Bar and Scope display modes.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        self.cache.clear();
        self.expanded_cache.clear();
    }

    /// Number of voices (bars / lanes) to draw for the current tune.
    fn bar_count(&self) -> usize {
        self.num_sids * 3
    }

    /// Compact 60 px strip for the track-info bar.
    /// Single click cycles Bars → Scope → Tracker; double-click expands full window.
    pub fn view<'a>(&'a self, tracker: Option<TrackerRef<'a>>) -> Element<'a, super::Message> {
        Canvas::new(VisProg {
            vis: self,
            info: None,
            expanded: false,
            tracker,
            cache: &self.cache,
        })
        .width(Length::Fill)
        .height(Length::Fixed(60.0))
        .into()
    }

    /// Full-window concert-screen overlay.
    /// Single click still cycles modes; double-click collapses back.
    pub fn view_expanded<'a>(
        &'a self,
        info: Option<&'a ExpandedInfo>,
        tracker: Option<TrackerRef<'a>>,
    ) -> Element<'a, super::Message> {
        Canvas::new(VisProg {
            vis: self,
            info,
            expanded: true,
            tracker,
            cache: &self.expanded_cache,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Canvas program
// ─────────────────────────────────────────────────────────────────────────────

struct VisProg<'v, 'i> {
    vis: &'v Visualizer,
    info: Option<&'i ExpandedInfo>,
    expanded: bool,
    tracker: Option<TrackerRef<'v>>,
    cache: &'v Cache,
}

impl<'v, 'i> canvas::Program<super::Message> for VisProg<'v, 'i> {
    type State = VisState;

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geom = self.cache.draw(renderer, bounds.size(), |frame| {
            if self.expanded {
                match self.vis.mode {
                    VisMode::Tracker => {
                        if let Some(ref tr) = self.tracker {
                            draw_tracker_expanded(tr, frame, bounds, self.info);
                        } else {
                            draw_expanded(self.vis, frame, bounds, self.info);
                        }
                    }
                    VisMode::Karaoke => {
                        draw_karaoke_expanded(frame, bounds, self.info);
                    }
                    _ => {
                        draw_expanded(self.vis, frame, bounds, self.info);
                    }
                }
            } else {
                match self.vis.mode {
                    VisMode::Bars => draw_bars(self.vis, frame, bounds),
                    VisMode::Scope => draw_scope(self.vis, frame, bounds),
                    VisMode::Karaoke => draw_bars(self.vis, frame, bounds), // compact fallback
                    VisMode::Tracker => {
                        if let Some(ref tr) = self.tracker {
                            super::sid_panel::paint_tracker_compact(
                                frame,
                                bounds,
                                &tr.history.frames,
                                tr.num_sids,
                            );
                        } else {
                            draw_bars(self.vis, frame, bounds);
                        }
                    }
                }
            }
        });
        vec![geom]
    }

    fn update(
        &self,
        state: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<super::Message>> {
        if let canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) = event {
            if cursor.is_over(bounds) {
                let now = Instant::now();
                if let Some(prev) = state.last_click {
                    if now.duration_since(prev).as_millis() <= DOUBLE_CLICK_MS {
                        state.last_click = None;
                        return Some(canvas::Action::publish(super::Message::ToggleVisFull));
                    }
                }
                state.last_click = Some(now);
                return Some(canvas::Action::publish(super::Message::ToggleVisMode));
            }
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Small bar — Bars
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

    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.08, 0.08, 0.10),
    );

    for i in 0..n {
        let x = i as f32 * (bar_w + gap);
        let level = vis.bars[i].clamp(0.0, 1.0);
        let color = voice_color(i);
        let min_h = MIN_BAR_HEIGHT * (h - 4.0);
        frame.fill_rectangle(
            Point::new(x, h - 2.0 - min_h),
            Size::new(bar_w, min_h),
            Color {
                r: color.r * 0.2,
                g: color.g * 0.2,
                b: color.b * 0.2,
                a: 0.5,
            },
        );
        let bar_h = level * (h - 4.0);
        if bar_h > min_h {
            frame.fill_rectangle(
                Point::new(x, h - 2.0 - bar_h),
                Size::new(bar_w, bar_h),
                color,
            );
        }
        let peak = vis.peaks[i].clamp(0.0, 1.0);
        if peak > 0.01 {
            frame.fill_rectangle(
                Point::new(x, h - 2.0 - peak * (h - 4.0)),
                Size::new(bar_w, 2.0),
                Color { a: 0.85, ..color },
            );
        }
    }
    draw_mode_hint(frame, bounds);
}

// ─────────────────────────────────────────────────────────────────────────────
//  Small bar — Scope
// ─────────────────────────────────────────────────────────────────────────────

/// Draw the scrolling oscilloscope, one lane per active voice.
fn draw_scope(vis: &Visualizer, frame: &mut Frame, bounds: Rectangle) {
    let n = vis.bar_count();
    if n == 0 {
        return;
    }
    let w = bounds.width;
    let h = bounds.height;
    let lane_h = h / n as f32;

    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.06, 0.07, 0.09),
    );

    for i in 0..n {
        let lane_top = i as f32 * lane_h;
        let lane_mid = lane_top + lane_h * 0.5;
        let amplitude = lane_h * (0.5 - SCOPE_LANE_PADDING);
        let color = voice_color(i);

        if i > 0 {
            frame.fill_rectangle(
                Point::new(0.0, lane_top),
                Size::new(w, 1.0),
                Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.05,
                },
            );
        }
        frame.fill_rectangle(
            Point::new(0.0, lane_mid - 0.5),
            Size::new(w, 1.0),
            Color {
                r: color.r * 0.25,
                g: color.g * 0.25,
                b: color.b * 0.25,
                a: 0.6,
            },
        );

        let path = Path::new(|b| {
            for s in 0..SCOPE_HISTORY {
                let level =
                    vis.scope_history[i][(vis.scope_cursor + s) % SCOPE_HISTORY].clamp(0.0, 1.0);
                let x = (s as f32 / (SCOPE_HISTORY - 1) as f32) * w;
                let y = lane_mid - level * amplitude;
                if s == 0 {
                    b.move_to(Point::new(x, y));
                } else {
                    b.line_to(Point::new(x, y));
                }
            }
        });
        frame.stroke(
            &path,
            Stroke::default()
                .with_color(Color { a: 0.90, ..color })
                .with_width(SCOPE_LINE_WIDTH),
        );

        let dot_r = 2.5_f32;
        frame.fill_rectangle(
            Point::new(3.0, lane_mid - dot_r),
            Size::new(dot_r * 2.0, dot_r * 2.0),
            Color { a: 0.7, ..color },
        );
    }
    draw_mode_hint(frame, bounds);
}

// ─────────────────────────────────────────────────────────────────────────────
//  Expanded full-window view
// ─────────────────────────────────────────────────────────────────────────────

fn format_time(secs: f32) -> String {
    let s = secs as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

fn draw_expanded(
    vis: &Visualizer,
    frame: &mut Frame,
    bounds: Rectangle,
    info: Option<&ExpandedInfo>,
) {
    let w = bounds.width;
    let h = bounds.height;

    // ── Background ────────────────────────────────────────────────────────────
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.04, 0.04, 0.06),
    );

    // CRT scanlines
    let mut sy = 0.0_f32;
    while sy < h {
        frame.fill_rectangle(
            Point::new(0.0, sy),
            Size::new(w, 1.0),
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.15,
            },
        );
        sy += 4.0;
    }

    // Vignette — darken edges with a few border rectangles
    for ring in 0..6_u8 {
        let t = ring as f32 / 6.0;
        let alpha = 0.20 * (1.0 - t) * (1.0 - t);
        let pad = t * (w.min(h) * 0.42);
        let v = Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: alpha,
        };
        frame.fill_rectangle(Point::new(0.0, 0.0), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, h - pad), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, pad), Size::new(pad, h - 2.0 * pad), v);
        frame.fill_rectangle(Point::new(w - pad, pad), Size::new(pad, h - 2.0 * pad), v);
    }

    // ── Corner brackets ───────────────────────────────────────────────────────
    let bl = 28.0_f32;
    let bt = 2.0_f32;
    let bp = 16.0_f32;
    let bc = Color {
        r: 0.35,
        g: 0.88,
        b: 0.58,
        a: 0.55,
    };
    // TL
    frame.fill_rectangle(Point::new(bp, bp), Size::new(bl, bt), bc);
    frame.fill_rectangle(Point::new(bp, bp), Size::new(bt, bl), bc);
    // TR
    frame.fill_rectangle(Point::new(w - bp - bl, bp), Size::new(bl, bt), bc);
    frame.fill_rectangle(Point::new(w - bp - bt, bp), Size::new(bt, bl), bc);
    // BL
    frame.fill_rectangle(Point::new(bp, h - bp - bt), Size::new(bl, bt), bc);
    frame.fill_rectangle(Point::new(bp, h - bp - bl), Size::new(bt, bl), bc);
    // BR
    frame.fill_rectangle(Point::new(w - bp - bl, h - bp - bt), Size::new(bl, bt), bc);
    frame.fill_rectangle(Point::new(w - bp - bt, h - bp - bl), Size::new(bt, bl), bc);

    // ── Layout zones ──────────────────────────────────────────────────────────
    let title_h = h * 0.22;
    let meta_h = h * 0.15;
    let vis_top = title_h;
    let vis_bot = h - meta_h;
    let vis_h = vis_bot - vis_top;
    let pad_x = 40.0_f32;
    let div = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 0.06,
    };

    frame.fill_rectangle(Point::new(0.0, vis_top - 1.0), Size::new(w, 1.0), div);
    frame.fill_rectangle(Point::new(0.0, vis_bot + 1.0), Size::new(w, 1.0), div);

    // ── Title block ───────────────────────────────────────────────────────────
    if let Some(info) = info {
        // Song name — scale 3
        let ns = 3_u32;
        let ncw = (3 * ns + ns) as f32;
        let nch = (5 * ns) as f32;
        let nmax = ((w - 80.0) / ncw).floor() as usize;
        let nch_vec: Vec<char> = info.name.chars().take(nmax).collect();
        let ntw = nch_vec.len() as f32 * ncw;
        draw_pixel_text(
            frame,
            &nch_vec,
            ((w - ntw) / 2.0).max(40.0),
            title_h * 0.15,
            ns,
            Color {
                r: 0.35,
                g: 0.90,
                b: 0.60,
                a: 0.95,
            },
        );

        // Author — scale 2
        let aus = 2_u32;
        let aucw = (3 * aus + aus) as f32;
        let aumax = ((w - 80.0) / aucw).floor() as usize;
        let au_vec: Vec<char> = info.author.chars().take(aumax).collect();
        let autw = au_vec.len() as f32 * aucw;
        draw_pixel_text(
            frame,
            &au_vec,
            ((w - autw) / 2.0).max(40.0),
            title_h * 0.15 + nch + 10.0,
            aus,
            Color {
                r: 0.55,
                g: 0.65,
                b: 0.90,
                a: 0.80,
            },
        );

        // ── Progress bar ──────────────────────────────────────────────────────
        if let Some(dur) = info.duration_secs {
            if dur > 0.0 {
                let progress = (info.elapsed_secs / dur).clamp(0.0, 1.0);
                let bar_y = title_h * 0.15 + nch + 10.0 + (5 * aus) as f32 + 14.0;
                let bar_w_full = w * 0.5;
                let bar_x = (w - bar_w_full) / 2.0;
                // Track
                frame.fill_rectangle(
                    Point::new(bar_x, bar_y),
                    Size::new(bar_w_full, 3.0),
                    Color {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.10,
                    },
                );
                // Fill
                frame.fill_rectangle(
                    Point::new(bar_x, bar_y),
                    Size::new(bar_w_full * progress, 3.0),
                    Color {
                        r: 0.35,
                        g: 0.90,
                        b: 0.60,
                        a: 0.70,
                    },
                );
                // Elapsed / duration label
                let et_str = format_time(info.elapsed_secs);
                let dt_str = format_time(dur);
                let ts = 1_u32;
                let tcw = (3 * ts + ts) as f32;
                draw_pixel_text(
                    frame,
                    &et_str.chars().collect::<Vec<_>>(),
                    bar_x,
                    bar_y + 6.0,
                    ts,
                    Color {
                        r: 0.6,
                        g: 0.7,
                        b: 0.6,
                        a: 0.55,
                    },
                );
                let dt_w = dt_str.chars().count() as f32 * tcw;
                draw_pixel_text(
                    frame,
                    &dt_str.chars().collect::<Vec<_>>(),
                    bar_x + bar_w_full - dt_w,
                    bar_y + 6.0,
                    ts,
                    Color {
                        r: 0.6,
                        g: 0.7,
                        b: 0.6,
                        a: 0.55,
                    },
                );
            }
        }

        // ── Metadata strip ────────────────────────────────────────────────────
        let song_str = if info.songs > 1 {
            format!("SONG {}/{}", info.current_song, info.songs)
        } else {
            String::from("SINGLE")
        };
        let sids_str = format!(
            "{} SID{}",
            info.num_sids,
            if info.num_sids > 1 { "S" } else { "" }
        );
        let tokens: &[&str] = &[
            &song_str,
            if info.is_pal { "PAL" } else { "NTSC" },
            if info.is_mus {
                "MUS"
            } else if info.is_rsid {
                "RSID"
            } else {
                "PSID"
            },
            info.sid_type.as_str(),
            &sids_str,
            info.released.as_str(),
        ];
        let meta_colors = [
            Color {
                r: 0.35,
                g: 0.90,
                b: 0.60,
                a: 0.70,
            },
            Color {
                r: 0.55,
                g: 0.65,
                b: 0.90,
                a: 0.60,
            },
            Color {
                r: 0.90,
                g: 0.55,
                b: 0.30,
                a: 0.60,
            },
            Color {
                r: 0.85,
                g: 0.35,
                b: 0.55,
                a: 0.60,
            },
            Color {
                r: 0.55,
                g: 0.65,
                b: 0.90,
                a: 0.60,
            },
            Color {
                r: 0.65,
                g: 0.65,
                b: 0.65,
                a: 0.50,
            },
        ];

        let ms = 2_u32;
        let mcw = (3 * ms + ms) as f32;
        let mch = (5 * ms) as f32;
        let sep_w = mcw * 3.0;
        let meta_y = vis_bot + (meta_h - mch) / 2.0;

        let total_tok_w: f32 = tokens
            .iter()
            .enumerate()
            .map(|(ti, tok)| {
                let tw = tok.chars().count() as f32 * mcw;
                tw + if ti < tokens.len() - 1 { sep_w } else { 0.0 }
            })
            .sum();

        let mut mx = ((w - total_tok_w) / 2.0).max(40.0);
        let dot_c = Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.18,
        };

        for (ti, tok) in tokens.iter().enumerate() {
            let chars: Vec<char> = tok.chars().collect();
            let tw = chars.len() as f32 * mcw;
            draw_pixel_text(frame, &chars, mx, meta_y, ms, meta_colors[ti]);
            mx += tw;
            if ti < tokens.len() - 1 {
                let dot_x = mx + sep_w / 2.0 - (ms as f32) / 2.0;
                let dot_y = meta_y + mch / 2.0 - (ms as f32) / 2.0;
                frame.fill_rectangle(
                    Point::new(dot_x, dot_y),
                    Size::new(ms as f32, ms as f32),
                    dot_c,
                );
                mx += sep_w;
            }
        }
    }

    // ── Main visualiser ───────────────────────────────────────────────────────
    let vis_bounds = Rectangle {
        x: bounds.x + pad_x,
        y: bounds.y + vis_top,
        width: w - pad_x * 2.0,
        height: vis_h,
    };
    match vis.mode {
        VisMode::Bars => draw_bars_expanded(vis, frame, vis_bounds),
        VisMode::Scope => draw_scope_expanded(vis, frame, vis_bounds),
        VisMode::Tracker | VisMode::Karaoke => {} // handled separately
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Expanded — Tracker full-screen with CRT overlay + title/progress from ExpandedInfo
// ─────────────────────────────────────────────────────────────────────────────

fn draw_tracker_expanded(
    tr: &TrackerRef<'_>,
    frame: &mut Frame,
    bounds: Rectangle,
    info: Option<&ExpandedInfo>,
) {
    let w = bounds.width;
    let h = bounds.height;

    // Dark CRT background
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.03, 0.05, 0.04),
    );

    // CRT scanlines
    let mut sy = 0.0_f32;
    while sy < h {
        frame.fill_rectangle(
            Point::new(0.0, sy),
            Size::new(w, 1.0),
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.12,
            },
        );
        sy += 4.0;
    }

    // Vignette — darken edges
    for ring in 0..5u8 {
        let t = ring as f32 / 5.0;
        let alpha = 0.18 * (1.0 - t) * (1.0 - t);
        let pad = t * w.min(h) * 0.30;
        let v = Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: alpha,
        };
        frame.fill_rectangle(Point::new(0.0, 0.0), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, h - pad), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, pad), Size::new(pad, h - 2.0 * pad), v);
        frame.fill_rectangle(Point::new(w - pad, pad), Size::new(pad, h - 2.0 * pad), v);
    }

    // ── Layout ───────────────────────────────────────────────────────────────
    // Tracker fills the top. Footer at bottom has:
    //   Left zone  (w - 120px): two scrolling rows — wave bounce + color wave
    //   Right zone (120px):     countdown, always visible, separated by glow line
    //
    // Footer height: generous so big fonts have room + wave amplitude headroom
    let s4h = 5.0 * 4.0_f32; // scale-4 char height (row 1 — bigger!)
    let s2h = 5.0 * 2.0_f32; // scale-2 char height (row 2)
    let wave_amp = 6.0_f32; // max vertical displacement for sine wave
    let row_pad = 10.0_f32;
    let footer_h = if info.is_some() {
        2.0 + row_pad + s4h + wave_amp + row_pad + s2h + row_pad
    } else {
        0.0
    };
    let cd_zone_w = 110.0_f32; // right panel width reserved for countdown

    let tracker_bounds = Rectangle {
        x: bounds.x,
        y: bounds.y,
        width: w,
        height: (h - footer_h).max(40.0),
    };

    super::sid_panel::paint_tracker_compact(frame, tracker_bounds, &tr.history.frames, tr.num_sids);

    if let Some(info) = info {
        let foot_y = h - footer_h;
        let scroller_w = w - cd_zone_w; // width available to the scroller rows

        // ── Footer background ─────────────────────────────────────────────────
        frame.fill_rectangle(
            Point::new(0.0, foot_y),
            Size::new(w, footer_h),
            Color {
                r: 0.015,
                g: 0.04,
                b: 0.02,
                a: 0.98,
            },
        );
        // Top separator — bright glow line
        frame.fill_rectangle(
            Point::new(0.0, foot_y),
            Size::new(w, 1.0),
            Color {
                r: 0.22,
                g: 0.88,
                b: 0.40,
                a: 0.70,
            },
        );
        frame.fill_rectangle(
            Point::new(0.0, foot_y + 1.0),
            Size::new(w, 1.0),
            Color {
                r: 0.10,
                g: 0.44,
                b: 0.20,
                a: 0.20,
            },
        );

        // Vertical divider before countdown zone
        frame.fill_rectangle(
            Point::new(scroller_w, foot_y + 2.0),
            Size::new(1.0, footer_h - 2.0),
            Color {
                r: 0.18,
                g: 0.60,
                b: 0.30,
                a: 0.35,
            },
        );

        // ── Countdown — right panel, big pixel font, always visible ───────────
        if let Some(dur) = info.duration_secs {
            let remaining = (dur - info.elapsed_secs).max(0.0);
            let rem_u = remaining as u32;
            let cd_str = format!("-{}:{:02}", rem_u / 60, rem_u % 60);
            let cd_chars: Vec<char> = cd_str.chars().collect();
            let cds = 3u32; // scale 3 for countdown — clearly readable
            let cdcw = (3 * cds + cds) as f32;
            let cdh = 5.0 * cds as f32;
            let cdw = cd_chars.len() as f32 * cdcw;
            // Centre in the right zone
            let cd_x = scroller_w + (cd_zone_w - cdw) * 0.5;
            let cd_y = foot_y + (footer_h - cdh) * 0.5;
            // Pulsing amber — intensity tied to countdown urgency
            let urgency = if dur > 0.0 {
                1.0 - (remaining / dur).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let cd_color = Color {
                r: 0.75 + urgency * 0.20,
                g: 0.45 - urgency * 0.20,
                b: 0.15,
                a: 0.90,
            };
            draw_pixel_text(frame, &cd_chars, cd_x, cd_y, cds, cd_color);
        } else {
            // No duration — show ??? in the right panel
            let q_chars: Vec<char> = "?:??".chars().collect();
            let qs = 3u32;
            let qcw = (3 * qs + qs) as f32;
            let qh = 5.0 * qs as f32;
            let qw = q_chars.len() as f32 * qcw;
            draw_pixel_text(
                frame,
                &q_chars,
                scroller_w + (cd_zone_w - qw) * 0.5,
                foot_y + (footer_h - qh) * 0.5,
                qs,
                Color {
                    r: 0.35,
                    g: 0.50,
                    b: 0.35,
                    a: 0.45,
                },
            );
        }

        // ── Row 1: sine-wave bouncing title+author scroll (scale 4) ──────────
        let row1_str = format!("{}   *   {}", info.name, info.author);
        let row1_chars: Vec<char> = row1_str.chars().collect();
        let r1cw = (3 * 4 + 4) as f32; // scale 4 char width
        let row1_px_w = row1_chars.len() as f32 * r1cw;
        let cycle1 = row1_px_w + scroller_w;
        let start1_x = scroller_w - (info.stil_scroll_x % cycle1);
        // Baseline y — leave room for wave_amp above and below
        let row1_base = foot_y + row_pad + wave_amp;

        // Per-character sine wave: each char has a phase offset
        let s4 = 4.0_f32;
        let r1cw_f = r1cw;
        for (ci, ch) in row1_chars.iter().enumerate() {
            let cx = start1_x + ci as f32 * r1cw_f;
            if cx + r1cw_f < 0.0 {
                continue;
            }
            if cx > scroller_w {
                break;
            }

            // Sine wave: amplitude 6px, each char 0.4 rad out of phase
            let phase = info.stil_scroll_x * 0.04 + ci as f32 * 0.4;
            let bob = phase.sin() * wave_amp;
            let char_y = row1_base + bob;

            // Color wave: hue rotates along the text
            let hue_t = ((ci as f32 * 0.15 + info.stil_scroll_x * 0.01) % 1.0).abs();
            let color = hue_to_rgb(hue_t, 0.85, 0.92);

            if let Some(rows) = glyph(*ch) {
                for (ri, row) in rows.iter().enumerate() {
                    for (pi, &on) in row.iter().enumerate() {
                        if on {
                            let px = cx + pi as f32 * s4;
                            let py = char_y + ri as f32 * s4;
                            if px >= 0.0 && px + s4 <= scroller_w {
                                frame.fill_rectangle(Point::new(px, py), Size::new(s4, s4), color);
                            }
                        }
                    }
                }
            }
        }

        // ── Row 2: STIL / info scroll (scale 2) with color wave ──────────────
        // Row 2 content: STIL text if available, else released/type/system
        let row2_content = if !info.stil_text.is_empty() {
            info.stil_text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect::<Vec<_>>()
                .join("   *   ")
        } else {
            format!(
                "{}   *   {}   *   {}",
                info.released,
                info.sid_type,
                if info.is_pal { "PAL" } else { "NTSC" }
            )
        };
        let row2_chars: Vec<char> = row2_content.chars().collect();
        let r2cw = (3 * 2 + 2) as f32;
        let row2_px_w = row2_chars.len() as f32 * r2cw;
        let cycle2 = row2_px_w + scroller_w;
        let start2_x = scroller_w - (info.stil_scroll_x * 0.65 % cycle2);
        let row2_y = foot_y + row_pad + s4h + wave_amp + row_pad;
        let s2 = 2.0_f32;

        for (ci, ch) in row2_chars.iter().enumerate() {
            let cx = start2_x + ci as f32 * r2cw;
            if cx + r2cw < 0.0 {
                continue;
            }
            if cx > scroller_w {
                break;
            }

            // Slower colour cycle
            let hue_t = ((ci as f32 * 0.08 + info.stil_scroll_x * 0.005) % 1.0).abs();
            let col = hue_to_rgb(hue_t, 0.60, 0.72);

            if let Some(rows) = glyph(*ch) {
                for (ri, row) in rows.iter().enumerate() {
                    for (pi, &on) in row.iter().enumerate() {
                        if on {
                            let px = cx + pi as f32 * s2;
                            let py = row2_y + ri as f32 * s2;
                            if px >= 0.0 && px + s2 <= scroller_w {
                                frame.fill_rectangle(Point::new(px, py), Size::new(s2, s2), col);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Expanded — Bars (bi-directional with glow)
// ─────────────────────────────────────────────────────────────────────────────

fn draw_bars_expanded(vis: &Visualizer, frame: &mut Frame, bounds: Rectangle) {
    let n = vis.bar_count();
    if n == 0 {
        return;
    }

    let w = bounds.width;
    let h = bounds.height;
    let x0 = bounds.x;
    let y0 = bounds.y;
    let mid = y0 + h / 2.0;
    let gap = 6.0_f32;
    let bar_w = ((w - gap * (n as f32 - 1.0)) / n as f32).max(8.0);

    // Centre line
    frame.fill_rectangle(
        Point::new(x0, mid - 0.5),
        Size::new(w, 1.0),
        Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.08,
        },
    );

    for i in 0..n {
        let x = x0 + i as f32 * (bar_w + gap);
        let level = vis.bars[i].clamp(0.0, 1.0);
        let color = voice_color(i);
        let slot_h = h * 0.96;

        // Faint background slot
        frame.fill_rectangle(
            Point::new(x, mid - slot_h / 2.0),
            Size::new(bar_w, slot_h),
            Color {
                r: color.r * 0.07,
                g: color.g * 0.07,
                b: color.b * 0.07,
                a: 0.6,
            },
        );

        if level > 0.005 {
            let half = level * (h / 2.0 - 4.0);

            // Glow layers (wider, more transparent)
            for g in 0..4_u8 {
                let extra = g as f32 * 5.0;
                let alpha = 0.10 - g as f32 * 0.02;
                frame.fill_rectangle(
                    Point::new(x - extra / 2.0, mid - half - extra / 2.0),
                    Size::new(bar_w + extra, half * 2.0 + extra),
                    Color {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                        a: alpha,
                    },
                );
            }

            // Solid bar bi-directional from centre
            frame.fill_rectangle(
                Point::new(x, mid - half),
                Size::new(bar_w, half * 2.0),
                color,
            );

            // Hot white core at the centre axis
            let core_h = (half * 0.12).max(2.0);
            frame.fill_rectangle(
                Point::new(x, mid - core_h / 2.0),
                Size::new(bar_w, core_h),
                Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.40,
                },
            );
        }

        // Peak hold — pair of lines above and below centre
        let peak = vis.peaks[i].clamp(0.0, 1.0);
        if peak > 0.01 {
            let ph = peak * (h / 2.0 - 4.0);
            frame.fill_rectangle(
                Point::new(x, mid - ph - 2.0),
                Size::new(bar_w, 2.0),
                Color { a: 0.75, ..color },
            );
            frame.fill_rectangle(
                Point::new(x, mid + ph),
                Size::new(bar_w, 2.0),
                Color { a: 0.75, ..color },
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Expanded — Scope (phosphor glow, 3-pass)
// ─────────────────────────────────────────────────────────────────────────────

fn draw_scope_expanded(vis: &Visualizer, frame: &mut Frame, bounds: Rectangle) {
    let n = vis.bar_count();
    if n == 0 {
        return;
    }

    let w = bounds.width;
    let h = bounds.height;
    let x0 = bounds.x;
    let y0 = bounds.y;
    let lane_h = h / n as f32;

    for i in 0..n {
        let lane_top = y0 + i as f32 * lane_h;
        let lane_mid = lane_top + lane_h * 0.5;
        let amplitude = lane_h * (0.5 - 0.06);
        let color = voice_color(i);

        // Lane tint
        frame.fill_rectangle(
            Point::new(x0, lane_top),
            Size::new(w, lane_h),
            Color {
                r: color.r * 0.04,
                g: color.g * 0.04,
                b: color.b * 0.04,
                a: 1.0,
            },
        );

        // Separator
        if i > 0 {
            frame.fill_rectangle(
                Point::new(x0, lane_top),
                Size::new(w, 1.0),
                Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.06,
                },
            );
        }

        // Centre line
        frame.fill_rectangle(
            Point::new(x0, lane_mid - 0.5),
            Size::new(w, 1.0),
            Color {
                r: color.r * 0.30,
                g: color.g * 0.30,
                b: color.b * 0.30,
                a: 0.70,
            },
        );

        // 3-pass phosphor: wide+faint → medium → sharp+bright
        for pass in 0..3_u8 {
            let (lw, alpha) = match pass {
                0 => (7.0_f32, 0.05_f32),
                1 => (2.5, 0.22),
                _ => (1.2, 0.92),
            };
            let path = Path::new(|b| {
                for s in 0..SCOPE_HISTORY {
                    let level = vis.scope_history[i][(vis.scope_cursor + s) % SCOPE_HISTORY]
                        .clamp(0.0, 1.0);
                    let x = x0 + (s as f32 / (SCOPE_HISTORY - 1) as f32) * w;
                    let y = lane_mid - level * amplitude;
                    if s == 0 {
                        b.move_to(Point::new(x, y));
                    } else {
                        b.line_to(Point::new(x, y));
                    }
                }
            });
            frame.stroke(
                &path,
                Stroke::default()
                    .with_color(Color {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                        a: alpha,
                    })
                    .with_width(lw),
            );
        }

        // Voice dot
        let dot = 3.5_f32;
        frame.fill_rectangle(
            Point::new(x0 + 6.0, lane_mid - dot),
            Size::new(dot * 2.0, dot * 2.0),
            Color { a: 0.60, ..color },
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Shared small-bar helper
// ─────────────────────────────────────────────────────────────────────────────

/// Draw a very faint "click to switch mode" indicator dot in the bottom-right
/// corner.  We use a tiny rectangle rather than text to avoid font deps.
fn draw_mode_hint(frame: &mut Frame, bounds: Rectangle) {
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

// ─────────────────────────────────────────────────────────────────────────────
//  Pixel font renderer
// ─────────────────────────────────────────────────────────────────────────────

/// Convert a hue [0..1] with given saturation and value to an RGB Color.
/// Classic HSV→RGB used for the colour-wave effect on the scroller text.
pub(crate) fn hue_to_rgb(h: f32, s: f32, v: f32) -> Color {
    let h6 = h * 6.0;
    let i = h6.floor() as u32;
    let f = h6 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match i % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    Color { r, g, b, a: 1.0 }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Karaoke expanded view
// ─────────────────────────────────────────────────────────────────────────────

fn draw_karaoke_expanded(frame: &mut Frame, bounds: Rectangle, info: Option<&ExpandedInfo>) {
    let w = bounds.width;
    let h = bounds.height;

    // ── Background (CRT) ─────────────────────────────────────────────────────
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.03, 0.03, 0.05),
    );
    let mut sy = 0.0_f32;
    while sy < h {
        frame.fill_rectangle(
            Point::new(0.0, sy),
            Size::new(w, 1.0),
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.12,
            },
        );
        sy += 4.0;
    }
    // Vignette
    for ring in 0..6_u8 {
        let t = ring as f32 / 6.0;
        let alpha = 0.20 * (1.0 - t) * (1.0 - t);
        let pad = t * (w.min(h) * 0.42);
        let v = Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: alpha,
        };
        frame.fill_rectangle(Point::new(0.0, 0.0), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, h - pad), Size::new(w, pad), v);
        frame.fill_rectangle(Point::new(0.0, pad), Size::new(pad, h - 2.0 * pad), v);
        frame.fill_rectangle(Point::new(w - pad, pad), Size::new(pad, h - 2.0 * pad), v);
    }

    let info = match info {
        Some(i) => i,
        None => return,
    };

    // ── Title (top) ──────────────────────────────────────────────────────────
    let title_y = 30.0_f32;
    let ns = 3_u32;
    let ncw = (3 * ns + ns) as f32;
    let nch = (5 * ns) as f32;
    let nmax = ((w - 80.0) / ncw).floor() as usize;
    let nch_vec: Vec<char> = info.name.chars().take(nmax).collect();
    let ntw = nch_vec.len() as f32 * ncw;
    draw_pixel_text(
        frame,
        &nch_vec,
        ((w - ntw) / 2.0).max(40.0),
        title_y,
        ns,
        Color {
            r: 0.35,
            g: 0.90,
            b: 0.60,
            a: 0.95,
        },
    );

    // Author
    let aus = 2_u32;
    let aucw = (3 * aus + aus) as f32;
    let aumax = ((w - 80.0) / aucw).floor() as usize;
    let au_vec: Vec<char> = info.author.chars().take(aumax).collect();
    let autw = au_vec.len() as f32 * aucw;
    draw_pixel_text(
        frame,
        &au_vec,
        ((w - autw) / 2.0).max(40.0),
        title_y + nch + 10.0,
        aus,
        Color {
            r: 0.55,
            g: 0.65,
            b: 0.90,
            a: 0.80,
        },
    );

    // ── Lyrics area ──────────────────────────────────────────────────────────
    let lyrics_top = title_y + nch + 10.0 + (5 * aus) as f32 + 30.0;
    let lyrics_bot = h - 50.0;
    let lyrics_h = lyrics_bot - lyrics_top;
    if lyrics_h < 40.0 {
        return;
    }

    // Divider lines
    let div = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 0.06,
    };
    frame.fill_rectangle(Point::new(0.0, lyrics_top - 2.0), Size::new(w, 1.0), div);
    frame.fill_rectangle(Point::new(0.0, lyrics_bot + 2.0), Size::new(w, 1.0), div);

    let lines: Vec<&str> = info.karaoke_text.lines().collect();
    if lines.is_empty() {
        return;
    }
    let total = lines.len();

    // Real-time karaoke sync: 1 FLAG = 1 WDS line.
    // Songs with fewer FLAGs than WDS lines only show partial lyrics.
    let current_idx = info.karaoke_line.min(total.saturating_sub(1));

    // Rendering params
    let current_scale = 3_u32;
    let other_scale = 2_u32;
    let current_ch = (5 * current_scale) as f32;
    let other_ch = (5 * other_scale) as f32;
    let line_spacing = 14.0_f32;
    let line_step = current_ch + line_spacing;

    // Teleprompter-style scrolling:
    // All lines are laid out top-to-bottom from lyrics_top.
    // We scroll the viewport only when the current line would fall
    // below the center of the lyrics area — so lyrics always START
    // at the top and only begin scrolling once enough lines have passed.
    let center_threshold = lyrics_h * 0.45;
    let current_line_top_y = current_idx as f32 * line_step;
    let scroll_offset = (current_line_top_y - center_threshold).max(0.0);

    // Render all lines that fall within the visible lyrics area.
    for idx in 0..total {
        let line = lines[idx].trim();
        if line.is_empty() {
            continue;
        }

        let delta = idx as i32 - current_idx as i32;
        let in_active = delta.abs() <= 1;
        let scale = if in_active { current_scale } else { other_scale };
        let ch = if in_active { current_ch } else { other_ch };
        let cw = (3 * scale + scale) as f32;

        // Y position: top-aligned with scroll offset
        let base_y = lyrics_top + idx as f32 * line_step - scroll_offset;

        // Clip to lyrics area
        if base_y + ch < lyrics_top || base_y > lyrics_bot {
            continue;
        }

        // Color and alpha based on distance from current
        let dist = delta.unsigned_abs() as f32;
        let color = if delta == 0 {
            // Current line — bright green
            Color {
                r: 0.40,
                g: 0.95,
                b: 0.70,
                a: 1.0,
            }
        } else if in_active {
            // Adjacent lines — slightly dimmed green
            Color {
                r: 0.35,
                g: 0.80,
                b: 0.60,
                a: 0.75,
            }
        } else if delta < 0 {
            // Past lines — dimmed gray, stay readable
            let a = (0.60 - (dist - 1.0) * 0.03).max(0.30);
            Color {
                r: 0.50,
                g: 0.55,
                b: 0.65,
                a,
            }
        } else {
            // Future lines — subtle blue, stay readable
            let a = (0.55 - (dist - 1.0) * 0.03).max(0.25);
            Color {
                r: 0.35,
                g: 0.55,
                b: 0.80,
                a,
            }
        };

        let max_chars = ((w - 80.0) / cw).floor() as usize;
        let chars: Vec<char> = line.chars().take(max_chars).collect();
        let tw = chars.len() as f32 * cw;
        draw_pixel_text(
            frame,
            &chars,
            ((w - tw) / 2.0).max(40.0),
            base_y,
            scale,
            color,
        );
    }

    // ── Progress bar (bottom) ────────────────────────────────────────────────
    if let Some(dur) = info.duration_secs {
        if dur > 0.0 {
            let bar_y = lyrics_bot + 16.0;
            let bar_w = w * 0.6;
            let bar_x = (w - bar_w) / 2.0;
            // Track
            frame.fill_rectangle(
                Point::new(bar_x, bar_y),
                Size::new(bar_w, 3.0),
                Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.10,
                },
            );
            // Fill
            frame.fill_rectangle(
                Point::new(bar_x, bar_y),
                Size::new(bar_w * (info.elapsed_secs / dur).clamp(0.0, 1.0), 3.0),
                Color {
                    r: 0.35,
                    g: 0.90,
                    b: 0.60,
                    a: 0.70,
                },
            );
            // Time labels
            let ts = 1_u32;
            let tcw = (3 * ts + ts) as f32;
            let elapsed_str = format_time(info.elapsed_secs);
            let dur_str = format_time(dur);
            let e_chars: Vec<char> = elapsed_str.chars().collect();
            let d_chars: Vec<char> = dur_str.chars().collect();
            draw_pixel_text(
                frame,
                &e_chars,
                bar_x - e_chars.len() as f32 * tcw - 8.0,
                bar_y - 1.0,
                ts,
                Color {
                    r: 0.6,
                    g: 0.6,
                    b: 0.7,
                    a: 0.7,
                },
            );
            draw_pixel_text(
                frame,
                &d_chars,
                bar_x + bar_w + 8.0,
                bar_y - 1.0,
                ts,
                Color {
                    r: 0.6,
                    g: 0.6,
                    b: 0.7,
                    a: 0.7,
                },
            );
        }
    }
}

pub(crate) fn draw_pixel_text(
    frame: &mut Frame,
    chars: &[char],
    x: f32,
    y: f32,
    scale: u32,
    color: Color,
) {
    let s = scale as f32;
    let char_w = 3.0 * s + s;
    for (ci, ch) in chars.iter().enumerate() {
        if let Some(rows) = glyph(*ch) {
            let cx = x + ci as f32 * char_w;
            for (ri, row) in rows.iter().enumerate() {
                for (pi, &on) in row.iter().enumerate() {
                    if on {
                        frame.fill_rectangle(
                            Point::new(cx + pi as f32 * s, y + ri as f32 * s),
                            Size::new(s, s),
                            color,
                        );
                    }
                }
            }
        }
    }
}

pub(crate) fn glyph(c: char) -> Option<[[bool; 3]; 5]> {
    const fn b(v: u8) -> [bool; 3] {
        [v & 4 != 0, v & 2 != 0, v & 1 != 0]
    }
    macro_rules! g {
        ($a:expr,$b:expr,$c:expr,$d:expr,$e:expr) => {
            Some([b($a), b($b), b($c), b($d), b($e)])
        };
    }
    match c.to_ascii_uppercase() {
        'A' => g!(0b010, 0b101, 0b111, 0b101, 0b101),
        'B' => g!(0b110, 0b101, 0b110, 0b101, 0b110),
        'C' => g!(0b011, 0b100, 0b100, 0b100, 0b011),
        'D' => g!(0b110, 0b101, 0b101, 0b101, 0b110),
        'E' => g!(0b111, 0b100, 0b110, 0b100, 0b111),
        'F' => g!(0b111, 0b100, 0b110, 0b100, 0b100),
        'G' => g!(0b011, 0b100, 0b101, 0b101, 0b011),
        'H' => g!(0b101, 0b101, 0b111, 0b101, 0b101),
        'I' => g!(0b111, 0b010, 0b010, 0b010, 0b111),
        'J' => g!(0b001, 0b001, 0b001, 0b101, 0b010),
        'K' => g!(0b101, 0b110, 0b100, 0b110, 0b101),
        'L' => g!(0b100, 0b100, 0b100, 0b100, 0b111),
        'M' => g!(0b101, 0b111, 0b101, 0b101, 0b101),
        'N' => g!(0b101, 0b111, 0b111, 0b111, 0b101),
        'O' => g!(0b010, 0b101, 0b101, 0b101, 0b010),
        'P' => g!(0b110, 0b101, 0b110, 0b100, 0b100),
        'Q' => g!(0b010, 0b101, 0b101, 0b111, 0b011),
        'R' => g!(0b110, 0b101, 0b110, 0b110, 0b101),
        'S' => g!(0b011, 0b100, 0b010, 0b001, 0b110),
        'T' => g!(0b111, 0b010, 0b010, 0b010, 0b010),
        'U' => g!(0b101, 0b101, 0b101, 0b101, 0b010),
        'V' => g!(0b101, 0b101, 0b101, 0b010, 0b010),
        'W' => g!(0b101, 0b101, 0b101, 0b111, 0b101),
        'X' => g!(0b101, 0b101, 0b010, 0b101, 0b101),
        'Y' => g!(0b101, 0b101, 0b010, 0b010, 0b010),
        'Z' => g!(0b111, 0b001, 0b010, 0b100, 0b111),
        '0' => g!(0b010, 0b101, 0b101, 0b101, 0b010),
        '1' => g!(0b010, 0b110, 0b010, 0b010, 0b111),
        '2' => g!(0b110, 0b001, 0b010, 0b100, 0b111),
        '3' => g!(0b110, 0b001, 0b010, 0b001, 0b110),
        '4' => g!(0b101, 0b101, 0b111, 0b001, 0b001),
        '5' => g!(0b111, 0b100, 0b110, 0b001, 0b110),
        '6' => g!(0b010, 0b100, 0b110, 0b101, 0b010),
        '7' => g!(0b111, 0b001, 0b010, 0b010, 0b010),
        '8' => g!(0b010, 0b101, 0b010, 0b101, 0b010),
        '9' => g!(0b010, 0b101, 0b011, 0b001, 0b010),
        ' ' => g!(0b000, 0b000, 0b000, 0b000, 0b000),
        '-' => g!(0b000, 0b000, 0b111, 0b000, 0b000),
        '_' => g!(0b000, 0b000, 0b000, 0b000, 0b111),
        '.' => g!(0b000, 0b000, 0b000, 0b000, 0b010),
        ',' => g!(0b000, 0b000, 0b000, 0b010, 0b100),
        ':' => g!(0b000, 0b010, 0b000, 0b010, 0b000),
        '/' => g!(0b001, 0b001, 0b010, 0b100, 0b100),
        '\'' | '`' => g!(0b010, 0b100, 0b000, 0b000, 0b000),
        '!' => g!(0b010, 0b010, 0b010, 0b000, 0b010),
        '?' => g!(0b010, 0b101, 0b010, 0b000, 0b010),
        '(' => g!(0b001, 0b010, 0b010, 0b010, 0b001),
        ')' => g!(0b100, 0b010, 0b010, 0b010, 0b100),
        _ => None,
    }
}

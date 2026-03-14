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
    pub num_sids: usize,
    pub elapsed_secs: f32,
    pub duration_secs: Option<f32>,
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

    /// Compact 60 px strip for the track-info bar.
    /// Single click toggles Bar / Scope mode; double-click expands to full window.
    pub fn view(&self) -> Element<'_, super::Message> {
        Canvas::new(VisProg {
            vis: self,
            info: None,
            expanded: false,
        })
        .width(Length::Fill)
        .height(Length::Fixed(60.0))
        .into()
    }

    /// Full-window concert-screen overlay.
    /// Single click still toggles Bar / Scope; double-click collapses back.
    pub fn view_expanded<'a>(
        &'a self,
        info: Option<&'a ExpandedInfo>,
    ) -> Element<'a, super::Message> {
        Canvas::new(VisProg {
            vis: self,
            info,
            expanded: true,
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
        let geom = self.vis.cache.draw(renderer, bounds.size(), |frame| {
            if self.expanded {
                draw_expanded(self.vis, frame, bounds, self.info);
            } else {
                match self.vis.mode {
                    VisMode::Bars => draw_bars(self.vis, frame, bounds),
                    VisMode::Scope => draw_scope(self.vis, frame, bounds),
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
            if info.is_rsid { "RSID" } else { "PSID" },
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

fn draw_pixel_text(frame: &mut Frame, chars: &[char], x: f32, y: f32, scale: u32, color: Color) {
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

fn glyph(c: char) -> Option<[[bool; 3]; 5]> {
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

// sid_panel.rs — SID register info panel + SIDdump-style tracker view.
//
// Layout (top → bottom):
//   ┌──────────────────────────────────────────────────────────┐
//   │  TRACKER VIEW  (Canvas, scrolling frame history)         │
//   │  One column per voice × num_sids.  Current frame is the  │
//   │  highlighted playhead row at ¼ from top; past rows scroll │
//   │  upward as the tune plays.                                │
//   ├──────────────────────────────────────────────────────────┤
//   │  SID REGISTER DETAIL  (read-only text grid)              │
//   │  Per-voice ADSR, waveform, note, gate — one block per SID │
//   └──────────────────────────────────────────────────────────┘
//
// TrackerHistory is a pure ring buffer of sid_regs snapshots.
// Call `TrackerHistory::push` every player tick, then pass the
// reference into `sid_panel`.  The Canvas draws directly from
// the ring — zero extra allocations per frame.
//
// Phosphor colour palette (matching the visualiser accent):
//   SID 1 – phosphor green   #4DFF99 / #1A7A44
//   SID 2 – amber            #FFAA33 / #7A5000
//   SID 3 – cyan             #33DDFF / #0A6677
//   SID 4 – magenta          #FF44BB / #7A1155

use std::collections::VecDeque;

use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry, Text};
use iced::widget::{column, container, row, rule, text, Column, Space};
use iced::{
    mouse, Alignment, Color, Element, Font, Length, Padding, Point, Rectangle, Size, Theme,
};

use super::Message;

// ─────────────────────────────────────────────────────────────────────────────
//  SID constants
// ─────────────────────────────────────────────────────────────────────────────

const SID_STRIDE: usize = 0x20;
const PAL_CLOCK: f64 = 985_248.0;
const NTSC_CLOCK: f64 = 1_022_727.0;

/// How many past frames the tracker keeps in its ring buffer.
/// 512 ≈ 10 seconds at PAL 50 Hz.
pub const TRACKER_HISTORY: usize = 512;

/// Row height in logical pixels for the tracker canvas.
const ROW_H: f32 = 13.0;

/// Fraction of the tracker canvas height where the playhead sits
/// (0.0 = very top, 1.0 = bottom).  0.25 keeps most rows as history.

/// Width of the row-number gutter on the left.
const GUTTER_W: f32 = 38.0;

/// Logical pixels allocated to each voice column.
const COL_W: f32 = 108.0;

// ─────────────────────────────────────────────────────────────────────────────
//  Phosphor colour palette
// ─────────────────────────────────────────────────────────────────────────────

/// Bright phosphor colour for each SID chip (gate-on / active rows).
const SID_BRIGHT: [Color; 4] = [
    Color {
        r: 0.30,
        g: 1.00,
        b: 0.60,
        a: 1.0,
    }, // SID1 – phosphor green
    Color {
        r: 1.00,
        g: 0.67,
        b: 0.20,
        a: 1.0,
    }, // SID2 – amber
    Color {
        r: 0.20,
        g: 0.87,
        b: 1.00,
        a: 1.0,
    }, // SID3 – cyan
    Color {
        r: 1.00,
        g: 0.27,
        b: 0.73,
        a: 1.0,
    }, // SID4 – magenta
];

/// Dim colour for inactive / gate-off cells (same hue, very dark).
const SID_DIM: [Color; 4] = [
    Color {
        r: 0.10,
        g: 0.30,
        b: 0.18,
        a: 1.0,
    },
    Color {
        r: 0.30,
        g: 0.20,
        b: 0.06,
        a: 1.0,
    },
    Color {
        r: 0.06,
        g: 0.27,
        b: 0.32,
        a: 1.0,
    },
    Color {
        r: 0.30,
        g: 0.08,
        b: 0.22,
        a: 1.0,
    },
];

/// Very faint background tint per SID chip column band.
const SID_TINT: [Color; 4] = [
    Color {
        r: 0.03,
        g: 0.08,
        b: 0.05,
        a: 1.0,
    },
    Color {
        r: 0.08,
        g: 0.06,
        b: 0.02,
        a: 1.0,
    },
    Color {
        r: 0.02,
        g: 0.07,
        b: 0.10,
        a: 1.0,
    },
    Color {
        r: 0.08,
        g: 0.02,
        b: 0.06,
        a: 1.0,
    },
];

/// Background colour of the playhead highlight band.
const PLAYHEAD_BG: Color = Color {
    r: 0.10,
    g: 0.20,
    b: 0.13,
    a: 1.0,
};

/// Row-number gutter text (dim green).
const GUTTER_DIM: Color = Color {
    r: 0.22,
    g: 0.35,
    b: 0.26,
    a: 1.0,
};

/// Accent colours for the register-detail section (same as the visualiser).
const SID_ACCENT: [Color; 4] = [
    Color {
        r: 0.30,
        g: 0.85,
        b: 0.55,
        a: 1.0,
    },
    Color {
        r: 0.40,
        g: 0.60,
        b: 0.95,
        a: 1.0,
    },
    Color {
        r: 0.90,
        g: 0.55,
        b: 0.30,
        a: 1.0,
    },
    Color {
        r: 0.85,
        g: 0.35,
        b: 0.55,
        a: 1.0,
    },
];

fn dim_color(c: Color, f: f32) -> Color {
    Color {
        r: c.r * f,
        g: c.g * f,
        b: c.b * f,
        a: c.a,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Register decoding helpers
// ─────────────────────────────────────────────────────────────────────────────

fn freq_to_hz(lo: u8, hi: u8, is_pal: bool) -> f64 {
    let word = ((hi as u32) << 8) | lo as u32;
    let clock = if is_pal { PAL_CLOCK } else { NTSC_CLOCK };
    word as f64 * clock / 16_777_216.0
}

/// Convert Hz → "C-4" style note name.  Uses a thread-local cache so we
/// never allocate in the hot paint path.
fn hz_to_note(hz: f64) -> &'static str {
    if hz < 16.0 {
        return "---";
    }
    let midi = (12.0 * (hz / 440.0).log2() + 69.0).round() as i32;
    let names = [
        "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
    ];
    let name = names[midi.rem_euclid(12) as usize];
    let oct = (midi / 12) - 1;
    intern_str(format!("{}{}", name, oct))
}

fn waveform_label(ctrl: u8) -> &'static str {
    match (ctrl >> 4) & 0x0F {
        0b0001 => "TRI",
        0b0010 => "SAW",
        0b0100 => "PUL",
        0b1000 => "NOI",
        0b0011 => "T+S",
        0b0101 => "T+P",
        0b0110 => "S+P",
        0b1001 => "N+T",
        0b1010 => "N+S",
        0b1100 => "N+P",
        0b0000 => "---",
        _ => "MLT",
    }
}

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
//  String interning — avoids alloc in the paint closure
// ─────────────────────────────────────────────────────────────────────────────

/// Intern `s` into a thread-local pool and return a `'static` reference.
/// The pool is bounded by the number of unique strings ever produced
/// (a few hundred note/waveform combos at most).
fn intern_str(s: String) -> &'static str {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static POOL: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    POOL.with(|p| {
        let mut m = p.borrow_mut();
        if let Some(&cached) = m.get(&s) {
            return cached;
        }
        let leaked: &'static str = Box::leak(s.clone().into_boxed_str());
        m.insert(s, leaked);
        leaked
    })
}

// ─────────────────────────────────────────────────────────────────────────────
//  TrackerVoice / TrackerFrame
// ─────────────────────────────────────────────────────────────────────────────

/// Decoded voice state for one SID voice in one frame.
#[derive(Clone)]
pub struct TrackerVoice {
    pub note: &'static str, // "C-4", "A#3", "---"
    pub wave: &'static str, // "SAW", "TRI", "---" …
    pub gate: bool,
    pub vol: u8, // sustain nibble 0-15
}

impl Default for TrackerVoice {
    fn default() -> Self {
        Self {
            note: "---",
            wave: "---",
            gate: false,
            vol: 0,
        }
    }
}

/// One complete frame snapshot: 12 voices (4 SIDs × 3 voices) + frame index.
#[derive(Clone)]
pub struct TrackerFrame {
    /// `voices[sid * 3 + voice_index]`
    pub voices: [TrackerVoice; 12],
    pub frame_idx: u64,
}

impl Default for TrackerFrame {
    fn default() -> Self {
        Self {
            voices: std::array::from_fn(|_| TrackerVoice::default()),
            frame_idx: 0,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  TrackerHistory — public ring buffer
// ─────────────────────────────────────────────────────────────────────────────

/// Rolling ring buffer of decoded tracker frames.
///
/// Embed this in your application state.  Call `push` once per player
/// tick with the raw `sid_regs` shadow, then pass a reference to
/// `sid_panel` and `TrackerView::view`.
pub struct TrackerHistory {
    pub frames: VecDeque<TrackerFrame>,
    pub frame_idx: u64,
}

impl TrackerHistory {
    pub fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(TRACKER_HISTORY),
            frame_idx: 0,
        }
    }

    /// Decode `sid_regs` and push a new frame into the ring.
    ///
    /// Call this every player tick *before* rebuilding the UI.
    pub fn push(&mut self, sid_regs: &[u8], num_sids: usize, is_pal: bool) {
        let n = num_sids.clamp(1, 4);
        let mut frame = TrackerFrame {
            voices: std::array::from_fn(|_| TrackerVoice::default()),
            frame_idx: self.frame_idx,
        };

        for sid in 0..n {
            let base = sid * SID_STRIDE;
            for voice in 0..3 {
                let vo = base + voice * 7;
                let safe = |i: usize| sid_regs.get(i).copied().unwrap_or(0);
                let freq_lo = safe(vo);
                let freq_hi = safe(vo + 1);
                let ctrl = safe(vo + 4);
                let sr = safe(vo + 6);
                let gate = ctrl & 0x01 != 0;
                let hz = freq_to_hz(freq_lo, freq_hi, is_pal);
                let sustain = (sr >> 4) as u8;

                frame.voices[sid * 3 + voice] = TrackerVoice {
                    note: if gate { hz_to_note(hz) } else { "---" },
                    wave: if gate { waveform_label(ctrl) } else { "---" },
                    gate,
                    vol: sustain,
                };
            }
        }

        if self.frames.len() >= TRACKER_HISTORY {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
        self.frame_idx += 1;
    }

    /// Clear all history (call on Stop or new tune).
    pub fn reset(&mut self) {
        self.frames.clear();
        self.frame_idx = 0;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  TrackerView — owns the iced Canvas Cache
// ─────────────────────────────────────────────────────────────────────────────

/// Stateful wrapper that owns the canvas `Cache`.
///
/// Embed one of these in your application state next to `TrackerHistory`.
///
/// ```ignore
/// // In your app struct:
/// pub tracker_history: TrackerHistory,
/// pub tracker_view:    TrackerView,
///
/// // Every player tick (e.g. in your Tick handler):
/// self.tracker_history.push(&status.sid_regs, num_sids, is_pal);
/// self.tracker_view.invalidate();
/// ```
pub struct TrackerView {
    cache: Cache,
}

impl TrackerView {
    pub fn new() -> Self {
        Self {
            cache: Cache::new(),
        }
    }

    /// Invalidate the canvas cache so iced redraws on the next frame.
    /// Call once per player tick.
    pub fn invalidate(&mut self) {
        self.cache.clear();
    }

    /// Reset cache (call when a new tune starts).
    pub fn reset(&mut self) {
        self.cache.clear();
    }

    /// Build the iced `Element` for embedding in your layout.
    pub fn view<'a>(
        &'a self,
        history: &'a TrackerHistory,
        num_sids: usize,
        _height: f32,
    ) -> Element<'a, Message> {
        Canvas::new(TrackerCanvas {
            history,
            num_sids,
            cache: &self.cache,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  TrackerCanvas — the iced canvas::Program impl
// ─────────────────────────────────────────────────────────────────────────────

struct TrackerCanvas<'a> {
    history: &'a TrackerHistory,
    num_sids: usize,
    cache: &'a Cache,
}

impl<'a> canvas::Program<Message> for TrackerCanvas<'a> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let n = self.num_sids.clamp(1, 4);
        let geom = self.cache.draw(renderer, bounds.size(), |frame| {
            paint_tracker(frame, bounds, &self.history.frames, n);
        });
        vec![geom]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  paint_tracker — main Canvas paint routine
// ─────────────────────────────────────────────────────────────────────────────

fn paint_tracker(
    frame: &mut Frame,
    bounds: Rectangle,
    history: &VecDeque<TrackerFrame>,
    num_sids: usize,
) {
    let w = bounds.width;
    let h = bounds.height.max(180.0);
    let num_voices = num_sids * 3;

    // ── Background ────────────────────────────────────────────────────────────
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, h),
        Color::from_rgb(0.03, 0.05, 0.04),
    );

    // ── Column width — always fill full available width ──────────────────────
    // Divide evenly across all voices; no upper cap so 1-SID gets wide
    // columns just like a real tracker, and 4-SID shrinks to fit.
    let available_w = w - GUTTER_W;
    let col_w = (available_w / num_voices as f32).max(40.0);

    // ── Per-SID column background tints ──────────────────────────────────────
    for sid in 0..num_sids {
        let tint = SID_TINT[sid];
        let x = GUTTER_W + (sid * 3) as f32 * col_w;
        frame.fill_rectangle(Point::new(x, 0.0), Size::new(col_w * 3.0, h), tint);
    }

    // ── Gutter background ─────────────────────────────────────────────────────
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(GUTTER_W, h),
        Color::from_rgb(0.025, 0.04, 0.03),
    );

    // ── SID-boundary separators (brighter) ────────────────────────────────────
    for sid in 0..=num_sids {
        let x = GUTTER_W + (sid * 3) as f32 * col_w;
        let c = Color {
            r: 0.18,
            g: 0.30,
            b: 0.20,
            a: 1.0,
        };
        frame.fill_rectangle(Point::new(x, 0.0), Size::new(1.0, h), c);
    }
    // Voice separators within each SID (faint)
    for sid in 0..num_sids {
        for vv in 1..3 {
            let x = GUTTER_W + (sid * 3 + vv) as f32 * col_w;
            frame.fill_rectangle(
                Point::new(x, 0.0),
                Size::new(1.0, h),
                Color {
                    r: 0.08,
                    g: 0.12,
                    b: 0.09,
                    a: 1.0,
                },
            );
        }
    }
    // Right edge of gutter
    frame.fill_rectangle(
        Point::new(GUTTER_W - 1.0, 0.0),
        Size::new(1.0, h),
        Color {
            r: 0.18,
            g: 0.30,
            b: 0.20,
            a: 1.0,
        },
    );

    // ── Header row ────────────────────────────────────────────────────────────
    let hdr_h = ROW_H + 4.0;
    frame.fill_rectangle(
        Point::ORIGIN,
        Size::new(w, hdr_h),
        Color::from_rgb(0.04, 0.07, 0.05),
    );
    frame.fill_rectangle(
        Point::new(0.0, hdr_h - 1.0),
        Size::new(w, 1.0),
        Color {
            r: 0.15,
            g: 0.28,
            b: 0.18,
            a: 1.0,
        },
    );

    // Gutter label
    px_label(frame, "ROW", GUTTER_W / 2.0, hdr_h / 2.0, GUTTER_DIM, false);

    // Voice / SID headers
    for sid in 0..num_sids {
        let bright = SID_BRIGHT[sid];
        for voice in 0..3 {
            let vi = sid * 3 + voice;
            let cx = GUTTER_W + vi as f32 * col_w + col_w * 0.5;
            let lbl = intern_str(format!("S{} V{}", sid + 1, voice + 1));
            px_label(frame, lbl, cx, hdr_h / 2.0, bright, true);
        }
    }

    // ── Row area ──────────────────────────────────────────────────────────────
    // Layout: playhead row is at the TOP (row 0), history rows go downward.
    // This matches real tracker behaviour — newest event at top, older rows
    // scroll down as time advances, filling the whole panel.
    let data_top = hdr_h;
    let data_h = h - data_top - 14.0; // reserve 14 px for footer
    let visible_rows = (data_h / ROW_H).ceil() as usize + 1;

    // Playhead sits one row from the top of the data area.
    let playhead_cy = data_top + ROW_H * 0.5;

    // Playhead highlight band
    frame.fill_rectangle(
        Point::new(0.0, playhead_cy - ROW_H * 0.5),
        Size::new(w, ROW_H),
        PLAYHEAD_BG,
    );
    // Bright left accent stripe on playhead
    frame.fill_rectangle(
        Point::new(0.0, playhead_cy - ROW_H * 0.5),
        Size::new(2.5, ROW_H),
        SID_BRIGHT[0],
    );

    let n_hist = history.len();

    for row_offset in 0..visible_rows {
        // row_offset 0 = newest frame (playhead, top row).
        // row_offset 1, 2, ... = older frames going downward.
        let cy = playhead_cy + row_offset as f32 * ROW_H;
        let row_top = cy - ROW_H * 0.5;

        if row_top > h {
            continue;
        }

        // Beat marker — faint horizontal tick in gutter every 4 rows.
        if row_offset % 4 == 0 && row_offset > 0 {
            frame.fill_rectangle(
                Point::new(1.0, row_top),
                Size::new(GUTTER_W - 2.0, 1.0),
                Color {
                    r: 0.14,
                    g: 0.22,
                    b: 0.16,
                    a: 1.0,
                },
            );
        }

        // Row number in gutter — counts down from current frame.
        if n_hist > 0 {
            let abs_idx = n_hist.saturating_sub(1 + row_offset);
            let row_label = intern_str(format!("{:04X}", abs_idx));
            let gc = if row_offset == 0 {
                SID_BRIGHT[0]
            } else {
                GUTTER_DIM
            };
            px_label(frame, row_label, GUTTER_W / 2.0, cy, gc, row_offset == 0);
        }

        // Fade alpha: newest row is full brightness, older rows fade as they
        // move further down toward the bottom of the panel.
        let alpha = if row_offset == 0 {
            1.0_f32
        } else {
            let t = (row_offset as f32 / (visible_rows as f32 * 0.85)).min(1.0);
            let fade = (1.0 - t * t).max(0.0);
            (fade * 0.80 + 0.12).min(1.0)
        };

        if row_offset >= n_hist {
            // No data yet for this slot — show faint dots.
            for vi in 0..num_voices {
                let sid = vi / 3;
                let cx = GUTTER_W + vi as f32 * col_w + col_w * 0.5;
                let dc = Color {
                    a: alpha * 0.35,
                    ..SID_DIM[sid]
                };
                px_label(frame, "·  ·", cx, cy, dc, false);
            }
            continue;
        }

        // Newest frame = last in deque (index n_hist-1), offset 0.
        // Older frames are further back: n_hist-1-row_offset.
        let hist_idx = n_hist - 1 - row_offset;
        let tf = &history[hist_idx];

        for vi in 0..num_voices {
            let sid = vi / 3;
            let voice = &tf.voices[vi];
            let col_x = GUTTER_W + vi as f32 * col_w;

            if voice.gate {
                let bright = SID_BRIGHT[sid];
                let bc = Color { a: alpha, ..bright };

                // Note in the left 45 % of the column.
                let note_cx = col_x + col_w * 0.30;
                // Waveform in the right 55 %.
                let wave_cx = col_x + col_w * 0.72;

                px_label(frame, voice.note, note_cx, cy, bc, row_offset == 0);
                let wc = Color {
                    a: alpha * 0.72,
                    ..bright
                };
                px_label(frame, voice.wave, wave_cx, cy, wc, false);

                // Phosphor glow — faint warm rect behind fresh active notes.
                if row_offset < 5 {
                    let gw = col_w * 0.60;
                    let gh = ROW_H * 0.65;
                    frame.fill_rectangle(
                        Point::new(col_x + (col_w - gw) * 0.5, cy - gh * 0.5),
                        Size::new(gw, gh),
                        Color {
                            r: bright.r * 0.18,
                            g: bright.g * 0.18,
                            b: bright.b * 0.18,
                            a: (alpha * 0.40).min(1.0),
                        },
                    );
                }
            } else {
                // Silent: dim dot pair.
                let cx = col_x + col_w * 0.5;
                let dc = Color {
                    a: alpha * 0.55,
                    ..SID_DIM[sid]
                };
                px_label(frame, "·  ·", cx, cy, dc, false);
            }
        }
    }

    // ── Footer status bar ─────────────────────────────────────────────────────
    let foot_top = h - 14.0;
    frame.fill_rectangle(
        Point::new(0.0, foot_top),
        Size::new(w, 14.0),
        Color::from_rgb(0.025, 0.04, 0.03),
    );
    frame.fill_rectangle(
        Point::new(0.0, foot_top),
        Size::new(w, 1.0),
        Color {
            r: 0.12,
            g: 0.22,
            b: 0.15,
            a: 1.0,
        },
    );

    if let Some(last) = history.back() {
        let mut x = GUTTER_W + 8.0;
        let foot_cy = foot_top + 7.0;
        for sid in 0..num_sids {
            let bright = SID_BRIGHT[sid];
            // Show note for voice 0 of each SID in the footer.
            let v0 = &last.voices[sid * 3];
            let note = if v0.gate { v0.note } else { "---" };
            let wave = if v0.gate { v0.wave } else { "" };
            let lbl = intern_str(format!("S{}: {}  {}", sid + 1, note, wave));
            px_label(frame, lbl, x + col_w * 1.0, foot_cy, bright, false);
            x += col_w * 3.0;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Label drawing helper
// ─────────────────────────────────────────────────────────────────────────────

/// Draw `s` centred horizontally at `cx`, vertically at `cy`.
/// Uses iced's built-in monospace font at size 9 (10 when `bright`).
fn px_label(frame: &mut Frame, s: &str, cx: f32, cy: f32, color: Color, bright: bool) {
    let size = if bright { 10.0_f32 } else { 9.0 };
    // Approximate rendered width so we can centre the text.
    // Monospace at 9 px ≈ 5.4 px/char; at 10 px ≈ 6 px/char.
    let char_w = if bright { 6.0_f32 } else { 5.4 };
    let text_w = s.len() as f32 * char_w;
    let x = cx - text_w * 0.5;
    let y = cy - size * 0.65; // shift up slightly to centre cap-height

    frame.fill_text(Text {
        content: s.to_owned(),
        position: Point::new(x, y),
        color,
        size: iced::Pixels(size),
        font: Font::MONOSPACE,
        align_x: iced::alignment::Horizontal::Left.into(),
        align_y: iced::alignment::Vertical::Top.into(),
        line_height: iced::widget::text::LineHeight::Relative(1.0),
        shaping: iced::widget::text::Shaping::Basic,
        max_width: f32::INFINITY,
    });
}

// ─────────────────────────────────────────────────────────────────────────────
//  Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Build the combined tracker + SID register panel.
///
/// # Parameters
/// - `tracker_view`    — owns the Canvas cache; call `invalidate()` every tick.
/// - `tracker_history` — ring buffer of decoded frames.
/// - `sid_regs`        — raw 128-byte register shadow from `PlayerStatus`.
/// - `num_sids`        — 1–4.
/// - `is_pal`          — PAL vs NTSC clock.
/// - `tracker_height`  — logical pixels for the tracker canvas.
///                       Recommended 260 – 360 for a comfortable view.
pub fn sid_panel<'a>(
    tracker_view: &'a TrackerView,
    tracker_history: &'a TrackerHistory,
    sid_regs: &[u8],
    num_sids: usize,
    is_pal: bool,
    tracker_height: f32,
) -> Element<'a, Message> {
    // Nothing playing — friendly placeholder.
    if sid_regs.is_empty() || sid_regs.iter().all(|&b| b == 0) {
        return container(
            column![
                Space::new().height(Length::Fixed(60.0)),
                text("Load a tune to see SID register state")
                    .size(13)
                    .color(Color::from_rgb(0.4, 0.4, 0.5)),
            ]
            .align_x(Alignment::Center)
            .width(Length::Fill),
        )
        .padding(40)
        .center_x(Length::Fill)
        .into();
    }

    let n = num_sids.clamp(1, 4);

    // ── Tracker canvas (top section) ──────────────────────────────────────────
    let tracker_elem = tracker_view.view(tracker_history, n, tracker_height);

    // ── Register detail panels (bottom section) ───────────────────────────────
    // The chip panels must align with the tracker canvas columns.
    // The canvas has GUTTER_W px on the left, then equal columns per voice.
    // We mirror this: a fixed-width Space matches the gutter, then each SID
    // panel gets equal Fill width (each covers 3 voice columns worth of space).
    // No rule::vertical separators — the tracker's own column lines are the dividers.
    let mut chips: Vec<Element<'a, Message>> = Vec::with_capacity(n);
    for sid in 0..n {
        chips.push(sid_chip_panel(sid_regs, sid, is_pal));
    }

    let chip_row = iced::widget::Row::with_children({
        let mut items: Vec<Element<'a, Message>> = Vec::new();
        // Gutter spacer — matches GUTTER_W in the canvas exactly.
        items.push(Space::new().width(Length::Fixed(GUTTER_W)).into());
        for (i, chip) in chips.into_iter().enumerate() {
            if i > 0 {
                // 1 px separator aligned with the SID-boundary lines in the canvas.
                items.push(rule::vertical(1).into());
            }
            items.push(chip);
        }
        items
    })
    .spacing(0)
    .height(Length::Shrink);

    let full = column![
        tracker_elem,
        rule::horizontal(1),
        container(chip_row)
            .width(Length::Fill)
            .height(Length::Shrink)
            .padding(Padding::default().top(4).bottom(6)),
    ]
    .spacing(0)
    .height(Length::Fill);

    container(full)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.04, 0.06, 0.05))),
            ..Default::default()
        })
        .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Per-chip register panel
// ─────────────────────────────────────────────────────────────────────────────

fn sid_chip_panel<'a>(regs: &[u8], sid: usize, is_pal: bool) -> Element<'a, Message> {
    let base = sid * SID_STRIDE;
    let accent = SID_ACCENT.get(sid).copied().unwrap_or(Color::WHITE);
    let label = format!("SID {}", sid + 1);

    let mut col = Column::new().spacing(5);
    col = col.push(text(label).size(11).color(accent));
    col = col.push(rule::horizontal(1));

    for voice in 0..3 {
        col = col.push(voice_row(regs, base, voice, accent, is_pal));
        if voice < 2 {
            col = col.push(rule::horizontal(1));
        }
    }

    col = col.push(rule::horizontal(1));
    col = col.push(global_row(regs, base, accent));

    container(col)
        .width(Length::Fill)
        .padding(Padding::from([3, 8]))
        .into()
}

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
    let pw_pct = pw_val as f32 / 40.95;

    let gate_color = if gate {
        accent
    } else {
        dim_color(accent, 0.25)
    };
    let gate_label = if gate { "GATE" } else { "    " };
    let label_color = if gate {
        accent
    } else {
        dim_color(accent, 0.40)
    };
    let vl = ["V1", "V2", "V3"][voice];

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

    let dc = Color::from_rgb(0.40, 0.44, 0.48);
    let vc = Color::from_rgb(0.82, 0.86, 0.90);

    let pulse_active = (ctrl >> 4) & 0x04 != 0;
    let pulse_row: Element<'a, Message> = row![
        Space::new().width(24),
        lbl("PW", if pulse_active { dc } else { Color::TRANSPARENT }),
        Space::new().width(3),
        lbl(
            if pulse_active {
                format!("{:.0}%  (${:03X})", pw_pct, pw_val)
            } else {
                String::new()
            },
            vc,
        ),
    ]
    .align_y(Alignment::Center)
    .into();

    column![
        row![
            text(vl)
                .size(11)
                .color(label_color)
                .width(Length::Fixed(18.0)),
            Space::new().width(4),
            text(wave).size(11).color(vc).width(Length::Fixed(58.0)),
            text(flags).size(10).color(dc).width(Length::Fill),
            text(gate_label).size(10).color(gate_color),
        ]
        .align_y(Alignment::Center),
        row![
            Space::new().width(22),
            text(note).size(11).color(vc).width(Length::Fixed(34.0)),
            text(hz_str).size(10).color(dc),
        ]
        .align_y(Alignment::Center),
        row![
            Space::new().width(22),
            lbl("A", dc),
            Space::new().width(2),
            lbl(ATTACK_TIMES[attack], vc),
            Space::new().width(6),
            lbl("D", dc),
            Space::new().width(2),
            lbl(DECAY_TIMES[decay], vc),
            Space::new().width(6),
            lbl("S", dc),
            Space::new().width(2),
            lbl(format!("{}", sustain), vc),
            Space::new().width(6),
            lbl("R", dc),
            Space::new().width(2),
            lbl(RELEASE_TIMES[release], vc),
        ]
        .align_y(Alignment::Center),
        pulse_row,
    ]
    .spacing(2)
    .padding(Padding::from([3, 0]))
    .into()
}

fn global_row<'a>(regs: &[u8], base: usize, accent: Color) -> Element<'a, Message> {
    let safe = |i: usize| regs.get(i).copied().unwrap_or(0);
    let flt_lo = safe(base + 0x15) & 0x07;
    let flt_hi = safe(base + 0x16);
    let flt_word = ((flt_hi as u16) << 3) | flt_lo as u16;
    let flt_ctrl = safe(base + 0x17);
    let resonance = (flt_ctrl >> 4) as usize;
    let route_v1 = flt_ctrl & 0x01 != 0;
    let route_v2 = flt_ctrl & 0x02 != 0;
    let route_v3 = flt_ctrl & 0x04 != 0;
    let route_ext = flt_ctrl & 0x08 != 0;
    let mode_vol = safe(base + 0x18);
    let volume = mode_vol & 0x0F;
    let lp = mode_vol & 0x10 != 0;
    let bp = mode_vol & 0x20 != 0;
    let hp = mode_vol & 0x40 != 0;
    let v3_off = mode_vol & 0x80 != 0;

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

    let dc = Color::from_rgb(0.40, 0.44, 0.48);
    let vc = Color::from_rgb(0.82, 0.86, 0.90);
    let hc = dim_color(accent, 0.70);

    let v3off_badge: Element<'a, Message> = row![
        Space::new().width(8),
        lbl(
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
        text("GLOBAL").size(10).color(hc),
        row![
            lbl("VOL", dc),
            Space::new().width(3),
            lbl(format!("{}/15", volume), vc),
            Space::new().width(8),
            lbl("FLT", dc),
            Space::new().width(3),
            lbl(format!("${:03X}", flt_word), vc),
            Space::new().width(8),
            lbl("RES", dc),
            Space::new().width(3),
            lbl(format!("{}/15", resonance), vc),
        ]
        .align_y(Alignment::Center),
        row![
            lbl("MODE", dc),
            Space::new().width(3),
            lbl(&mode, vc),
            Space::new().width(8),
            lbl("ROUTE", dc),
            Space::new().width(3),
            lbl(&routing, vc),
            v3off_badge,
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(2)
    .padding(Padding::from([3, 0]))
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn lbl<'a>(s: impl ToString, color: Color) -> Element<'a, Message> {
    text(s.to_string()).size(10).color(color).into()
}

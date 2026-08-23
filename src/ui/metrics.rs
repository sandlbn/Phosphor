// Toolbar width measurement, so the labels-vs-icons switch happens when the
// labels genuinely stop fitting.
//
// This measures the REAL shaped text through iced's font stack rather than
// estimating from a per-character table. An earlier version used such a table
// and over-estimated by 25-50% (it put "Files" at 37px against a real 25px),
// which stripped the labels while there was still plenty of room. A table also
// can't be right for everyone: the font, the point size and the display scale
// all differ per user, and emoji advances vary by fallback font.
//
// Shaping ~15 labels costs ~0.8 ms, which is far too much to repeat in every
// `view()` at 30 fps, so results are memoised. The labels are static and the
// size changes only when the user edits it, so the cache is a handful of
// entries and effectively permanent.
//
// Paddings and spacings below are the literal constants the toolbar widgets
// use, not guesses — they are raw `u16` in the UI and deliberately do not
// scale with the font, which is why the total is affine in the text size
// rather than proportional to it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Width of `s` shaped in the real UI font at `pt`, in pixels.
///
/// `Shaping::Advanced` so emoji and non-Latin glyphs measure correctly — the
/// toolbar is mostly emoji, and basic shaping would mis-measure every one.
fn shape_width(s: &str, pt: f32) -> f32 {
    // cosmic-text panics outright on a zero line height, and a non-positive
    // size is meaningless anyway. Guard rather than let a stray font setting
    // take the process down.
    if !(pt > 0.0) || s.is_empty() {
        return 0.0;
    }
    use iced::advanced::graphics::text::Paragraph;
    use iced::advanced::text::{LineHeight, Paragraph as _, Shaping, Text, Wrapping};
    use iced::{alignment, Font, Pixels, Size};

    Paragraph::with_text(Text {
        content: s,
        bounds: Size::INFINITE,
        size: Pixels(pt),
        line_height: LineHeight::default(),
        font: Font::default(),
        align_x: alignment::Horizontal::Left.into(),
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::None,
    })
    .min_width()
}

/// Memoised `shape_width`. Keyed on the text and the exact size bits, so a
/// font-size change re-measures rather than serving a stale width.
pub fn text_px(s: &str, pt: f32) -> f32 {
    static CACHE: OnceLock<Mutex<HashMap<(String, u32), f32>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (s.to_owned(), pt.to_bits());
    if let Ok(map) = cache.lock() {
        if let Some(&w) = map.get(&key) {
            return w;
        }
    }
    let w = shape_width(s, pt);
    if let Ok(mut map) = cache.lock() {
        map.insert(key, w);
    }
    w
}

/// Button width: measured label plus the horizontal padding on both sides.
/// Padding is a raw constant in the UI, so it does not scale with the font.
pub fn button_px(label: &str, pt: f32, pad_h: f32) -> f32 {
    text_px(label, pt) + pad_h * 2.0
}

/// Toolbar state that changes how wide the rows need to be.
#[derive(Debug, Clone, Default)]
pub struct ToolbarInputs {
    pub repeat_label: String,
    pub shuffle_on: bool,
    pub has_remote_pill: bool,
    pub has_update_badge: bool,
    pub update_version_len: usize,
}

// Non-compact geometry, mirroring `controls_bar`.
const PAD_H: f32 = 10.0; // small_button
const ACCENT_PAD_H: f32 = 12.0; // accent_button (Library)
const GROUP_SPACING: f32 = 4.0;
const ROW_SPACING: f32 = 8.0;
const BAR_PAD_H: f32 = 16.0;
const SEP_PX: f32 = 9.0; // 1 px rule + [0,4] padding

fn group_px(labels: &[&str], pt: f32, pad_h: f32) -> f32 {
    let text: f32 = labels.iter().map(|l| button_px(l, pt, pad_h)).sum();
    text + GROUP_SPACING * labels.len().saturating_sub(1) as f32
}

/// Width the bottom row (file ops, Library, panel toggles) needs with labels.
pub fn controls_bottom_row_px(inputs: &ToolbarInputs, pt: f32) -> f32 {
    let file_ops = group_px(
        &["➕ Files", "📁 Folder", "📂 Open", "💾 Save", "🗑 Clear"],
        pt,
        PAD_H,
    );
    let library = button_px("📚 Library", pt, ACCENT_PAD_H);
    let toggles = group_px(&["🕐 Recent", "SID", "🔧 Device", "⚙ Settings"], pt, PAD_H);

    let mut extras = 0.0;
    if inputs.has_remote_pill {
        extras += button_px("● Remote", pt, PAD_H) + ROW_SPACING;
    }
    if inputs.has_update_badge {
        let v = "⬆ ".to_string() + &"0".repeat(inputs.update_version_len.max(1));
        extras += button_px(&v, pt, PAD_H) + ROW_SPACING;
    }

    file_ops + library + toggles + SEP_PX * 2.0 + ROW_SPACING * 4.0 + BAR_PAD_H * 2.0 + extras
}

/// Width the top row (transport, subtune, shuffle/repeat) needs with labels.
pub fn controls_top_row_px(inputs: &ToolbarInputs, pt: f32) -> f32 {
    let transport = group_px(&["◄◄", "▶", "■", "►►", "🎲"], pt, PAD_H);
    let subtune = group_px(&["◄ tune", "tune ►"], pt, PAD_H);
    let shuffle = if inputs.shuffle_on {
        "🔀 On"
    } else {
        "🔀 Off"
    };
    let repeat = if inputs.repeat_label.is_empty() {
        "⮔ Off"
    } else {
        inputs.repeat_label.as_str()
    };
    let mode = group_px(&[shuffle, repeat], pt, PAD_H);

    transport + subtune + mode + SEP_PX * 2.0 + ROW_SPACING * 4.0 + BAR_PAD_H * 2.0
}

/// No headroom: switch only when the labels genuinely do not fit.
///
/// An earlier version carried a margin to absorb error in a per-character
/// width estimate. The widths are now measured by shaping the real text, so
/// there is no error to absorb, and any margin would drop the labels while
/// there was still room — the exact complaint this replaced.
///
/// Erring on the side of keeping labels is safe: `Wrapping::None` means an
/// overflowing label clips rather than wrapping to a second line, so the worst
/// case is a slightly cropped word, not a broken toolbar.
const SAFETY_FACTOR: f32 = 1.0;
const SAFETY_PX: f32 = 0.0;

/// Should the toolbar drop its labels and show icons only?
///
/// Driven by the wider of the two rows. One flag for both, because the rows
/// share `btn_size`/`btn_pad`/`row_spacing` and a labelled row sitting above
/// an icon-only row reads as a bug.
pub fn toolbar_is_compact(window_width: f32, inputs: &ToolbarInputs, pt: f32) -> bool {
    let need = controls_bottom_row_px(inputs, pt).max(controls_top_row_px(inputs, pt));
    window_width < need * SAFETY_FACTOR + SAFETY_PX
}

/// Width the bottom row needs once labels are gone. Exists as an ordering
/// check against the labelled width — the icon row must always be narrower,
/// or dropping labels wouldn't buy anything.
#[cfg(test)]
pub fn controls_icons_px(inputs: &ToolbarInputs, pt: f32) -> f32 {
    let file_ops = group_px(&["➕", "📁", "📂", "💾", "🗑"], pt, 6.0);
    let library = button_px("📚", pt, 8.0);
    let toggles = group_px(&["🕐", "SID", "🔧", "⚙"], pt, 6.0);
    let mut extras = 0.0;
    if inputs.has_remote_pill {
        extras += button_px("●", pt, 6.0) + ROW_SPACING;
    }
    if inputs.has_update_badge {
        extras += button_px("⬆", pt, 6.0) + ROW_SPACING;
    }
    file_ops + library + toggles + SEP_PX * 2.0 + ROW_SPACING * 4.0 + 12.0 * 2.0 + extras
}

/// `track_info_bar`'s threshold. Cosmetic (visualiser 300 -> 200 px), so it
/// doesn't need the estimator — but it does need to be affine. At scale 1.0
/// this is exactly the 760.0 it replaces, so the default look is unchanged.
pub const INFO_BAR_FIXED_PX: f32 = 460.0;
pub const INFO_BAR_TEXT_PX: f32 = 300.0;

pub fn info_bar_is_compact(window_width: f32, scale: f32) -> bool {
    window_width < INFO_BAR_FIXED_PX + INFO_BAR_TEXT_PX * scale
}

#[cfg(test)]
mod tests {
    use super::*;

    const PT: f32 = 12.0;

    fn inputs() -> ToolbarInputs {
        ToolbarInputs {
            repeat_label: "⮔ Off".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn measurement_is_real_and_monotonic() {
        assert!(text_px("Settings", PT) > text_px("Set", PT));
        // A space is what makes "⚙ Settings" wrappable, so it must count.
        assert!(text_px("⚙ Settings", PT) > text_px("⚙", PT));
        // Bigger type measures wider — this is what makes the whole thing
        // follow the user's font size instead of a baked-in table.
        assert!(text_px("Settings", PT * 2.0) > text_px("Settings", PT));
    }

    #[test]
    fn memoisation_returns_the_same_value() {
        // Shaping costs ~50us per label, far too much for every frame, so the
        // result is cached. A stale or key-colliding cache would show up here.
        let a = text_px("⚙ Settings", PT);
        let b = text_px("⚙ Settings", PT);
        assert_eq!(a, b);
        assert_ne!(text_px("⚙ Settings", PT), text_px("⚙ Settings", PT * 1.5));
    }

    #[test]
    fn row_width_is_affine_in_text_size() {
        // Padding is a raw constant that does not scale, so the total keeps a
        // fixed term. A purely proportional model (`760 * scale`) gets this
        // wrong in the opposite direction to the old per-character table.
        let i = inputs();
        // Padding survives even when the text measures nothing.
        assert!(controls_bottom_row_px(&i, 0.0) > 0.0);
        // Equal steps in size give equal steps in width => affine, not
        // proportional.
        let d1 = controls_bottom_row_px(&i, 24.0) - controls_bottom_row_px(&i, 12.0);
        let d2 = controls_bottom_row_px(&i, 36.0) - controls_bottom_row_px(&i, 24.0);
        assert!(
            (d1 - d2).abs() / d1.max(1.0) < 0.05,
            "expected near-equal deltas, got {d1} and {d2}"
        );
    }

    #[test]
    fn compact_triggers_only_when_labels_really_do_not_fit() {
        // The reported bug: an over-estimating width table stripped the labels
        // while there was still plenty of room. Compact must be false at any
        // width that genuinely fits the measured row.
        let i = inputs();
        for &pt in &[8.0f32, 12.0, 18.0, 24.0] {
            let need = controls_bottom_row_px(&i, pt).max(controls_top_row_px(&i, pt));
            assert!(
                !toolbar_is_compact(need + 1.0, &i, pt),
                "labels fit at {need} but were dropped (pt {pt})"
            );
            assert!(
                toolbar_is_compact(need - 1.0, &i, pt),
                "must be compact 1px short of {need} (pt {pt})"
            );
        }
    }

    #[test]
    fn default_window_keeps_its_labels() {
        // DEFAULT_WINDOW_WIDTH is 900. With real measurement the labelled row
        // fits comfortably, so a default-sized window must show text.
        assert!(!toolbar_is_compact(900.0, &inputs(), PT));
    }

    #[test]
    fn icon_mode_is_strictly_narrower() {
        let i = inputs();
        for &pt in &[8.0f32, 12.0, 24.0] {
            assert!(controls_icons_px(&i, pt) < controls_bottom_row_px(&i, pt));
        }
    }

    #[test]
    fn extras_widen_the_requirement() {
        let base = inputs();
        let pill = ToolbarInputs {
            has_remote_pill: true,
            ..base.clone()
        };
        let both = ToolbarInputs {
            has_remote_pill: true,
            has_update_badge: true,
            update_version_len: 5,
            ..base.clone()
        };
        let w = |i: &ToolbarInputs| controls_bottom_row_px(i, PT);
        assert!(w(&pill) > w(&base));
        assert!(w(&both) > w(&pill));
    }

    #[test]
    fn info_bar_threshold_matches_old_behaviour_at_scale_one() {
        // Exactly 760.0 at scale 1.0 => no visual change by default...
        assert!(info_bar_is_compact(759.0, 1.0));
        assert!(!info_bar_is_compact(760.0, 1.0));
        // ...but it now moves with the font size, which 760.0 never did.
        assert!(info_bar_is_compact(900.0, 2.0));
    }
}

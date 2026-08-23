// Width estimation for the toolbar, so the compact/icon threshold is derived
// rather than guessed.
//
// The old rule was `window_width < 760.0`. Two things were wrong with it:
//
//   * The labelled bottom row actually needs ~854 px at scale 1.0, so anything
//     in the 760..880 band overflowed while `compact` was still false. Worse,
//     the Remote pill (+~95) and update badge (+~76) push that past 1000 — the
//     default 900 px window overflows when both are showing.
//   * A row's width is *affine*, not linear: `font::sized()` scales the text,
//     but paddings and spacings are raw `u16` constants that don't scale. So
//     `760.0 * font::scale()` would be wrong in the other direction.
//
// Nothing here touches a renderer, so it can only approximate glyph advances.
// It deliberately over-estimates: guessing high costs an early switch to icons
// (harmless), guessing low costs clipped labels (visible). If the app ever sets
// an explicit default font, re-check the em table below.
//
// Every function takes `scale` as a parameter and never reads `font::scale()`
// itself — that is a process-global atomic, and tests mutating it would race
// under cargo test's default parallelism.

/// Approximate advance width of `s`, in em units at 1.0 pt.
pub fn em_width(s: &str) -> f32 {
    s.chars()
        .map(|c| match c {
            ' ' => 0.30,
            c if !c.is_ascii() => 1.10, // emoji, arrows, box drawing
            c if c.is_ascii_uppercase() => 0.72,
            c if c.is_ascii_lowercase() || c.is_ascii_digit() => 0.60,
            _ => 0.38, // ascii punctuation
        })
        .sum()
}

/// Text width in px at a given design point size and font scale.
pub fn text_px(s: &str, pt: f32, scale: f32) -> f32 {
    em_width(s) * pt * scale
}

/// Button width: scaled text plus horizontal padding on both sides. Padding
/// is a raw constant in the UI, so it is *not* multiplied by `scale`.
pub fn button_px(label: &str, pt: f32, pad_h: f32, scale: f32) -> f32 {
    text_px(label, pt, scale) + pad_h * 2.0
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
const PT: f32 = 12.0;
const PAD_H: f32 = 10.0; // small_button
const ACCENT_PAD_H: f32 = 12.0; // accent_button (Library)
const GROUP_SPACING: f32 = 4.0;
const ROW_SPACING: f32 = 8.0;
const BAR_PAD_H: f32 = 16.0;
const SEP_PX: f32 = 9.0; // 1 px rule + [0,4] padding

fn group_px(labels: &[&str], pt: f32, pad_h: f32, scale: f32) -> f32 {
    let text: f32 = labels.iter().map(|l| button_px(l, pt, pad_h, scale)).sum();
    text + GROUP_SPACING * labels.len().saturating_sub(1) as f32
}

/// Width the bottom row (file ops, Library, panel toggles) needs with labels.
pub fn controls_bottom_row_px(inputs: &ToolbarInputs, scale: f32) -> f32 {
    let file_ops = group_px(
        &["➕ Files", "📁 Folder", "📂 Open", "💾 Save", "🗑 Clear"],
        PT,
        PAD_H,
        scale,
    );
    let library = button_px("📚 Library", PT, ACCENT_PAD_H, scale);
    let toggles = group_px(
        &["🕐 Recent", "SID", "🔧 Device", "⚙ Settings"],
        PT,
        PAD_H,
        scale,
    );

    let mut extras = 0.0;
    if inputs.has_remote_pill {
        extras += button_px("● Remote", PT, PAD_H, scale) + ROW_SPACING;
    }
    if inputs.has_update_badge {
        let v = "⬆ ".to_string() + &"0".repeat(inputs.update_version_len.max(1));
        extras += button_px(&v, PT, PAD_H, scale) + ROW_SPACING;
    }

    file_ops + library + toggles + SEP_PX * 2.0 + ROW_SPACING * 4.0 + BAR_PAD_H * 2.0 + extras
}

/// Width the top row (transport, subtune, shuffle/repeat) needs with labels.
pub fn controls_top_row_px(inputs: &ToolbarInputs, scale: f32) -> f32 {
    let transport = group_px(&["◄◄", "▶", "■", "►►", "🎲"], PT, PAD_H, scale);
    let subtune = group_px(&["◄ tune", "tune ►"], PT, PAD_H, scale);
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
    let mode = group_px(&[shuffle, repeat], PT, PAD_H, scale);

    transport + subtune + mode + SEP_PX * 2.0 + ROW_SPACING * 4.0 + BAR_PAD_H * 2.0
}

/// Headroom before switching, absorbing estimator error.
///
/// Kept deliberately small. Two independent reconstructions put the labelled
/// row at 854-871 px, and the default window is 900 — a larger cushion would
/// strip labels at the default size, which is a real cost for no benefit.
/// 3% covers the spread between those two estimates.
const SAFETY_FACTOR: f32 = 1.03;
const SAFETY_PX: f32 = 0.0;

/// Should the toolbar drop its labels and show icons only?
///
/// Driven by the wider of the two rows. One flag for both, because the rows
/// share `btn_size`/`btn_pad`/`row_spacing` and a labelled row sitting above
/// an icon-only row reads as a bug.
pub fn toolbar_is_compact(window_width: f32, inputs: &ToolbarInputs, scale: f32) -> bool {
    let need = controls_bottom_row_px(inputs, scale).max(controls_top_row_px(inputs, scale));
    window_width < need * SAFETY_FACTOR + SAFETY_PX
}

/// Width the bottom row needs once labels are gone. Exists as an ordering
/// check against the labelled width — the icon row must always be narrower,
/// or dropping labels wouldn't buy anything.
#[cfg(test)]
pub fn controls_icons_px(inputs: &ToolbarInputs, scale: f32) -> f32 {
    let file_ops = group_px(&["➕", "📁", "📂", "💾", "🗑"], PT, 6.0, scale);
    let library = button_px("📚", PT, 8.0, scale);
    let toggles = group_px(&["🕐", "SID", "🔧", "⚙"], PT, 6.0, scale);
    let mut extras = 0.0;
    if inputs.has_remote_pill {
        extras += button_px("●", PT, 6.0, scale) + ROW_SPACING;
    }
    if inputs.has_update_badge {
        extras += button_px("⬆", PT, 6.0, scale) + ROW_SPACING;
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

    fn inputs() -> ToolbarInputs {
        ToolbarInputs {
            repeat_label: "⮔ Off".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn em_width_is_monotonic() {
        assert_eq!(em_width(""), 0.0);
        assert!(em_width("Settings") > em_width("Set"));
        // A space is what makes "⚙ Settings" wrappable, so it must count.
        assert!(em_width("⚙ Settings") > em_width("⚙Settings"));
        assert!(em_width("⚙ Settings") > em_width("⚙"));
    }

    #[test]
    fn row_width_is_affine_not_linear() {
        let i = inputs();
        // Equal per-unit-scale deltas => affine in `scale`.
        let d1 = controls_bottom_row_px(&i, 2.0) - controls_bottom_row_px(&i, 1.0);
        let d2 = controls_bottom_row_px(&i, 3.0) - controls_bottom_row_px(&i, 2.0);
        assert!((d1 - d2).abs() < 0.01, "{d1} vs {d2}");

        // The fixed padding term survives scale -> 0. This is exactly what a
        // naive `760.0 * font::scale()` gets wrong.
        assert!(controls_bottom_row_px(&i, 0.0) > 0.0);
    }

    #[test]
    fn compact_triggers_before_the_row_overflows() {
        // The regression test for the reported bug: at scale 1.0 the row needs
        // ~854 px, and the old rule returned "not compact" for anything >= 760.
        let i = inputs();
        for &s in &[0.5_f32, 1.0, 1.5, 2.0, 3.0] {
            let need = controls_bottom_row_px(&i, s);
            assert!(
                toolbar_is_compact(need - 1.0, &i, s),
                "must be compact at scale {s} when 1px short of {need}"
            );
            assert!(
                !toolbar_is_compact(need * 2.0, &i, s),
                "must not be compact at scale {s} with double the room"
            );
        }
    }

    #[test]
    fn old_fixed_threshold_would_have_missed_the_bug() {
        // Documents the failure band: at the default font the row needs more
        // than the 760 the old rule used.
        let need = controls_bottom_row_px(&inputs(), 1.0);
        assert!(
            need > 760.0,
            "expected the row to exceed the old threshold, got {need}"
        );
        assert!(need < 1000.0, "sanity: measured ~854, got {need}");
    }

    #[test]
    fn default_window_keeps_its_labels() {
        // DEFAULT_WINDOW_WIDTH is 900 and the plain labelled row needs ~871,
        // so it fits. Pins the safety margin: if someone widens it enough to
        // flip this, that is a visible regression in information density.
        assert!(!toolbar_is_compact(900.0, &inputs(), 1.0));
        // The reported failure width must still go to icons.
        assert!(toolbar_is_compact(781.0, &inputs(), 1.0));
    }

    #[test]
    fn icon_mode_is_strictly_narrower() {
        let i = inputs();
        for &s in &[0.5_f32, 1.0, 2.0, 3.0] {
            assert!(controls_icons_px(&i, s) < controls_bottom_row_px(&i, s));
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
        let w = |i: &ToolbarInputs| controls_bottom_row_px(i, 1.0);
        assert!(w(&pill) > w(&base));
        assert!(w(&both) > w(&pill));
        // Both showing pushes a default 900px window over.
        assert!(w(&both) > 900.0, "got {}", w(&both));
    }

    #[test]
    fn button_px_includes_unscaled_padding() {
        // Padding is a raw constant in the UI and must not be scaled, or the
        // estimate drifts badly at large font sizes.
        let a = button_px("Save", 12.0, 10.0, 1.0);
        let b = button_px("Save", 12.0, 10.0, 2.0);
        assert!((b - a - text_px("Save", 12.0, 1.0)).abs() < 0.01);
    }

    #[test]
    fn info_bar_threshold_matches_old_behaviour_at_scale_one() {
        // Exactly 760.0 at scale 1.0 => no visual change by default.
        assert!(info_bar_is_compact(759.0, 1.0));
        assert!(!info_bar_is_compact(760.0, 1.0));
        // ...but it now moves with the font size, which 760.0 never did.
        assert!(info_bar_is_compact(900.0, 2.0));
    }
}

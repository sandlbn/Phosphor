// Global font scale.
//
// iced 0.14 has no built-in scale knob, so every `text(...).size(N)` call
// site multiplies its literal by the live scale on render. The scale is
// stored as an `f32`'s bit pattern in an `AtomicU32` (no `AtomicF32` exists)
// so it can be updated from anywhere without locking.

use std::sync::atomic::{AtomicU32, Ordering};

/// Reference base size that the existing UI literals were authored against.
/// User-configured base sizes are interpreted as "what 12pt should be now".
const DESIGN_BASE: f32 = 12.0;

/// Live scale ratio = `user_base / DESIGN_BASE`. Default 1.0.
static SCALE_BITS: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0_f32.to_bits()

/// Update the global scale from a user-supplied base size in points.
/// Clamped to a generous range so a typo can't make the UI unusable.
pub fn set_base(base_pt: f32) {
    let scale = (base_pt / DESIGN_BASE).clamp(0.5, 3.0);
    SCALE_BITS.store(scale.to_bits(), Ordering::Relaxed);
}

/// Current scale ratio.
pub fn scale() -> f32 {
    f32::from_bits(SCALE_BITS.load(Ordering::Relaxed))
}

/// Multiply a design-time pt size by the current scale. Use this at every
/// `text(...).size(...)` call site so the UI scales as a whole.
#[inline]
pub fn sized(pt: f32) -> f32 {
    pt * scale()
}

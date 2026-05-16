// Global master-volume scalar applied to host-side audio output.
//
// All three cpal-driven engines (emulated, sidlite, U64 streaming) read
// `scale()` inside their audio callback and multiply each sample by it.
// Stored as an `f32` bit-pattern in an `AtomicU32` (no `AtomicF32`) so the
// GUI can update it without locking the audio thread.
//
// Does NOT affect the USB hardware engine (analog out from USBSID-Pico is
// outside Phosphor's reach) — that engine simply ignores the value.

use std::sync::atomic::{AtomicU32, Ordering};

/// Live volume in [0.0, 1.0]. Defaults to 1.0 (unity gain).
static VOLUME_BITS: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0_f32.to_bits()

/// Update the global volume from a UI control value.  Clamped to [0, 1].
pub fn set(volume: f32) {
    let v = volume.clamp(0.0, 1.0);
    VOLUME_BITS.store(v.to_bits(), Ordering::Relaxed);
}

/// Current master volume in [0.0, 1.0]. Read once per audio callback.
#[inline]
pub fn scale() -> f32 {
    f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed))
}

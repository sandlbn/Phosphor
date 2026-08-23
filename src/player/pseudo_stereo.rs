// Pseudo-stereo: widen a mono (1-SID) tune by mirroring SID1's writes to the
// second SID chip with every voice transposed a few cents FLAT, so the two
// chips beat against each other.
//
// Phosphor already mirrors SID1 to SID2 for mono tunes, but byte-identically —
// both chips produce the same signal, which sums back to a centred image. The
// engines hard-pan SID1 to the left channel and SID2 to the right
// (`sid_emulated.rs` / `sid_sidlite.rs`), so making the copy differ is all that
// is needed for genuine width.
//
// SID2 is a pure TRANSPOSITION of SID1 — every voice gets the same ratio. That
// matters musically: control-register bits 1 and 2 (SYNC and RING MOD) make a
// voice's timbre depend on the *ratio* between its frequency and its
// neighbour's. A uniform ratio preserves those ratios exactly, so sync leads
// and ring-mod bells keep their character; detuning voices in opposite
// directions would square the error and change the timbre, not just the pitch.
// It also preserves every interval, arpeggio and chord voicing.
//
// Only the oscillator frequency is altered. Pulse width, control, ADSR, filter
// and volume are mirrored verbatim: they shape timbre and envelope, and
// offsetting them would fight the composer rather than widen the image.

/// How far SID2 is detuned. Flat, never sharp — see `ratio_q16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetuneStrength {
    Subtle,
    #[default]
    Medium,
    Wide,
}

impl DetuneStrength {
    /// Detune in cents. Documentation value; the hot path uses `ratio_q16`.
    ///
    /// 4/8/16 spans the usable band: analogue-synth unison detune sits at
    /// 3-10 cents, and past roughly 25 cents the ear stops hearing "wide" and
    /// starts hearing "out of tune". At A4 these beat at about 1, 2 and 4 Hz.
    pub fn cents(self) -> u32 {
        match self {
            Self::Subtle => 4,
            Self::Medium => 8,
            Self::Wide => 16,
        }
    }

    /// `2^(-cents/1200)` in Q16 fixed point, i.e. scaled by 65536.
    ///
    /// Fixed point rather than `f64::powf` so results are bit-identical across
    /// platforms and the tests can assert exact values.
    ///
    /// Deliberately FLAT (ratio < 1): the transform maps `[0, 0xFFFF]` into
    /// itself, so 16-bit wraparound is structurally impossible. A sharp detune
    /// would need clamping above ~3.8 kHz, and getting that wrong would drop a
    /// note by an octave with an audible click. Flat and sharp are
    /// indistinguishable for a widening effect, so this takes the free safety.
    pub fn ratio_q16(self) -> u32 {
        match self {
            Self::Subtle => 65385, // 2^(-4/1200)
            Self::Medium => 65234, // 2^(-8/1200)
            Self::Wide => 64933,   // 2^(-16/1200)
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Subtle => "subtle",
            Self::Medium => "medium",
            Self::Wide => "wide",
        }
    }

    /// Whitelist parse; anything unrecognised falls back to `Medium` so a
    /// hand-edited config cannot produce an unrepresentable state.
    pub fn from_config_str(s: &str) -> Self {
        match s.trim() {
            "subtle" => Self::Subtle,
            "wide" => Self::Wide,
            _ => Self::Medium,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Subtle => "Subtle",
            Self::Medium => "Medium",
            Self::Wide => "Wide",
        }
    }

    pub const ALL: [Self; 3] = [Self::Subtle, Self::Medium, Self::Wide];
}

/// Voice N's FREQ_LO / FREQ_HI live at `N*7` and `N*7 + 1`.
const VOICE_STRIDE: u8 = 7;
const NUM_VOICES: u8 = 3;
/// First register past the three voices — filter cutoff LO.
const FIRST_NON_VOICE_REG: u8 = VOICE_STRIDE * NUM_VOICES; // 0x15

/// `(voice, is_hi)` for a SID1-local register, or `None` if it is not a
/// frequency byte.
///
/// The `FIRST_NON_VOICE_REG` guard is load-bearing: `0x15 % 7 == 0`, so without
/// it the filter cutoff LO register would be mis-read as a fourth voice's
/// FREQ_LO and filter sweeps would be silently mangled.
fn classify(reg: u8) -> Option<(usize, bool)> {
    if reg >= FIRST_NON_VOICE_REG {
        return None;
    }
    match reg % VOICE_STRIDE {
        0 => Some(((reg / VOICE_STRIDE) as usize, false)),
        1 => Some(((reg / VOICE_STRIDE) as usize, true)),
        _ => None,
    }
}

/// Up to two `(register, value)` writes destined for SID2, in the unified
/// register space. Fixed-size so the hot path never allocates.
pub struct MirrorOut {
    n: usize,
    pairs: [(u8, u8); 2],
}

impl MirrorOut {
    pub fn iter(&self) -> impl Iterator<Item = (u8, u8)> + '_ {
        self.pairs[..self.n].iter().copied()
    }
}

/// Per-voice frequency shadow, so a detuned 16-bit word can be recomputed when
/// either half is written.
pub struct PseudoStereo {
    strength: DetuneStrength,
    /// Last FREQ_LO/HI byte SID1 has been given, per voice. `[voice][0]` = LO.
    ///
    /// Starts all-zero, which is exactly the SID's post-reset register state,
    /// so the shadow is accurate from the first frame with no "is this valid
    /// yet" flags. `setup_playback` creates this alongside `bridge.reset()`,
    /// which is what makes that true — keep the two together.
    src: [[u8; 2]; 3],
    /// Last FREQ_LO/HI byte actually emitted to SID2, to suppress redundant
    /// writes. Also starts all-zero == post-reset SID2.
    ///
    /// Correct only while nothing else writes SID2's frequency registers.
    /// Today nothing does; the only other device write in the player is the
    /// volume write in `setup_playback`.
    dst: [[u8; 2]; 3],
}

impl PseudoStereo {
    pub fn new(strength: DetuneStrength) -> Self {
        Self {
            strength,
            src: [[0; 2]; 3],
            dst: [[0; 2]; 3],
        }
    }

    pub fn strength(&self) -> DetuneStrength {
        self.strength
    }

    /// Detuned 16-bit word for `voice`, from the current shadow.
    fn tuned_word(&self, voice: usize) -> u16 {
        let word = self.src[voice][0] as u32 | ((self.src[voice][1] as u32) << 8);
        // Rounding, not truncation. No minimum delta: at very small words the
        // offset rounds to zero and the voice is simply left alone, which is
        // correct — forcing ±1 LSB there would be a ~17-cent error.
        (((word * self.strength.ratio_q16() + 0x8000) >> 16) as u16).min(0xFFFF)
    }

    /// Record a SID1 write without emitting anything.
    ///
    /// Used to seed the shadow from the INIT register dump, which
    /// `setup_playback` sends straight to the device and does not mirror.
    /// Without this, a tune whose play routine only touches FREQ_HI would be
    /// detuned from a stale LO of zero.
    pub fn observe(&mut self, reg: u8, val: u8) {
        if let Some((voice, is_hi)) = classify(reg) {
            self.src[voice][usize::from(is_hi)] = val;
        }
    }

    /// Record a SID1 write and return the SID2 writes it should produce.
    ///
    /// Non-frequency registers pass through unchanged, so this is the single
    /// mirroring authority — callers need no separate branch.
    pub fn mirror(&mut self, reg: u8, val: u8) -> MirrorOut {
        let Some((voice, is_hi)) = classify(reg) else {
            return MirrorOut {
                n: 1,
                pairs: [(reg + super::memory::SID_REG_SIZE, val), (0, 0)],
            };
        };

        self.src[voice][usize::from(is_hi)] = val;
        let tuned = self.tuned_word(voice);
        let (lo, hi) = (tuned as u8, (tuned >> 8) as u8);
        let base = (voice as u8) * VOICE_STRIDE + super::memory::SID_REG_SIZE;

        // Emit both halves so SID2's frequency is always internally
        // consistent, but skip whichever byte it already holds.
        let mut out = MirrorOut {
            n: 0,
            pairs: [(0, 0); 2],
        };
        if lo != self.dst[voice][0] {
            out.pairs[out.n] = (base, lo);
            out.n += 1;
        }
        if hi != self.dst[voice][1] {
            out.pairs[out.n] = (base + 1, hi);
            out.n += 1;
        }
        self.dst[voice] = [lo, hi];
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitted(p: &mut PseudoStereo, reg: u8, val: u8) -> Vec<(u8, u8)> {
        p.mirror(reg, val).iter().collect()
    }

    #[test]
    fn filter_cutoff_lo_is_not_mistaken_for_a_fourth_voice() {
        // 0x15 % 7 == 0, so without the FIRST_NON_VOICE_REG guard the filter
        // cutoff LO register reads as voice 3's FREQ_LO and filter sweeps get
        // silently detuned into nonsense.
        assert_eq!(classify(0x15), None);
        assert_eq!(classify(0x16), None);
        assert_eq!(classify(0x17), None);
        assert_eq!(classify(0x18), None);
    }

    #[test]
    fn freq_registers_map_to_the_right_voice_and_byte() {
        assert_eq!(classify(0x00), Some((0, false)));
        assert_eq!(classify(0x01), Some((0, true)));
        assert_eq!(classify(0x07), Some((1, false)));
        assert_eq!(classify(0x08), Some((1, true)));
        assert_eq!(classify(0x0E), Some((2, false)));
        assert_eq!(classify(0x0F), Some((2, true)));
        // Everything else inside the voice block is pulse width / control /
        // ADSR and must pass through untouched.
        for reg in [0x02, 0x03, 0x04, 0x05, 0x06, 0x09, 0x0D, 0x10, 0x14] {
            assert_eq!(classify(reg), None, "reg {reg:#04x}");
        }
    }

    #[test]
    fn lo_then_hi_produces_the_detuned_word_not_a_detuned_byte() {
        // The headline regression. Detuning each byte independently is NOT the
        // same transform as detuning the 16-bit word: 0x1000 scaled as a word
        // is 0x0FDA, whereas scaling the bytes separately gives 0x0F00.
        let mut p = PseudoStereo::new(DetuneStrength::Wide);
        emitted(&mut p, 0x00, 0x00);
        emitted(&mut p, 0x01, 0x10); // word = 0x1000
        let expect = ((0x1000u32 * 64933 + 0x8000) >> 16) as u16;
        assert_eq!(expect, 0x0FDA);
        assert_eq!(p.dst[0], [expect as u8, (expect >> 8) as u8]);
    }

    #[test]
    fn hi_only_write_still_detunes_using_the_shadowed_lo() {
        // Seeded from the INIT dump, which is sent unmirrored.
        let mut p = PseudoStereo::new(DetuneStrength::Medium);
        p.observe(0x00, 0x80);
        emitted(&mut p, 0x01, 0x10); // word = 0x1080, not 0x1000
        let expect = ((0x1080u32 * 65234 + 0x8000) >> 16) as u16;
        assert_eq!(p.dst[0], [expect as u8, (expect >> 8) as u8]);
        assert_ne!(expect, ((0x1000u32 * 65234 + 0x8000) >> 16) as u16);
    }

    #[test]
    fn detune_never_wraps_at_full_scale() {
        // A flat ratio makes overflow structurally impossible. This fails
        // loudly if anyone flips the sign without adding a saturating clamp —
        // a wrapped 0xFFFF would drop the note by an octave with a click.
        for (s, want) in [
            (DetuneStrength::Subtle, 0xFF68u16),
            (DetuneStrength::Medium, 0xFED1),
            (DetuneStrength::Wide, 0xFDA4),
        ] {
            let mut p = PseudoStereo::new(s);
            p.observe(0x00, 0xFF);
            p.observe(0x01, 0xFF);
            assert_eq!(p.tuned_word(0), want, "{s:?}");
            assert!(p.tuned_word(0) < 0xFFFF);
        }
    }

    #[test]
    fn q16_multiplier_matches_the_cent_formula() {
        for s in DetuneStrength::ALL {
            let want = (2f64.powf(-(s.cents() as f64) / 1200.0) * 65536.0).round() as u32;
            assert_eq!(s.ratio_q16(), want, "{s:?}");
            // The fixed-point multiply must not overflow u32 at full scale.
            assert!(0xFFFFu32.checked_mul(s.ratio_q16()).unwrap() + 0x8000 < u32::MAX);
        }
    }

    #[test]
    fn strengths_are_ordered_by_width() {
        assert!(DetuneStrength::Subtle.ratio_q16() > DetuneStrength::Medium.ratio_q16());
        assert!(DetuneStrength::Medium.ratio_q16() > DetuneStrength::Wide.ratio_q16());
        assert!(DetuneStrength::Subtle.cents() < DetuneStrength::Wide.cents());
    }

    #[test]
    fn tiny_frequencies_are_left_alone_rather_than_forced_off_pitch() {
        // A 4-cent offset on word 100 rounds to zero. Forcing a ±1 LSB minimum
        // would be a ~17-cent error on that voice — far worse than no effect.
        let mut p = PseudoStereo::new(DetuneStrength::Subtle);
        p.observe(0x00, 100);
        assert_eq!(p.tuned_word(0), 100);
    }

    #[test]
    fn redundant_writes_are_suppressed() {
        let mut p = PseudoStereo::new(DetuneStrength::Medium);
        assert!(!emitted(&mut p, 0x00, 0x40).is_empty(), "first write emits");
        assert!(
            emitted(&mut p, 0x00, 0x40).is_empty(),
            "identical rewrite emits nothing"
        );
    }

    #[test]
    fn non_frequency_registers_pass_through_untouched() {
        let mut p = PseudoStereo::new(DetuneStrength::Wide);
        for (reg, val) in [(0x04u8, 0x41u8), (0x02, 0x7F), (0x15, 0x30), (0x18, 0x0F)] {
            assert_eq!(
                emitted(&mut p, reg, val),
                vec![(reg + super::super::memory::SID_REG_SIZE, val)],
                "reg {reg:#04x} must mirror verbatim"
            );
        }
    }

    #[test]
    fn strength_round_trips_through_config_strings() {
        for s in DetuneStrength::ALL {
            assert_eq!(DetuneStrength::from_config_str(s.as_config_str()), s);
        }
        assert_eq!(
            DetuneStrength::from_config_str("ludicrous"),
            DetuneStrength::Medium
        );
    }
}

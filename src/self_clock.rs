// Self-clocked cycle stream for USBSID-Pico.
//
// The player hands the USB devices one frame at a time: cycle deltas whose
// first entry is the offset *from the start of the frame*. The firmware
// counts each delta from when the previous write was executed — or, if its
// queue ran dry, from when the packet arrived. Sending each frame "just in
// time" therefore made the frame start depend on USB arrival time, which on
// macOS jitters by ±2–4 ms (measured: 1.9 ms std frame-to-frame). On real
// SIDs that is audible as crackle.
//
// `SelfClock` turns the per-frame deltas into one continuous timeline:
//   * the rest of the previous frame is carried into the first delta, so the
//     board's own clock spaces the frames (measured: 0.023 ms std);
//   * after a (re)start the first write is delayed by PREROLL, so the board
//     is always a few frames ahead and never waits on the host;
//   * empty frames accumulate and are bridged with a no-op write to $1E
//     (unused SID register) before the 16-bit delta overflows;
//   * if the host falls behind far enough that the board must have run
//     dry, the timeline restarts with a fresh pre-roll.

use std::time::Instant;

/// No-op filler target: SID register $1E is unused.
const FILLER_REG: u8 = 0x1E;
/// Pre-roll in seconds (~3 PAL frames).
const PREROLL_SECS: f64 = 0.060;

/// PHOSPHOR_NO_SELFCLOCK=1 restores the old arrival-timed behaviour (A/B
/// testing and a safety valve).
fn bypass() -> bool {
    static B: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *B.get_or_init(|| std::env::var_os("PHOSPHOR_NO_SELFCLOCK").is_some())
}

pub struct SelfClock {
    cycles_per_frame: u32,
    clock_hz: f64,
    /// Frame cycle of the last write emitted in the current frame.
    pos: u32,
    /// Cycles still owed to the timeline, added to the next write's delta.
    carry: u64,
    /// Next write starts a new timeline (pre-roll applied).
    armed: bool,
    /// Wall time at which the current timeline started.
    start: Option<Instant>,
    /// Cycles scheduled on the current timeline (incl. pre-roll).
    scheduled: u64,
}

impl Default for SelfClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfClock {
    pub fn new() -> Self {
        Self {
            cycles_per_frame: 19_656,
            clock_hz: 985_248.0,
            pos: 0,
            carry: 0,
            armed: true,
            start: None,
            scheduled: 0,
        }
    }

    pub fn set_cycles_per_frame(&mut self, cycles: u32) {
        if cycles > 0 {
            self.cycles_per_frame = cycles;
        }
    }

    pub fn set_pal(&mut self, is_pal: bool) {
        self.clock_hz = if is_pal { 985_248.0 } else { 1_022_727.0 };
    }

    /// Start a fresh timeline (tune start, reset, discard).
    pub fn restart(&mut self) {
        self.pos = 0;
        self.carry = 0;
        self.armed = true;
        self.start = None;
        self.scheduled = 0;
    }

    fn preroll_cycles(&self) -> u64 {
        (self.clock_hz * PREROLL_SECS) as u64
    }

    /// True when the board has already played everything scheduled, i.e.
    /// the host fell behind and the self-clocked timeline is broken.
    fn starved(&self, now: Instant) -> bool {
        match self.start {
            Some(t0) => {
                let device_cycles = now.duration_since(t0).as_secs_f64() * self.clock_hz;
                device_cycles > self.scheduled as f64
            }
            None => false,
        }
    }

    /// Lead-in for the first write of a frame: carry plus pre-roll on a
    /// (re)start. Returns cycles to add to that write's delta.
    fn lead_in(&mut self) -> u64 {
        let now = Instant::now();
        let mut extra = std::mem::take(&mut self.carry);
        if self.armed || self.starved(now) {
            if !self.armed {
                eprintln!("[self-clock] host fell behind the board; re-arming pre-roll");
            }
            self.armed = false;
            self.start = Some(now);
            self.scheduled = 0;
            extra = self.preroll_cycles();
        }
        extra
    }

    /// Push `delta` cycles followed by `(reg, val)`, splitting deltas that
    /// don't fit 16 bits with filler writes.
    fn push(&mut self, out: &mut Vec<(u16, u8, u8)>, mut delta: u64, reg: u8, val: u8) {
        while delta > 0xFFFF {
            out.push((0xFFFF, FILLER_REG, 0));
            delta -= 0xFFFF;
            self.scheduled += 0xFFFF;
        }
        out.push((delta as u16, reg, val));
        self.scheduled += delta;
    }

    /// Re-time one frame's writes. Input deltas: first is relative to the
    /// frame start, the rest to the previous write (the player's format).
    pub fn frame_writes(&mut self, writes: &[(u16, u8, u8)]) -> Vec<(u16, u8, u8)> {
        if bypass() {
            return writes.to_vec();
        }
        let mut out = Vec::with_capacity(writes.len() + 1);
        for (i, &(d, reg, val)) in writes.iter().enumerate() {
            self.pos = self.pos.saturating_add(d as u32);
            let mut delta = d as u64;
            if i == 0 {
                delta += self.lead_in();
            }
            self.push(&mut out, delta, reg, val);
        }
        out
    }

    /// End of frame: owe the rest of it to the timeline. Returns filler
    /// writes to send now when the owed cycles approach the 16-bit limit
    /// (long silences), so the board's timeline never stalls.
    pub fn end_frame(&mut self) -> Vec<(u16, u8, u8)> {
        if bypass() {
            return Vec::new();
        }
        if std::env::var_os("PHOSPHOR_CLOCK_DEBUG").is_some() {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            static SUM: AtomicU64 = AtomicU64::new(0);
            static OVER: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            SUM.fetch_add(self.cycles_per_frame as u64, Ordering::Relaxed);
            if self.pos > self.cycles_per_frame {
                OVER.fetch_add((self.pos - self.cycles_per_frame) as u64, Ordering::Relaxed);
            }
            if n.is_multiple_of(250) {
                let buffered = self
                    .start
                    .map(|t0| self.scheduled as f64 / self.clock_hz - t0.elapsed().as_secs_f64());
                eprintln!(
                    "[self-clock] frames {n} avg cpf {:.1} overflow cycles {} buffered {:?} ms",
                    SUM.load(Ordering::Relaxed) as f64 / n as f64,
                    OVER.load(Ordering::Relaxed),
                    buffered.map(|b| (b * 1000.0).round())
                );
            }
        }
        let rest = self.cycles_per_frame.saturating_sub(self.pos) as u64;
        self.pos = 0;
        self.carry += rest;
        let mut out = Vec::new();
        if !self.armed && self.carry > 0xFFFF - self.cycles_per_frame as u64 {
            let d = std::mem::take(&mut self.carry);
            self.push(&mut out, d, FILLER_REG, 0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(v: &[(u16, u8, u8)]) -> u64 {
        v.iter().map(|w| w.0 as u64).sum()
    }

    #[test]
    fn frames_are_spaced_by_frame_length() {
        let mut c = SelfClock::new();
        c.set_cycles_per_frame(19_656);
        let f1 = c.frame_writes(&[(200, 4, 0x11), (8000, 4, 0x09)]);
        assert!(c.end_frame().is_empty());
        let f2 = c.frame_writes(&[(200, 4, 0x11), (8000, 4, 0x09)]);
        // First write of frame 1 carries the pre-roll.
        assert_eq!(f1[0].0 as u64, c.preroll_cycles() + 200);
        // Frame 2's first write lands exactly one frame after frame 1's.
        assert_eq!(f2[0].0 as u32, 19_656 - 8_200 + 200);
        assert_eq!(f2[1].0, 8000);
    }

    #[test]
    fn silence_is_bridged_with_filler() {
        let mut c = SelfClock::new();
        c.frame_writes(&[(10, 0x18, 0x0F)]);
        let mut fill = 0u64;
        let mut fillers = 0;
        for _ in 0..10 {
            let f = c.end_frame();
            fillers += f.len();
            fill += total(&f);
        }
        let next = c.frame_writes(&[(0, 0x18, 0x0F)]);
        // Timeline: 10 frames from the first write's frame start.
        assert!(fillers >= 2);
        assert_eq!(fill + total(&next), 10 * 19_656 - 10);
        assert!(next.iter().all(|w| w.1 == FILLER_REG || w.1 == 0x18));
    }

    #[test]
    fn restart_rearms_preroll() {
        let mut c = SelfClock::new();
        c.frame_writes(&[(5, 1, 1)]);
        c.end_frame();
        c.restart();
        let f = c.frame_writes(&[(5, 1, 1)]);
        assert_eq!(f[0].0 as u64, c.preroll_cycles() + 5);
    }
}

//! CIA interval timer.
//!
//! Each CIA has two 16-bit timers (A and B).  Timer A always counts
//! PHI2 pulses.  Timer B can count PHI2 pulses or Timer-A underflows.
//!
//! The control-register state machine follows the VICE / libsidplayfp
//! implementation.

/// Control-register / state bits (matching libsidplayfp constants).
pub const CIAT_CR_START: u32 = 0x01;
pub const CIAT_STEP: u32 = 0x04;
pub const CIAT_CR_ONESHOT: u32 = 0x08;
pub const CIAT_CR_FLOAD: u32 = 0x10;
pub const CIAT_PHI2IN: u32 = 0x20;
pub const CIAT_CR_MASK: u32 = CIAT_CR_START | CIAT_CR_ONESHOT | CIAT_CR_FLOAD | CIAT_PHI2IN;

pub const CIAT_COUNT2: u32 = 0x100;
pub const CIAT_COUNT3: u32 = 0x200;
pub const CIAT_ONESHOT0: u32 = 0x08 << 8;
pub const CIAT_ONESHOT: u32 = 0x08 << 16;
pub const CIAT_LOAD1: u32 = 0x10 << 8;
pub const CIAT_LOAD: u32 = 0x10 << 16;
pub const CIAT_OUT: u32 = 0x8000_0000;

pub struct Timer {
    pub counter: u16,
    pub latch: u16,
    pub state: u32,
    pub pb_toggle: bool,
    last_control: u8,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            counter: 0xFFFF,
            latch: 0xFFFF,
            state: 0,
            pb_toggle: false,
            last_control: 0,
        }
    }

    pub fn reset(&mut self) {
        self.counter = 0xFFFF;
        self.latch = 0xFFFF;
        self.state = 0;
        self.pb_toggle = false;
        self.last_control = 0;
    }

    pub fn started(&self) -> bool {
        (self.state & CIAT_CR_START) != 0
    }

    pub fn set_control(&mut self, cr: u8) {
        self.state &= !CIAT_CR_MASK;
        self.state |= (cr as u32 & CIAT_CR_MASK) ^ CIAT_PHI2IN;
        self.last_control = cr;
    }

    pub fn latch_lo(&mut self, data: u8) {
        self.latch = (self.latch & 0xFF00) | data as u16;
        if (self.state & CIAT_LOAD) != 0 {
            self.counter = self.latch;
        }
    }

    pub fn latch_hi(&mut self, data: u8) {
        self.latch = (self.latch & 0x00FF) | ((data as u16) << 8);
        if (self.state & CIAT_LOAD) != 0 {
            self.counter = self.latch;
        } else if (self.state & CIAT_CR_START) == 0 {
            self.state |= CIAT_LOAD1;
        }
    }

    /// Called once per Timer-A underflow when Timer-B counts A.
    pub fn cascade_step(&mut self) {
        self.state |= CIAT_STEP;
    }

    /// Advance one PHI2 cycle.  Returns `true` on underflow.
    pub fn tick_phi2(&mut self) -> bool {
        // --- count ---
        if (self.state & CIAT_COUNT3) != 0 {
            self.counter = self.counter.wrapping_sub(1);
        }

        // --- state machine (from VICE ciatimer.c) ---
        let mut adj = self.state & (CIAT_CR_START | CIAT_CR_ONESHOT | CIAT_PHI2IN);

        if (self.state & (CIAT_CR_START | CIAT_PHI2IN)) == (CIAT_CR_START | CIAT_PHI2IN) {
            adj |= CIAT_COUNT2;
        }
        if (self.state & CIAT_COUNT2) != 0
            || (self.state & (CIAT_STEP | CIAT_CR_START)) == (CIAT_STEP | CIAT_CR_START)
        {
            adj |= CIAT_COUNT3;
        }

        adj |= (self.state & (CIAT_CR_FLOAD | CIAT_CR_ONESHOT | CIAT_LOAD1 | CIAT_ONESHOT0)) << 8;
        self.state = adj;

        // --- underflow ---
        let underflow = self.counter == 0 && (self.state & CIAT_COUNT3) != 0;
        if underflow {
            self.state |= CIAT_LOAD | CIAT_OUT;

            if (self.state & (CIAT_ONESHOT | CIAT_ONESHOT0)) != 0 {
                self.state &= !(CIAT_CR_START | CIAT_COUNT2);
            }

            let toggle = (self.last_control & 0x06) == 6;
            self.pb_toggle = toggle && !self.pb_toggle;
        }

        // --- reload ---
        if (self.state & CIAT_LOAD) != 0 {
            self.counter = self.latch;
            self.state &= !CIAT_COUNT3;
        }

        underflow
    }

    /// Get the PB6/PB7 output state.
    pub fn get_pb(&self, reg: u8) -> bool {
        if reg & 0x04 != 0 {
            self.pb_toggle
        } else {
            (self.state & CIAT_OUT) != 0
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

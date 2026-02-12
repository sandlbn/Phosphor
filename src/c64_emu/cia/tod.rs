//! CIA Time-of-Day (TOD) clock.
//!
//! BCD counter: tenths / seconds / minutes / hours (with AM/PM).
//! Driven by the 50 Hz or 60 Hz power-line signal (divided from the
//! system clock).

const TENTHS: usize  = 0;
const SECONDS: usize = 1;
const MINUTES: usize = 2;
const HOURS: usize   = 3;

pub struct Tod {
    clock: [u8; 4],
    latch: [u8; 4],
    alarm: [u8; 4],

    is_latched: bool,
    is_stopped: bool,

    /// Accumulator for fractional cycle counting (fixed-point 25.7).
    cycles: u64,
    /// Divider period — set from CPU frequency / power-line frequency.
    period: u64,

    /// 3-bit ring counter for 50/60 Hz → 10 Hz divider.
    tick_counter: u8,
}

impl Tod {
    pub fn new() -> Self {
        Self {
            clock: [0, 0, 0, 1], // hours = 1
            latch: [0, 0, 0, 1],
            alarm: [0; 4],
            is_latched: false,
            is_stopped: true,
            cycles: 0,
            period: u64::MAX, // dummy until configured
            tick_counter: 0,
        }
    }

    pub fn reset(&mut self) {
        self.cycles = 0;
        self.tick_counter = 0;
        self.clock = [0, 0, 0, 1];
        self.latch = self.clock;
        self.alarm = [0; 4];
        self.is_latched = false;
        self.is_stopped = true;
    }

    pub fn set_period(&mut self, rate: u32) {
        self.period = (rate as u64) << 7; // fixed-point ×128
    }

    /// Read a TOD register (0 = tenths … 3 = hours).
    pub fn read(&mut self, reg: u8) -> u8 {
        let r = reg as usize;
        if !self.is_latched {
            self.latch = self.clock;
        }
        if r == TENTHS {
            self.is_latched = false;
        } else if r == HOURS {
            self.is_latched = true;
        }
        self.latch[r]
    }

    /// Write a TOD register.  `cra` and `crb` are the CIA control registers
    /// (CRB bit 7 selects alarm vs clock write).
    pub fn write(&mut self, reg: u8, mut data: u8, _cra: u8, crb: u8) {
        let r = reg as usize;
        match r {
            TENTHS  => data &= 0x0F,
            SECONDS | MINUTES => data &= 0x7F,
            HOURS => {
                data &= 0x9F;
                // Flip AM/PM at hour 12 when writing time
                if (data & 0x1F) == 0x12 && (crb & 0x80) == 0 {
                    data ^= 0x80;
                }
            }
            _ => {}
        }

        if crb & 0x80 != 0 {
            // set alarm
            self.alarm[r] = data;
        } else {
            // set time
            if r == TENTHS {
                if self.is_stopped {
                    self.tick_counter = 0;
                    self.is_stopped = false;
                }
            } else if r == HOURS {
                self.is_stopped = true;
            }
            self.clock[r] = data;
        }

        self.check_alarm();
    }

    /// Advance one PHI2 cycle.  Returns `true` if the alarm matched.
    pub fn tick(&mut self, cra: u8) -> bool {
        self.cycles += self.period;
        // The cycle accumulator overflows once per power-line tick.
        if self.cycles < (1 << 7) {
            return false;
        }
        let ticks = self.cycles >> 7;
        self.cycles &= 0x7F;

        let mut alarm = false;
        for _ in 0..ticks {
            if !self.is_stopped {
                // 3-bit ring counter: 000→001→011→111→110→100→(match)
                if self.tick_counter == (0x1 | ((cra as u8 & 0x80) >> 6)) {
                    self.tick_counter = 0;
                    self.update_counters();
                    if self.check_alarm() {
                        alarm = true;
                    }
                } else {
                    self.tick_counter = (self.tick_counter >> 1)
                        | ((!self.tick_counter << 2) & 0x4);
                }
            }
        }
        alarm
    }

    fn update_counters(&mut self) {
        let mut ts = self.clock[TENTHS] & 0x0F;
        let mut sl = self.clock[SECONDS] & 0x0F;
        let mut sh = (self.clock[SECONDS] >> 4) & 0x07;
        let mut ml = self.clock[MINUTES] & 0x0F;
        let mut mh = (self.clock[MINUTES] >> 4) & 0x07;
        let mut hl = self.clock[HOURS] & 0x0F;
        let mut hh = (self.clock[HOURS] >> 4) & 0x01;
        let mut pm = self.clock[HOURS] & 0x80;

        ts = (ts + 1) & 0x0F;
        if ts == 10 {
            ts = 0;
            sl = (sl + 1) & 0x0F;
            if sl == 10 {
                sl = 0;
                sh = (sh + 1) & 0x07;
                if sh == 6 {
                    sh = 0;
                    ml = (ml + 1) & 0x0F;
                    if ml == 10 {
                        ml = 0;
                        mh = (mh + 1) & 0x07;
                        if mh == 6 {
                            mh = 0;
                            if (hl == 2 && hh == 1) || (hl == 9 && hh == 0) {
                                hl = hh;
                                hh ^= 1;
                            } else {
                                hl = (hl + 1) & 0x0F;
                                if hl == 2 && hh == 1 {
                                    pm ^= 0x80;
                                }
                            }
                        }
                    }
                }
            }
        }

        self.clock[TENTHS]  = ts;
        self.clock[SECONDS] = sl | (sh << 4);
        self.clock[MINUTES] = ml | (mh << 4);
        self.clock[HOURS]   = hl | (hh << 4) | pm;
    }

    fn check_alarm(&self) -> bool {
        self.alarm == self.clock
    }
}

impl Default for Tod {
    fn default() -> Self { Self::new() }
}

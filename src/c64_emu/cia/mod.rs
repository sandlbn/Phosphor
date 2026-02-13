//! MOS 6526 / 8521 CIA emulation.
//!
//! Ported from libsidplayfp.  The CIA contains:
//! - Two 16-bit interval timers (A & B)
//! - Time-of-Day clock (BCD, 1/10 s resolution)
//! - 8-bit serial shift register
//! - Interrupt control logic (old 6526: 1-cycle delayed; new 8521: immediate)
//! - Two 8-bit I/O ports (directly memory-mapped at the 16 registers)

pub mod interrupt;
pub mod timer;
pub mod tod;

use interrupt::{CiaModel, InterruptSource};
use timer::Timer;
use tod::Tod;

// ── Register offsets (low 4 bits of address) ──────────────────

pub const PRA: u8 = 0;
pub const PRB: u8 = 1;
pub const DDRA: u8 = 2;
pub const DDRB: u8 = 3;
pub const TAL: u8 = 4;
pub const TAH: u8 = 5;
pub const TBL: u8 = 6;
pub const TBH: u8 = 7;
pub const TOD_TEN: u8 = 8;
pub const TOD_SEC: u8 = 9;
pub const TOD_MIN: u8 = 10;
pub const TOD_HR: u8 = 11;
pub const SDR: u8 = 12;
pub const ICR: u8 = 13;
pub const CRA: u8 = 14;
pub const CRB: u8 = 15;

// ── Interrupt flag bits ───────────────────────────────────────

pub const INT_UNDERFLOW_A: u8 = 1 << 0;
pub const INT_UNDERFLOW_B: u8 = 1 << 1;
pub const INT_ALARM: u8 = 1 << 2;
pub const INT_SP: u8 = 1 << 3;
pub const INT_FLAG: u8 = 1 << 4;
pub const INT_REQUEST: u8 = 1 << 7;

// ── MOS652X ───────────────────────────────────────────────────

/// Complete CIA chip.
pub struct Mos652x {
    pub regs: [u8; 16],

    pub timer_a: Timer,
    pub timer_b: Timer,
    pub tod: Tod,
    pub interrupt: InterruptSource,

    /// Ticks elapsed (caller must feed this).
    pub clock: u64,
}

impl Mos652x {
    pub fn new(model: CiaModel) -> Self {
        let mut cia = Self {
            regs: [0; 16],
            timer_a: Timer::new(),
            timer_b: Timer::new(),
            tod: Tod::new(),
            interrupt: InterruptSource::new(model),
            clock: 0,
        };
        cia.reset();
        cia
    }

    pub fn set_model(&mut self, model: CiaModel) {
        self.interrupt = InterruptSource::new(model);
    }

    pub fn reset(&mut self) {
        self.regs.fill(0);
        self.timer_a.reset();
        self.timer_b.reset();
        self.tod.reset();
        self.interrupt.reset();
    }

    /// Read a CIA register.  Returns the byte value and an optional
    /// interrupt-state delta (Some(true) = assert, Some(false) = deassert).
    pub fn read(&mut self, addr: u8) -> (u8, Option<bool>) {
        let addr = addr & 0x0F;
        let mut irq_delta = None;

        let val = match addr {
            PRA => self.regs[PRA as usize] | !self.regs[DDRA as usize],
            PRB => {
                let mut data = self.regs[PRB as usize] | !self.regs[DDRB as usize];
                data = self.adjust_data_port(data);
                data
            }
            TAL => (self.timer_a.counter & 0xFF) as u8,
            TAH => (self.timer_a.counter >> 8) as u8,
            TBL => (self.timer_b.counter & 0xFF) as u8,
            TBH => (self.timer_b.counter >> 8) as u8,
            TOD_TEN..=TOD_HR => self.tod.read(addr - TOD_TEN),
            ICR => {
                let old = self.interrupt.clear();
                irq_delta = Some(false); // reading ICR clears the IRQ line
                old
            }
            CRA => (self.regs[CRA as usize] & 0xEE) | (self.timer_a.started() as u8),
            CRB => (self.regs[CRB as usize] & 0xEE) | (self.timer_b.started() as u8),
            _ => self.regs[addr as usize],
        };
        (val, irq_delta)
    }

    /// Write a CIA register.  Returns an optional interrupt-state delta.
    pub fn write(&mut self, addr: u8, data: u8) -> Option<bool> {
        let addr = addr & 0x0F;
        let old = self.regs[addr as usize];
        self.regs[addr as usize] = data;
        let mut irq_delta = None;

        match addr {
            PRA | DDRA => { /* portA callback handled by caller */ }
            PRB | DDRB => { /* portB callback handled by caller */ }
            TAL => self.timer_a.latch_lo(data),
            TAH => self.timer_a.latch_hi(data),
            TBL => self.timer_b.latch_lo(data),
            TBH => self.timer_b.latch_hi(data),
            TOD_TEN..=TOD_HR => {
                self.tod.write(
                    addr - TOD_TEN,
                    data,
                    self.regs[CRA as usize],
                    self.regs[CRB as usize],
                );
            }
            SDR => { /* serial port start — simplified */ }
            ICR => {
                irq_delta = self.interrupt.set_mask(data);
            }
            CRA => {
                if (data & 1) != 0 && (old & 1) == 0 {
                    self.timer_a.pb_toggle = true;
                }
                self.timer_a.set_control(data);
            }
            CRB => {
                if (data & 1) != 0 && (old & 1) == 0 {
                    self.timer_b.pb_toggle = true;
                }
                // Bit 6 of CRB selects timer-B input (PHI2 vs timer-A underflow).
                self.timer_b.set_control(data | ((data & 0x40) >> 1));
            }
            _ => {}
        }

        irq_delta
    }

    /// Advance the CIA by one PHI2 cycle.  Returns interrupt state changes:
    /// `Some(true)` = IRQ asserted, `Some(false)` = IRQ deasserted, `None` = no change.
    pub fn tick(&mut self) -> Option<bool> {
        self.clock += 1;

        // --- Timer A ---
        let ua = self.timer_a.tick_phi2();
        if ua {
            // Timer A underflow → trigger interrupt
            let irq = self.interrupt.trigger(INT_UNDERFLOW_A);

            // If Timer B counts Timer A underflows (CRB bits 6,0 = 1,1)
            if (self.regs[CRB as usize] & 0x41) == 0x41 && self.timer_b.started() {
                self.timer_b.cascade_step();
            }

            // Serial port handling (simplified)
            if self.regs[CRA as usize] & 0x40 != 0 {
                // output mode — count underflows for shift register
            }

            if irq {
                return Some(true);
            }
        }

        // --- Timer B ---
        let ub = self.timer_b.tick_phi2();
        if ub {
            let irq = self.interrupt.trigger(INT_UNDERFLOW_B);
            if irq {
                return Some(true);
            }
        }

        // --- TOD ---
        let alarm = self.tod.tick(self.regs[CRA as usize]);
        if alarm {
            let irq = self.interrupt.trigger(INT_ALARM);
            if irq {
                return Some(true);
            }
        }

        None
    }

    fn adjust_data_port(&self, mut data: u8) -> u8 {
        if self.regs[CRA as usize] & 0x02 != 0 {
            data &= 0xBF;
            if self.timer_a.get_pb(self.regs[CRA as usize]) {
                data |= 0x40;
            }
        }
        if self.regs[CRB as usize] & 0x02 != 0 {
            data &= 0x7F;
            if self.timer_b.get_pb(self.regs[CRB as usize]) {
                data |= 0x80;
            }
        }
        data
    }

    // ── Convenience for C64 wiring ────────────────────────────

    pub fn set_day_of_time_rate(&mut self, rate: u32) {
        self.tod.set_period(rate);
    }
}

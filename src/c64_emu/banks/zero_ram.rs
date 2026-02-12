//! Zero-page RAM bank with CPU port ($00 / $01) emulation.
//!
//! Addresses $00 (data direction) and $01 (data port) control the PLA
//! banking lines LORAM / HIRAM / CHAREN.  Bits 6 & 7 of the data port
//! are unused on the 6510 and exhibit a capacitor-like fall-off from 1→0.

use super::bank::Bank;
use super::super::event::EventClock;

// ── Data-bit fall-off emulation ───────────────────────────────

/// Fall-off time in PHI2 cycles for a 6510 (~350 ms at ~1 MHz).
const FALL_OFF_CYCLES: EventClock = 350_000;

struct DataBit {
    data_set_clk: EventClock,
    is_falling_off: bool,
    data_set: u8,
    bit_mask: u8,
}

impl DataBit {
    fn new(bit: u8) -> Self {
        Self {
            data_set_clk: 0,
            is_falling_off: false,
            data_set: 0,
            bit_mask: 1 << bit,
        }
    }
    fn reset(&mut self) {
        self.is_falling_off = false;
        self.data_set = 0;
    }
    fn read(&mut self, phi2_time: EventClock) -> u8 {
        if self.is_falling_off && self.data_set_clk < phi2_time {
            self.reset();
        }
        self.data_set
    }
    fn write(&mut self, phi2_time: EventClock, value: u8) {
        self.data_set_clk = phi2_time + FALL_OFF_CYCLES;
        self.data_set = value & self.bit_mask;
        self.is_falling_off = true;
    }
}

// ── ZeroRamBank ───────────────────────────────────────────────

/// Callback so the bank can tell the MMU about port changes.
pub type CpuPortCallback = Box<dyn FnMut(u8)>;

pub struct ZeroRamBank {
    /// Direction register ($00).
    dir: u8,
    /// Data register ($01).
    data: u8,
    /// Computed value that reads back from $01.
    data_read: u8,
    /// Current state of the port pins.
    proc_port_pins: u8,

    bit6: DataBit,
    bit7: DataBit,

    /// Closure invoked when the effective CPU-port value changes.
    /// Receives the 3-bit PLA state (LORAM | HIRAM | CHAREN).
    on_port_change: Option<CpuPortCallback>,

    /// Getter for PHI2 time (provided by the C64 / scheduler).
    phi2_time_fn: Option<Box<dyn Fn() -> EventClock>>,

    /// Pseudo-random "last byte on VIC bus" for disconnected reads.
    last_read_byte_fn: Option<Box<dyn Fn() -> u8>>,
}

impl ZeroRamBank {
    pub fn new() -> Self {
        Self {
            dir: 0,
            data: 0x3F,
            data_read: 0x3F,
            proc_port_pins: 0x3F,
            bit6: DataBit::new(6),
            bit7: DataBit::new(7),
            on_port_change: None,
            phi2_time_fn: None,
            last_read_byte_fn: None,
        }
    }

    /// Wire up the callback that feeds PLA state to the MMU.
    pub fn set_port_callback(&mut self, cb: CpuPortCallback) {
        self.on_port_change = Some(cb);
    }

    pub fn set_phi2_time_fn(&mut self, f: Box<dyn Fn() -> EventClock>) {
        self.phi2_time_fn = Some(f);
    }

    pub fn set_last_read_byte_fn(&mut self, f: Box<dyn Fn() -> u8>) {
        self.last_read_byte_fn = Some(f);
    }

    pub fn reset(&mut self) {
        self.bit6.reset();
        self.bit7.reset();
        self.dir = 0;
        self.data = 0x3F;
        self.data_read = 0x3F;
        self.proc_port_pins = 0x3F;
        self.update_cpu_port();
    }

    fn phi2_time(&self) -> EventClock {
        self.phi2_time_fn.as_ref().map_or(0, |f| f())
    }

    #[allow(dead_code)]
    fn last_read_byte(&self) -> u8 {
        self.last_read_byte_fn.as_ref().map_or(0xFF, |f| f())
    }

    fn update_cpu_port(&mut self) {
        self.proc_port_pins = (self.proc_port_pins & !self.dir) | (self.data & self.dir);
        self.data_read = (self.data | !self.dir) & (self.proc_port_pins | 0x17);

        let pla_state = (self.data | !self.dir) & 0x07;

        if (self.dir & 0x20) == 0 {
            self.data_read &= !0x20;
        }

        if let Some(ref mut cb) = self.on_port_change {
            cb(pla_state);
        }
    }
}

impl Default for ZeroRamBank {
    fn default() -> Self { Self::new() }
}

impl Bank for ZeroRamBank {
    fn peek(&self, _address: u16) -> u8 {
        // immutable peek — used for read-only contexts
        // For $00/$01 we return the cached value.
        0
    }

    fn peek_mut(&mut self, address: u16) -> u8 {
        match address {
            0 => self.dir,
            1 => {
                let mut retval = self.data_read;
                let t = self.phi2_time();
                if (self.dir & 0x40) == 0 {
                    retval &= !0x40;
                    retval |= self.bit6.read(t);
                }
                if (self.dir & 0x80) == 0 {
                    retval &= !0x80;
                    retval |= self.bit7.read(t);
                }
                retval
            }
            _ => 0, // actual RAM read handled by the MMU layer
        }
    }

    fn poke(&mut self, address: u16, value: u8) {
        match address {
            0 => {
                if self.dir != value {
                    let t = self.phi2_time();
                    if (self.dir & 0x40) != 0 && (value & 0x40) == 0 {
                        self.bit6.write(t, self.data);
                    }
                    if (self.dir & 0x80) != 0 && (value & 0x80) == 0 {
                        self.bit7.write(t, self.data);
                    }
                    self.dir = value;
                    self.update_cpu_port();
                }
            }
            1 => {
                let t = self.phi2_time();
                if self.dir & 0x40 != 0 {
                    self.bit6.write(t, value);
                }
                if self.dir & 0x80 != 0 {
                    self.bit7.write(t, value);
                }
                if self.data != value {
                    self.data = value;
                    self.update_cpu_port();
                }
            }
            _ => { /* RAM write handled by MMU layer */ }
        }
    }
}

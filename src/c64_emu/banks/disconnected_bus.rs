//! Disconnected bus bank ($DE00–$DFFF).
//!
//! When no expansion cartridge is connected these areas float and
//! return the last byte that was on the VIC data bus.

use super::bank::Bank;

pub struct DisconnectedBusBank {
    /// Closure returning the "last read byte" (pseudo-random in our
    /// implementation, same as libsidplayfp).
    last_read_byte_fn: Option<Box<dyn Fn() -> u8>>,
}

impl DisconnectedBusBank {
    pub fn new() -> Self {
        Self { last_read_byte_fn: None }
    }

    pub fn set_last_read_byte_fn(&mut self, f: Box<dyn Fn() -> u8>) {
        self.last_read_byte_fn = Some(f);
    }
}

impl Default for DisconnectedBusBank {
    fn default() -> Self { Self::new() }
}

impl Bank for DisconnectedBusBank {
    fn poke(&mut self, _address: u16, _value: u8) { /* no device */ }
    fn peek(&self, _address: u16) -> u8 {
        self.last_read_byte_fn.as_ref().map_or(0xFF, |f| f())
    }
}

//! Color RAM — 1 K × 4-bit SRAM ($D800–$DBFF).

use super::bank::Bank;

pub struct ColorRamBank {
    ram: [u8; 0x400],
}

impl ColorRamBank {
    pub fn new() -> Self { Self { ram: [0; 0x400] } }
    pub fn reset(&mut self) { self.ram.fill(0); }
}

impl Default for ColorRamBank {
    fn default() -> Self { Self::new() }
}

impl Bank for ColorRamBank {
    fn poke(&mut self, address: u16, value: u8) {
        self.ram[(address & 0x3FF) as usize] = value & 0x0F;
    }
    fn peek(&self, address: u16) -> u8 {
        self.ram[(address & 0x3FF) as usize]
    }
}

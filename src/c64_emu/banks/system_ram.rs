//! 64 KB system RAM with the classic C64 power-on pattern.

use super::bank::Bank;

pub struct SystemRamBank {
    pub ram: [u8; 0x1_0000],
}

impl SystemRamBank {
    pub fn new() -> Self {
        let mut bank = Self { ram: [0; 0x1_0000] };
        bank.reset();
        bank
    }

    /// Initialize RAM with the classic C64 power-up pattern:
    /// ```text
    /// $0000: 00 00 ff ff ff ff 00 00  00 00 ff ff ff ff 00 00
    /// $4000: ff ff 00 00 00 00 ff ff  ff ff 00 00 00 00 ff ff
    /// $8000: (same as $0000)
    /// $C000: (same as $4000)
    /// ```
    pub fn reset(&mut self) {
        let mut byte: u8 = 0x00;
        for j in (0..0x1_0000usize).step_by(0x4000) {
            self.ram[j..j + 0x4000].fill(byte);
            byte = !byte;
            for i in (0x02..0x4000usize).step_by(0x08) {
                let start = j + i;
                let end = (start + 4).min(j + 0x4000);
                self.ram[start..end].fill(byte);
            }
        }
    }
}

impl Default for SystemRamBank {
    fn default() -> Self { Self::new() }
}

impl Bank for SystemRamBank {
    fn poke(&mut self, address: u16, value: u8) {
        self.ram[address as usize] = value;
    }
    fn peek(&self, address: u16) -> u8 {
        self.ram[address as usize]
    }
}

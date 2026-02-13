//! Primary SID bank ($D400–$D7FF, mirrored every 32 bytes).

use super::bank::Bank;

/// Trait that an external SID emulation must implement.
pub trait SidChip {
    fn reset(&mut self, volume: u8);
    fn read(&self, reg: u8) -> u8;
    fn write(&mut self, reg: u8, data: u8);
}

/// Null SID — placeholder returning 0xFF on reads.
pub struct NullSid;

impl SidChip for NullSid {
    fn reset(&mut self, _volume: u8) {}
    fn read(&self, _reg: u8) -> u8 {
        0xFF
    }
    fn write(&mut self, _reg: u8, _data: u8) {}
}

pub struct SidBank {
    sid: Box<dyn SidChip>,
    last_poke: [u8; 0x20],
}

impl SidBank {
    pub fn new() -> Self {
        Self {
            sid: Box::new(NullSid),
            last_poke: [0; 0x20],
        }
    }

    pub fn set_sid(&mut self, s: Option<Box<dyn SidChip>>) {
        self.sid = s.unwrap_or_else(|| Box::new(NullSid));
    }

    pub fn reset(&mut self) {
        self.last_poke.fill(0);
        self.sid.reset(0x0F);
    }

    pub fn get_status(&self, out: &mut [u8; 0x20]) {
        out.copy_from_slice(&self.last_poke);
    }
}

impl Default for SidBank {
    fn default() -> Self {
        Self::new()
    }
}

impl Bank for SidBank {
    fn poke(&mut self, address: u16, value: u8) {
        let reg = (address & 0x1F) as usize;
        self.last_poke[reg] = value;
        self.sid.write(reg as u8, value);
    }
    fn peek(&self, address: u16) -> u8 {
        self.sid.read((address & 0x1F) as u8)
    }
}

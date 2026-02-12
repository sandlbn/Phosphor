//! Extra SID bank — supports mapping up to 8 additional SID chips
//! within a 256-byte I/O page (each SID occupies 32 bytes).

use super::sid_bank::SidChip;

const MAPPER_SIZE: usize = 8;

pub struct ExtraSidBank {
    /// For each 32-byte slot, either an extra SID or a fallback that
    /// just returns 0xFF.
    sids: Vec<Box<dyn SidChip>>,
    /// Maps 32-byte slot index → SID index (or `None` for fallback).
    mapper: [Option<usize>; MAPPER_SIZE],
}

impl ExtraSidBank {
    pub fn new() -> Self {
        Self {
            sids: Vec::new(),
            mapper: [None; MAPPER_SIZE],
        }
    }

    fn mapper_index(address: u16) -> usize {
        ((address >> 5) as usize) & (MAPPER_SIZE - 1)
    }

    pub fn reset(&mut self) {
        for sid in &mut self.sids {
            sid.reset(0x0F);
        }
    }

    /// Add a SID chip mapped at the given base address (e.g. $D420).
    pub fn add_sid(&mut self, sid: Box<dyn SidChip>, address: u16) {
        let idx = self.sids.len();
        self.sids.push(sid);
        self.mapper[Self::mapper_index(address)] = Some(idx);
    }

    pub fn peek(&self, addr: u16) -> u8 {
        let slot = Self::mapper_index(addr);
        match self.mapper[slot] {
            Some(i) => self.sids[i].read((addr & 0x1F) as u8),
            None => 0xFF,
        }
    }

    pub fn poke(&mut self, addr: u16, data: u8) {
        let slot = Self::mapper_index(addr);
        if let Some(i) = self.mapper[slot] {
            self.sids[i].write((addr & 0x1F) as u8, data);
        }
    }

    pub fn installed_sids(&self) -> usize {
        self.sids.len()
    }
}

impl Default for ExtraSidBank {
    fn default() -> Self {
        Self::new()
    }
}

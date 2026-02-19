//! Extra SID bank — supports mapping up to 8 additional SID chips
//! anywhere in the I/O space ($D000–$DFFF, 128 × 32-byte slots).

use super::sid_bank::SidChip;

/// Number of 32-byte slots in the 4 KB I/O space ($D000–$DFFF).
const MAPPER_SIZE: usize = 128;

pub struct ExtraSidBank {
    /// Registered extra SID chips.
    sids: Vec<Box<dyn SidChip>>,
    /// For each 32-byte slot in $D000–$DFFF, which SID handles it (if any).
    /// Slot index = ((address >> 5) & 0x7F), covering $D000–$DFFF.
    mapper: [Option<usize>; MAPPER_SIZE],
}

impl ExtraSidBank {
    pub fn new() -> Self {
        Self {
            sids: Vec::new(),
            mapper: [None; MAPPER_SIZE],
        }
    }

    /// Convert a full 16-bit address to its 32-byte slot index (0–127).
    /// Valid for $D000–$DFFF only.
    fn slot(address: u16) -> usize {
        ((address >> 5) as usize) & (MAPPER_SIZE - 1)
    }

    pub fn reset(&mut self) {
        for sid in &mut self.sids {
            sid.reset(0x0F);
        }
    }

    /// Add a SID chip mapped at `base_address` (e.g. $D420, $D500, $DE00).
    /// The chip occupies the 32-byte slot containing that address.
    pub fn add_sid(&mut self, sid: Box<dyn SidChip>, base_address: u16) {
        let idx = self.sids.len();
        self.sids.push(sid);
        self.mapper[Self::slot(base_address)] = Some(idx);
    }

    /// Returns true if a chip is mapped at the 32-byte slot for this address.
    pub fn has_slot(&self, address: u16) -> bool {
        self.mapper[Self::slot(address)].is_some()
    }

    pub fn peek(&self, addr: u16) -> u8 {
        match self.mapper[Self::slot(addr)] {
            Some(i) => self.sids[i].read((addr & 0x1F) as u8),
            None => 0xFF,
        }
    }

    pub fn poke(&mut self, addr: u16, data: u8) {
        if let Some(i) = self.mapper[Self::slot(addr)] {
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

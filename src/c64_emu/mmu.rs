//! C64 MMU / PLA — maps the CPU's 64 KB address space to RAM, ROM, or I/O
//! depending on the processor-port bits ($01) and the address.
//!
//! The mapping is split into 16 × 4 KB pages.  Pages 0–9 and B are always
//! RAM.  Pages A–B, D, and E–F switch between RAM, ROM, and I/O based on
//! the LORAM / HIRAM / CHAREN signals from the CPU port.

/// Which bank is currently selected for a given 4 KB page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMapping {
    Ram,
    BasicRom,
    KernalRom,
    CharacterRom,
    Io,
}

pub struct Mmu {
    loram: bool,
    hiram: bool,
    charen: bool,

    /// Read mapping for each 4 KB page.
    pub read_map: [PageMapping; 16],
    /// Write mapping (always RAM except page $D which goes to I/O when mapped).
    pub write_map: [PageMapping; 16],

    /// Pseudo-random seed for "last VIC bus byte" emulation.
    seed: u32,
}

impl Mmu {
    pub fn new() -> Self {
        let mut mmu = Self {
            loram: false,
            hiram: false,
            charen: false,
            read_map: [PageMapping::Ram; 16],
            write_map: [PageMapping::Ram; 16],
            seed: 3_686_734,
        };
        mmu.update_mapping();
        mmu
    }

    pub fn reset(&mut self) {
        self.loram = false;
        self.hiram = false;
        self.charen = false;
        self.update_mapping();
    }

    /// Called by the zero-page bank when $01 changes.
    pub fn set_cpu_port(&mut self, state: u8) {
        self.loram  = (state & 1) != 0;
        self.hiram  = (state & 2) != 0;
        self.charen = (state & 4) != 0;
        self.update_mapping();
    }

    fn update_mapping(&mut self) {
        // Default everything to RAM.
        self.read_map.fill(PageMapping::Ram);
        self.write_map.fill(PageMapping::Ram);

        // $E000-$FFFF: Kernal ROM when HIRAM is set.
        if self.hiram {
            self.read_map[0xE] = PageMapping::KernalRom;
            self.read_map[0xF] = PageMapping::KernalRom;
        }

        // $A000-$BFFF: BASIC ROM when both LORAM and HIRAM are set.
        if self.loram && self.hiram {
            self.read_map[0xA] = PageMapping::BasicRom;
            self.read_map[0xB] = PageMapping::BasicRom;
        }

        // $D000-$DFFF: depends on CHAREN + LORAM/HIRAM.
        if self.charen && (self.loram || self.hiram) {
            self.read_map[0xD] = PageMapping::Io;
            self.write_map[0xD] = PageMapping::Io;
        } else if !self.charen && (self.loram || self.hiram) {
            self.read_map[0xD] = PageMapping::CharacterRom;
            // writes still go to RAM
        }
        // else: both read and write go to RAM (already set)
    }

    /// Pseudo-random "last byte on VIC bus" (same LCG as libsidplayfp).
    pub fn last_read_byte(&mut self) -> u8 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.seed >> 16) as u8
    }
}

impl Default for Mmu {
    fn default() -> Self { Self::new() }
}

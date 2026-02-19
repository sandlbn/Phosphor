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
    /// EXROM line state: true = high = no cartridge ROM at $8000.
    exrom: bool,
    /// GAME line state: true = high = no cartridge ROM at $A000; false+exrom=true = Ultimax mode.
    game: bool,

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
            exrom: true,
            game: true,
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
        self.exrom = true;
        self.game = true;
        self.update_mapping();
    }

    /// Set the EXROM and GAME cartridge port lines.
    /// Both true = no cartridge (default).
    /// exrom=false, game=true = Ultimax mode: $D000-$DFFF always I/O,
    /// $E000-$FFFF always Kernal, regardless of CPU port bits.
    pub fn set_exrom_game(&mut self, exrom: bool, game: bool) {
        self.exrom = exrom;
        self.game = game;
        self.update_mapping();
    }

    /// Called by the zero-page bank when $01 changes.
    pub fn set_cpu_port(&mut self, state: u8) {
        self.loram = (state & 1) != 0;
        self.hiram = (state & 2) != 0;
        self.charen = (state & 4) != 0;
        self.update_mapping();
    }

    fn update_mapping(&mut self) {
        // Default everything to RAM.
        self.read_map.fill(PageMapping::Ram);
        self.write_map.fill(PageMapping::Ram);

        // Ultimax mode (EXROM low, GAME high): $D000-$DFFF always I/O,
        // $E000-$FFFF always Kernal ROM. CPU port bits are ignored for these.
        if !self.exrom && self.game {
            self.read_map[0xD] = PageMapping::Io;
            self.write_map[0xD] = PageMapping::Io;
            self.read_map[0xE] = PageMapping::KernalRom;
            self.read_map[0xF] = PageMapping::KernalRom;
            return;
        }

        // Normal mode: CPU port bits select banks.

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
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.seed >> 16) as u8
    }
}

impl Default for Mmu {
    fn default() -> Self {
        Self::new()
    }
}

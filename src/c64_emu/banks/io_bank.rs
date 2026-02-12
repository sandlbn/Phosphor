//! I/O region router ($D000–$DFFF).
//!
//! The 4 KB I/O area is divided into 16 × 256-byte pages, each mapped
//! to a different chip (VIC-II, SID, Color RAM, CIA1, CIA2, IO1, IO2).

/// Index type for the 16 pages.
pub type PageIndex = usize; // 0..16

/// A thin dispatch layer.  The actual `Bank` implementations live
/// elsewhere; `IoBank` just holds indices/references.
///
/// Because Rust's ownership rules make storing 16 `&mut dyn Bank`
/// references tricky, we use an indirection: the C64 struct owns
/// all the chips, and `IoBank` stores *indices* (page → chip id).
/// The C64 `cpuRead` / `cpuWrite` path resolves the index.
///
/// Alternatively, the caller can use `IoBank::dispatch` directly.
pub struct IoBank {
    /// For each of the 16 pages, which chip handles it.
    /// Encoded as a user-defined discriminant.
    map: [IoChip; 16],
}

/// Which chip owns a given $Dxxx page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoChip {
    Vic,
    Sid,
    ColorRam,
    Cia1,
    Cia2,
    DisconnectedBus,
    ExtraSid(u8), // extra-SID bank id
}

impl IoBank {
    pub fn new() -> Self {
        Self {
            map: [IoChip::DisconnectedBus; 16],
        }
    }

    pub fn set_bank(&mut self, page: PageIndex, chip: IoChip) {
        assert!(page < 16);
        self.map[page] = chip;
    }

    pub fn get_bank(&self, page: PageIndex) -> IoChip {
        self.map[page]
    }

    /// Given a full 16-bit address in $D000-$DFFF, return the chip
    /// that should handle it.
    pub fn dispatch(&self, addr: u16) -> IoChip {
        let page = ((addr >> 8) & 0x0F) as usize;
        self.map[page]
    }

    /// Standard C64 I/O mapping.
    pub fn reset_default(&mut self) {
        // $D000-$D3FF  VIC-II
        for i in 0x0..=0x3 { self.map[i] = IoChip::Vic; }
        // $D400-$D7FF  SID
        for i in 0x4..=0x7 { self.map[i] = IoChip::Sid; }
        // $D800-$DBFF  Color RAM
        for i in 0x8..=0xB { self.map[i] = IoChip::ColorRam; }
        // $DC00-$DCFF  CIA1
        self.map[0xC] = IoChip::Cia1;
        // $DD00-$DDFF  CIA2
        self.map[0xD] = IoChip::Cia2;
        // $DE00-$DFFF  IO1/IO2 (no device connected)
        self.map[0xE] = IoChip::DisconnectedBus;
        self.map[0xF] = IoChip::DisconnectedBus;
    }
}

impl Default for IoBank {
    fn default() -> Self {
        let mut b = Self::new();
        b.reset_default();
        b
    }
}

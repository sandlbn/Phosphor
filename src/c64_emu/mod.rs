//! Commodore 64 emulation core (embedded from c64_emu crate).
//!
//! Ported from libsidplayfp (C++).
//! CPU is delegated to the `mos6502` crate; everything else —
//! VIC-II, CIA ×2, memory banks, PLA/MMU — lives here.

pub mod event;
pub mod banks;
pub mod cia;
pub mod vic_ii;
pub mod mmu;
pub mod c64;

//! System ROM banks: Kernal, BASIC, Character ROM.
//!
//! Writes to ROM are silently ignored.  When no ROM image is loaded a
//! minimal stub is installed so the emulator can boot far enough to run
//! SID tunes.

use super::bank::Bank;

/// 6502 opcodes used in the stub ROM.
mod opc {
    pub const RTS: u8 = 0x60;
    pub const RTI: u8 = 0x40;
    pub const JMP_ABS: u8 = 0x4C;
    pub const JMP_IND: u8 = 0x6C;
    pub const NOP_ABS: u8 = 0x0C; // unofficial – 3-byte NOP
    pub const PHA: u8 = 0x48;
    pub const PLA: u8 = 0x68;
    pub const TXA: u8 = 0x8A;
    pub const TAX: u8 = 0xAA;
    pub const TYA: u8 = 0x98;
    pub const TAY: u8 = 0xA8;
    pub const SEI: u8 = 0x78;
    pub const LDA_IMM: u8 = 0xA9;
    pub const STA_ABS: u8 = 0x8D;
    pub const JSR: u8 = 0x20;
}

// ── Generic ROM helper ────────────────────────────────────────

fn mask(size: usize, addr: u16) -> usize {
    (addr as usize) & (size - 1)
}

// ── Kernal ROM ($E000-$FFFF, 8 KB) ───────────────────────────

pub struct KernalRomBank {
    rom: [u8; 0x2000],
    reset_vector: [u8; 2], // backup of $FFFC/$FFFD
}

impl KernalRomBank {
    pub fn new() -> Self {
        let mut k = Self {
            rom: [opc::RTS; 0x2000],
            reset_vector: [0; 2],
        };
        k.install_stub();
        k
    }

    /// Load a real Kernal image.  `None` installs the minimal stub.
    pub fn set(&mut self, source: Option<&[u8]>) {
        if let Some(data) = source {
            let len = data.len().min(0x2000);
            self.rom[..len].copy_from_slice(&data[..len]);
        } else {
            self.rom.fill(opc::RTS);
            self.install_stub();
        }
        self.reset_vector[0] = self.rom[mask(0x2000, 0xFFFC)];
        self.reset_vector[1] = self.rom[mask(0x2000, 0xFFFD)];
    }

    pub fn reset(&mut self) {
        self.rom[mask(0x2000, 0xFFFC)] = self.reset_vector[0];
        self.rom[mask(0x2000, 0xFFFD)] = self.reset_vector[1];
    }

    /// Direct mutable access to the 8 KB ROM image.
    pub fn rom_mut(&mut self) -> &mut [u8; 0x2000] {
        &mut self.rom
    }

    /// Direct read access to the 8 KB ROM image.
    pub fn rom_ref(&self) -> &[u8; 0x2000] {
        &self.rom
    }

    pub fn install_reset_hook(&mut self, addr: u16) {
        self.rom[mask(0x2000, 0xFFFC)] = (addr & 0xFF) as u8;
        self.rom[mask(0x2000, 0xFFFD)] = (addr >> 8) as u8;
    }

    fn set_val(&mut self, addr: u16, val: u8) {
        self.rom[mask(0x2000, addr)] = val;
    }

    /// Install the minimal IRQ / NMI / RESET stubs (same layout as
    /// libsidplayfp so SID-tune player hooks work).
    fn install_stub(&mut self) {
        // IRQ routine at $EA31
        self.set_val(0xEA31, opc::JMP_ABS);
        self.set_val(0xEA32, 0x7E);
        self.set_val(0xEA33, 0xEA);

        self.set_val(0xEA7E, opc::NOP_ABS); // clear IRQ latch
        self.set_val(0xEA7F, 0x0D);
        self.set_val(0xEA80, 0xDC);
        self.set_val(0xEA81, opc::PLA);
        self.set_val(0xEA82, opc::TAY);
        self.set_val(0xEA83, opc::PLA);
        self.set_val(0xEA84, opc::TAX);
        self.set_val(0xEA85, opc::PLA);
        self.set_val(0xEA86, opc::RTI);

        // RESET entry
        self.set_val(0xFCE2, 0x02); // KIL / halt

        // NMI
        self.set_val(0xFE43, opc::SEI);
        self.set_val(0xFE44, opc::JMP_IND);
        self.set_val(0xFE45, 0x18);
        self.set_val(0xFE46, 0x03);
        self.set_val(0xFE47, opc::RTI);

        // IRQ entry
        self.set_val(0xFF48, opc::PHA);
        self.set_val(0xFF49, opc::TXA);
        self.set_val(0xFF4A, opc::PHA);
        self.set_val(0xFF4B, opc::TYA);
        self.set_val(0xFF4C, opc::PHA);
        self.set_val(0xFF4D, opc::JMP_IND);
        self.set_val(0xFF4E, 0x14);
        self.set_val(0xFF4F, 0x03);

        // Hardware vectors
        self.set_val(0xFFFA, 0x43); // NMI  → $FE43
        self.set_val(0xFFFB, 0xFE);
        self.set_val(0xFFFC, 0xE2); // RESET→ $FCE2
        self.set_val(0xFFFD, 0xFC);
        self.set_val(0xFFFE, 0x48); // IRQ  → $FF48
        self.set_val(0xFFFF, 0xFF);

        self.reset_vector[0] = self.rom[mask(0x2000, 0xFFFC)];
        self.reset_vector[1] = self.rom[mask(0x2000, 0xFFFD)];
    }
}

impl Default for KernalRomBank {
    fn default() -> Self {
        Self::new()
    }
}

impl Bank for KernalRomBank {
    fn poke(&mut self, _address: u16, _value: u8) { /* ROM: no-op */
    }
    fn peek(&self, address: u16) -> u8 {
        self.rom[mask(0x2000, address)]
    }
}

// ── BASIC ROM ($A000-$BFFF, 8 KB) ────────────────────────────

pub struct BasicRomBank {
    rom: [u8; 0x2000],
    trap_backup: [u8; 3],
    subtune_backup: [u8; 11],
}

impl BasicRomBank {
    pub fn new() -> Self {
        Self {
            rom: [opc::RTS; 0x2000],
            trap_backup: [0; 3],
            subtune_backup: [0; 11],
        }
    }

    pub fn set(&mut self, source: Option<&[u8]>) {
        if let Some(data) = source {
            let len = data.len().min(0x2000);
            self.rom[..len].copy_from_slice(&data[..len]);
        }
        // backup warm-start
        let off = mask(0x2000, 0xA7AE);
        self.trap_backup.copy_from_slice(&self.rom[off..off + 3]);
        let off2 = mask(0x2000, 0xBF53);
        self.subtune_backup
            .copy_from_slice(&self.rom[off2..off2 + 11]);
    }

    pub fn reset(&mut self) {
        let off = mask(0x2000, 0xA7AE);
        self.rom[off..off + 3].copy_from_slice(&self.trap_backup);
        let off2 = mask(0x2000, 0xBF53);
        self.rom[off2..off2 + 11].copy_from_slice(&self.subtune_backup);
    }

    pub fn install_trap(&mut self, addr: u16) {
        let off = mask(0x2000, 0xA7AE);
        self.rom[off] = opc::JMP_ABS;
        self.rom[off + 1] = (addr & 0xFF) as u8;
        self.rom[off + 2] = (addr >> 8) as u8;
    }

    pub fn set_subtune(&mut self, tune: u8) {
        let o = mask(0x2000, 0xBF53);
        self.rom[o] = opc::LDA_IMM;
        self.rom[o + 1] = tune;
        self.rom[o + 2] = opc::STA_ABS;
        self.rom[o + 3] = 0x0C;
        self.rom[o + 4] = 0x03;
        self.rom[o + 5] = opc::JSR;
        self.rom[o + 6] = 0x2C;
        self.rom[o + 7] = 0xA8;
        self.rom[o + 8] = opc::JMP_ABS;
        self.rom[o + 9] = 0xB1;
        self.rom[o + 10] = 0xA7;
    }
}

impl Default for BasicRomBank {
    fn default() -> Self {
        Self::new()
    }
}

impl Bank for BasicRomBank {
    fn poke(&mut self, _address: u16, _value: u8) { /* ROM: no-op */
    }
    fn peek(&self, address: u16) -> u8 {
        self.rom[mask(0x2000, address)]
    }
}

// ── Character ROM ($D000-$DFFF, 4 KB) ────────────────────────

pub struct CharacterRomBank {
    rom: [u8; 0x1000],
}

impl CharacterRomBank {
    pub fn new() -> Self {
        Self { rom: [0; 0x1000] }
    }

    pub fn set(&mut self, source: Option<&[u8]>) {
        if let Some(data) = source {
            let len = data.len().min(0x1000);
            self.rom[..len].copy_from_slice(&data[..len]);
        }
    }
}

impl Default for CharacterRomBank {
    fn default() -> Self {
        Self::new()
    }
}

impl Bank for CharacterRomBank {
    fn poke(&mut self, _address: u16, _value: u8) {}
    fn peek(&self, address: u16) -> u8 {
        self.rom[mask(0x1000, address)]
    }
}

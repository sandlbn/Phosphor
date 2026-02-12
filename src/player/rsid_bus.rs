// RSID bus: wraps the cycle-accurate c64_emu::C64 core and intercepts
// SID register writes so they can be forwarded to the USBSID hardware.
//
// Only used for RSID playback. PSID continues to use the simpler C64Memory.

use mos6502::memory::Bus;

use crate::c64_emu::c64::{C64, C64Model, C64CiaModel};
use crate::c64_emu::mmu::PageMapping;

use super::memory::{SidMapper, SidWrite, SID_REG_SIZE};

// ─────────────────────────────────────────────────────────────────────────────
//  Approximate 6502 cycle counts per opcode (same table as memory.rs)
// ─────────────────────────────────────────────────────────────────────────────

static OPCODE_CYCLES: [u8; 256] = [
    //0  1  2  3  4  5  6  7  8  9  A  B  C  D  E  F
    7, 6, 2, 8, 3, 3, 5, 5, 3, 2, 2, 2, 4, 4, 6, 6, // 0x
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // 1x
    6, 6, 2, 8, 3, 3, 5, 5, 4, 2, 2, 2, 4, 4, 6, 6, // 2x
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // 3x
    6, 6, 2, 8, 3, 3, 5, 5, 3, 2, 2, 2, 3, 4, 6, 6, // 4x
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // 5x
    6, 6, 2, 8, 3, 3, 5, 5, 4, 2, 2, 2, 5, 4, 6, 6, // 6x
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // 7x
    2, 6, 2, 6, 3, 3, 3, 3, 2, 2, 2, 2, 4, 4, 4, 4, // 8x
    2, 6, 2, 6, 4, 4, 4, 4, 2, 5, 2, 5, 5, 5, 5, 5, // 9x
    2, 6, 2, 6, 3, 3, 3, 3, 2, 2, 2, 2, 4, 4, 4, 4, // Ax
    2, 5, 2, 5, 4, 4, 4, 4, 2, 4, 2, 4, 4, 4, 4, 4, // Bx
    2, 6, 2, 8, 3, 3, 5, 5, 2, 2, 2, 2, 4, 4, 6, 6, // Cx
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // Dx
    2, 6, 2, 8, 3, 3, 5, 5, 2, 2, 2, 2, 4, 4, 6, 6, // Ex
    2, 5, 2, 8, 4, 4, 6, 6, 2, 4, 2, 7, 4, 4, 7, 7, // Fx
];

// ─────────────────────────────────────────────────────────────────────────────
//  RsidBus — wraps c64_emu::C64 and captures SID writes
// ─────────────────────────────────────────────────────────────────────────────

pub struct RsidBus {
    pub c64: C64,
    pub sid_writes: Vec<SidWrite>,
    pub frame_cycle: u32,
    pub sid_shadow: [u8; 128],
    mapper: SidMapper,
    mono: bool,
    osc3_seed: u32,
}

impl RsidBus {
    pub fn new(is_pal: bool, mapper: SidMapper, mono: bool) -> Self {
        let mut c64 = C64::new();
        c64.set_model(if is_pal { C64Model::PalB } else { C64Model::NtscM });
        c64.set_cia_model(C64CiaModel::Old);
        c64.reset();

        Self {
            c64,
            sid_writes: Vec::with_capacity(256),
            frame_cycle: 0,
            sid_shadow: [0u8; 128],
            mapper,
            mono,
            osc3_seed: 0x12345678,
        }
    }

    // ── Memory helpers ───────────────────────────────────────────────────

    /// Load payload into RAM.
    pub fn load(&mut self, addr: u16, data: &[u8]) {
        let a = addr as usize;
        let end = (a + data.len()).min(65536);
        self.c64.ram.ram[a..end].copy_from_slice(&data[..end - a]);
    }

    /// Build and install KERNAL stubs as a ROM image.
    /// If `tune_e000` is provided (tune data overlapping $E000-$FFFF),
    /// it gets overlaid first, then stubs are patched on top.
    pub fn install_kernal_stubs(&mut self) {
        let mut rom = [0x60u8; 8192]; // fill with RTS

        // Copy any tune data that was loaded into $E000-$FFFF from RAM
        rom.copy_from_slice(&self.c64.ram.ram[0xE000..0x10000]);

        // Now install our stubs on top
        Self::patch_kernal_stubs(&mut rom);

        self.c64.set_kernal(Some(&rom));
    }

    fn patch_kernal_stubs(rom: &mut [u8; 8192]) {
        // Helper: write to rom at a $E000-based absolute address
        macro_rules! s {
            ($addr:expr, $val:expr) => {
                rom[($addr as usize) - 0xE000] = $val;
            };
        }

        // RTS stubs for KERNAL entry points (same set as Phosphor)
        let rts_stubs: &[u16] = &[
            0xFF81, 0xFF84, 0xFF87, 0xFF8A, 0xFF8D, 0xFF90, 0xFF93, 0xFF96,
            0xFF99, 0xFF9C, 0xFFA5, 0xFFB1, 0xFFB4, 0xFFBD, 0xFFC0, 0xFFC3,
            0xFFC6, 0xFFC9, 0xFFCC, 0xFFCF, 0xFFD2, 0xFFD5, 0xFFD8, 0xFFDB,
            0xFFDE, 0xFFE7,
        ];
        for &addr in rts_stubs {
            s!(addr, 0x60);
        }

        // $FFE4 (GETIN) — LDA #$00; CLC; (RTS at $FFE7 above)
        s!(0xFFE4, 0xA9); s!(0xFFE5, 0x00); s!(0xFFE6, 0x18);
        // $FFE1 (STOP) — CLC; RTS
        s!(0xFFE1, 0x18); s!(0xFFE2, 0x60);
        // $E544 (CLRSCR) — RTS
        s!(0xE544, 0x60);

        // $FF48: KERNAL IRQ entry — saves A/X/Y, checks BRK, dispatches
        let kernal_irq: [u8; 19] = [
            0x48, 0x8A, 0x48, 0x98, 0x48, 0xBA, 0xBD, 0x04,
            0x01, 0x29, 0x10, 0xD0, 0x03, 0x6C, 0x14, 0x03,
            0x6C, 0x16, 0x03,
        ];
        rom[0xFF48 - 0xE000..0xFF48 - 0xE000 + 19].copy_from_slice(&kernal_irq);

        // $EA31: Default IRQ handler — ack CIA1 + ack VIC + bump jiffy
        s!(0xEA31, 0xAD); s!(0xEA32, 0x0D); s!(0xEA33, 0xDC); // LDA $DC0D
        s!(0xEA34, 0xA9); s!(0xEA35, 0xFF);                     // LDA #$FF
        s!(0xEA36, 0x8D); s!(0xEA37, 0x19); s!(0xEA38, 0xD0);  // STA $D019
        s!(0xEA39, 0xEE); s!(0xEA3A, 0xA2); s!(0xEA3B, 0x00);  // INC $00A2
        s!(0xEA3C, 0x4C); s!(0xEA3D, 0x81); s!(0xEA3E, 0xEA);  // JMP $EA81

        // $EA81: IRQ exit — PLA; TAY; PLA; TAX; PLA; RTI
        s!(0xEA81, 0x68); s!(0xEA82, 0xA8);
        s!(0xEA83, 0x68); s!(0xEA84, 0xAA);
        s!(0xEA85, 0x68); s!(0xEA86, 0x40);

        // $FE43: KERNAL NMI entry
        let kernal_nmi: [u8; 8] = [0x48, 0x8A, 0x48, 0x98, 0x48, 0x6C, 0x18, 0x03];
        rom[0xFE43 - 0xE000..0xFE43 - 0xE000 + 8].copy_from_slice(&kernal_nmi);

        // $FE72: Default NMI handler — LDA $DD0D; JMP $EA81
        s!(0xFE72, 0xAD); s!(0xFE73, 0x0D); s!(0xFE74, 0xDD);
        s!(0xFE75, 0x4C); s!(0xFE76, 0x81); s!(0xFE77, 0xEA);

        // Hardware interrupt vectors
        s!(0xFFFA, 0x43); s!(0xFFFB, 0xFE); // NMI  → $FE43
        s!(0xFFFC, 0x00); s!(0xFFFD, 0xE0); // RESET
        s!(0xFFFE, 0x48); s!(0xFFFF, 0xFF); // IRQ  → $FF48
    }

    /// Install software vectors in RAM ($0314–$0319).
    pub fn install_software_vectors(&mut self) {
        self.c64.ram.ram[0x0314] = 0x31;
        self.c64.ram.ram[0x0315] = 0xEA; // IRQ → $EA31
        self.c64.ram.ram[0x0316] = 0x81;
        self.c64.ram.ram[0x0317] = 0xEA; // BRK → $EA81
        self.c64.ram.ram[0x0318] = 0x72;
        self.c64.ram.ram[0x0319] = 0xFE; // NMI → $FE72
    }

    /// Set up C64 machine state (CPU port, zero-page, CIA DDRs, VIC defaults).
    pub fn setup_machine_state(&mut self, is_pal: bool) {
        let ram = &mut self.c64.ram.ram;

        // CPU port
        ram[0x0000] = 0x2F; // DDR: bits 0-2,5 output
        ram[0x0001] = 0x37; // BASIC+KERNAL+I/O visible

        // Zero-page / KERNAL workspace
        ram[0x02A6] = if is_pal { 0x01 } else { 0x00 };
        ram[0x0028] = 0xF0;
        ram[0x0037] = 0x00;
        ram[0x0038] = 0xA0;
        ram[0x0073] = 39;
        ram[0x0282] = 0x08;
        ram[0x0286] = 0x0E;
        ram[0x00C5] = 0x40;
        ram[0x00CB] = 0x40;
        ram[0x00C6] = 0x00;
        ram[0x028F] = 0x0A;

        // VIC register shadows in RAM
        ram[0xD018] = 0x15;
        ram[0xD020] = 0x0E;
        ram[0xD021] = 0x06;
        ram[0xD011] = 0x1B;

        // CIA DDR shadows
        ram[0xDC02] = 0xFF;
        ram[0xDC03] = 0x00;
        ram[0xDD02] = 0x3F;
        ram[0xDD03] = 0x00;
        ram[0xDD00] = 0x17;

        // Sync CPU port to the PLA/MMU
        // Write through the Bus impl to trigger the MMU update
        self.c64.set_byte(0x0000, 0x2F);
        self.c64.set_byte(0x0001, 0x37);

        // Configure CIA DDRs through the chip registers
        self.c64.cia1.regs[2] = 0xFF; // DDRA
        self.c64.cia1.regs[3] = 0x00; // DDRB
        self.c64.cia2.regs[2] = 0x3F; // DDRA
        self.c64.cia2.regs[3] = 0x00; // DDRB
        self.c64.cia2.regs[0] = 0x17; // PRA (VIC bank)
    }

    /// Set RSID defaults for CIA1 (timer A at frame rate, running, IRQ enabled).
    pub fn setup_rsid_cia_defaults(&mut self, is_pal: bool) {
        let latch: u16 = if is_pal { 0x4025 } else { 0x4295 };

        // Set timer A latch and load counter
        self.c64.cia1.timer_a.latch = latch;
        self.c64.cia1.timer_a.counter = latch;

        // Start timer A counting PHI2, continuous mode
        self.c64.cia1.write(0x0E, 0x11); // CRA: start + force-load
        self.c64.cia1.regs[0x0E] = 0x01; // CRA read-back: just started

        // Enable timer A interrupt
        self.c64.cia1.write(0x0D, 0x81); // ICR: set Timer A mask
    }

    /// Install trampoline at `at`: JSR target; JMP halt
    pub fn install_trampoline(&mut self, at: u16, target: u16) {
        let a = at as usize;
        self.c64.ram.ram[a]     = 0x20; // JSR
        self.c64.ram.ram[a + 1] = (target & 0xFF) as u8;
        self.c64.ram.ram[a + 2] = (target >> 8) as u8;
        self.c64.ram.ram[a + 3] = 0x4C; // JMP (halt)
        self.c64.ram.ram[a + 4] = ((at + 3) & 0xFF) as u8;
        self.c64.ram.ram[a + 5] = ((at + 3) >> 8) as u8;
    }

    /// Set a hardware vector in both RAM and KERNAL ROM overlay.
    pub fn set_hw_vector(&mut self, addr: u16, value: u16) {
        let lo = (value & 0xFF) as u8;
        let hi = (value >> 8) as u8;
        self.c64.ram.ram[addr as usize] = lo;
        self.c64.ram.ram[addr as usize + 1] = hi;
        if addr >= 0xE000 {
            let rom = self.c64.kernal_rom.rom_mut();
            let off = (addr - 0xE000) as usize;
            rom[off] = lo;
            rom[off + 1] = hi;
        }
    }

    pub fn clear_writes(&mut self) {
        self.sid_writes.clear();
        self.frame_cycle = 0;
    }

    /// Increment the KERNAL jiffy clock at $00A0–$00A2.
    pub fn tick_jiffy_clock(&mut self) {
        let a2 = self.c64.ram.ram[0x00A2].wrapping_add(1);
        self.c64.ram.ram[0x00A2] = a2;
        if a2 == 0 {
            let a1 = self.c64.ram.ram[0x00A1].wrapping_add(1);
            self.c64.ram.ram[0x00A1] = a1;
            if a1 == 0 {
                self.c64.ram.ram[0x00A0] = self.c64.ram.ram[0x00A0].wrapping_add(1);
            }
        }
    }

    /// Voice activity levels for the visualiser.
    pub fn voice_levels(&self) -> Vec<f32> {
        let num_sids = self.mapper.num_sids().max(1);
        let actual = if self.mono { 1 } else { num_sids };
        let mut levels = Vec::with_capacity(actual * 3);

        for sid in 0..actual {
            let base = (sid as usize) * SID_REG_SIZE as usize;
            let global_vol = (self.sid_shadow[base + 0x18] & 0x0F) as f32 / 15.0;
            for voice in 0..3 {
                let vo = base + voice * 7;
                let control = self.sid_shadow[vo + 4];
                let gate = control & 0x01;
                let sustain = (self.sid_shadow[vo + 6] >> 4) as f32 / 15.0;
                let level = if gate != 0 { sustain * global_vol } else { 0.0 };
                levels.push(level);
            }
        }
        levels
    }

    /// Check if an address maps to a SID register.  Returns the
    /// mapped USBSID register offset, or None.
    fn map_sid_write(&self, addr: u16) -> Option<u8> {
        if self.mono {
            if addr >= 0xD400 && addr <= 0xD7FF {
                Some((addr as u8) & 0x1F)
            } else {
                None
            }
        } else {
            self.mapper.map(addr)
        }
    }

    /// Check IRQ state: CIA1 or VIC.
    pub fn irq_pending(&self) -> bool {
        self.c64.cia1.interrupt.asserted || self.c64.vic.irq_state
    }

    /// Check NMI state: CIA2.
    pub fn nmi_pending(&self) -> bool {
        self.c64.cia2.interrupt.asserted
    }

    /// Get the opcode byte at `pc` through the banking layer.
    pub fn opcode_cycles(&self, pc: u16) -> u32 {
        // Read through KERNAL ROM banking
        let byte = if pc >= 0xE000 {
            // Check if KERNAL ROM is visible (HIRAM set)
            let port = self.c64.ram.ram[0x0001];
            if port & 0x02 != 0 {
                self.c64.kernal_rom.rom_ref()[(pc - 0xE000) as usize]
            } else {
                self.c64.ram.ram[pc as usize]
            }
        } else {
            self.c64.ram.ram[pc as usize]
        };
        OPCODE_CYCLES[byte as usize] as u32
    }

    /// Clear stale CIA interrupt flags after INIT.
    pub fn clear_stale_ints(&mut self) {
        // If timer A is not started, clear its pending flag
        if !self.c64.cia1.timer_a.started() {
            self.c64.cia1.interrupt.clear();
        }
        if !self.c64.cia2.timer_a.started() {
            self.c64.cia2.interrupt.clear();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Bus implementation — intercept SID writes, delegate everything else
// ─────────────────────────────────────────────────────────────────────────────

impl Bus for RsidBus {
    fn get_byte(&mut self, addr: u16) -> u8 {
        // Intercept SID reads for osc3 / envelope when I/O is mapped
        if addr >= 0xD400 && addr <= 0xD7FF
            && self.c64.mmu.read_map[0xD] == PageMapping::Io
        {
            return match (addr & 0x1F) as u8 {
                0x1B => {
                    self.osc3_seed = self.osc3_seed
                        .wrapping_mul(1103515245)
                        .wrapping_add(12345);
                    (self.osc3_seed >> 16) as u8
                }
                0x1C => 0xFF,
                0x19 => 0x80, // potX
                0x1A => 0x80, // potY
                _ => 0,
            };
        }

        self.c64.get_byte(addr)
    }

    fn set_byte(&mut self, addr: u16, val: u8) {
        // Intercept SID writes before passing through to the C64 core
        if addr >= 0xD400 && addr <= 0xD7FF
            && self.c64.mmu.write_map[0xD] == PageMapping::Io
        {
            if let Some(reg) = self.map_sid_write(addr) {
                self.sid_writes.push((self.frame_cycle, reg, val));
                self.sid_shadow[reg as usize] = val;
            }
        }

        // Sync writes to KERNAL area into the ROM overlay so that
        // tune code installed in $E000-$FFFF is visible through banking
        if addr >= 0xE000 {
            let rom = self.c64.kernal_rom.rom_mut();
            rom[(addr - 0xE000) as usize] = val;
        }

        self.c64.set_byte(addr, val);
    }
}

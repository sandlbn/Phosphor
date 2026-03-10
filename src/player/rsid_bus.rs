// RSID bus: wraps the cycle-accurate c64_emu::C64 core and intercepts
// SID register writes so they can be forwarded to the USBSID hardware.
//
// Only used for RSID playback. PSID continues to use the simpler C64Memory.

use mos6502::memory::Bus;

use crate::c64_emu::banks::Bank;
use crate::c64_emu::c64::{C64CiaModel, C64Model, C64};
use crate::c64_emu::mmu::PageMapping;

use super::hacks::HackFlags;
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
    /// Hack flags set during setup; carried into the emulation loops.
    pub hack_flags: HackFlags,
}

impl RsidBus {
    pub fn new(is_pal: bool, mapper: SidMapper, mono: bool) -> Self {
        let (mut c64, roms_loaded) = C64::new_with_auto_roms();
        if !roms_loaded {
            eprintln!("[rsid_bus] ROM files not found — running with stub ROMs");
        } else {
            eprintln!("[rsid_bus] ROM files  found");
        }

        c64.set_model(if is_pal {
            C64Model::PalB
        } else {
            C64Model::NtscM
        });
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
            hack_flags: HackFlags::default(),
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
            0xFF81, 0xFF84, 0xFF87, 0xFF8A, 0xFF8D, 0xFF90, 0xFF93, 0xFF96, 0xFF99, 0xFF9C, 0xFFA5,
            0xFFB1, 0xFFB4, 0xFFBD, 0xFFC0, 0xFFC3, 0xFFC6, 0xFFC9, 0xFFCC, 0xFFCF, 0xFFD2, 0xFFD5,
            0xFFD8, 0xFFDB, 0xFFDE, 0xFFE7,
        ];
        for &addr in rts_stubs {
            s!(addr, 0x60);
        }

        // $FFE4 (GETIN) — LDA #$00; CLC; (RTS at $FFE7 above)
        s!(0xFFE4, 0xA9);
        s!(0xFFE5, 0x00);
        s!(0xFFE6, 0x18);
        // $FFE1 (STOP) — CLC; RTS
        s!(0xFFE1, 0x18);
        s!(0xFFE2, 0x60);
        // $E544 (CLRSCR) — RTS
        s!(0xE544, 0x60);

        // ── $FF48: KERNAL IRQ entry ─────────────────────────────────────────
        // Saves A/X/Y, checks if source is BRK (bit 4 of stacked P).
        // Reference uses BEQ (F0): if B=0 (normal IRQ) branch skips the 3 NOPs
        // and falls into JMP($0314).  If B=1 (BRK) fall through NOPs to JMP($0314)
        // anyway — reference routes both through $0314.
        // F0 03 = BEQ +3  (branch if B==0, i.e. normal IRQ → skip 3 NOPs to JMP)
        let kernal_irq: [u8; 19] = [
            0x48, 0x8A, 0x48, 0x98, 0x48,  // PHA; TXA;PHA; TYA;PHA
            0xBA,                           // TSX
            0xBD, 0x04, 0x01,               // LDA $0104,X  (stacked P)
            0x29, 0x10,                     // AND #$10     (test B flag)
            0xF0, 0x03,                     // BEQ +3       (normal IRQ → skip NOPs)
            0xEA, 0xEA, 0xEA,               // NOP NOP NOP  (BRK path, falls through)
            0x6C, 0x14, 0x03,               // JMP ($0314)  (user IRQ handler)
        ];
        rom[0xFF48 - 0xE000..0xFF48 - 0xE000 + 19].copy_from_slice(&kernal_irq);

        // ── $EA31–$EA74: NOP sled (reference fills this range with NOPs) ───
        // Tunes that JMP into the middle of the EA31 area still reach EA75.
        for addr in 0xEA31u16..0xEA75 {
            s!(addr, 0xEA); // NOP
        }

        // ── $EA75: IRQ return handler (matches WebSid _irq_handler_EA75) ───
        // LDA $01 / AND #$1F / STA $01 — restores KERNAL bank in case tune
        //   switched it off; ensures $DC0D read goes through I/O.
        // INC $A2  — jiffy clock lo-byte (frame counter)
        // NOP      — timing pad (present in reference)
        // LDA $DC0D — ack CIA1 interrupt (reading clears flags)
        // PLA/TAY/PLA/TAX/PLA/RTI — restore Y, X, A; return from interrupt
        let ea75: [u8; 18] = [
            0xA5, 0x01,             // LDA $01
            0x29, 0x1F,             // AND #$1F   (keep lower 5 bits = enable KERNAL/IO)
            0x85, 0x01,             // STA $01    (restore bank: KERNAL+IO visible)
            0xE6, 0xA2,             // INC $A2    (jiffy counter, frame tick)
            0xEA,                   // NOP
            0xAD, 0x0D, 0xDC,       // LDA $DC0D  (ack CIA1, clears interrupt flag)
            0x68, 0xA8,             // PLA; TAY
            0x68, 0xAA,             // PLA; TAX
            0x68,                   // PLA
            0x40,                   // RTI
        ];
        rom[0xEA75 - 0xE000..0xEA75 - 0xE000 + 18].copy_from_slice(&ea75);

        // ── $EA81: IRQ exit — register restore + RTI (alias used by many tunes) ─
        // (EA81 falls inside the NOP sled range above, so we patch it explicitly)
        s!(0xEA81, 0x68); // PLA
        s!(0xEA82, 0xA8); // TAY
        s!(0xEA83, 0x68); // PLA
        s!(0xEA84, 0xAA); // TAX
        s!(0xEA85, 0x68); // PLA
        s!(0xEA86, 0x40); // RTI

        // ── $FEBC: IRQ end handler (same as EA81 — PLA;TAY;PLA;TAX;PLA;RTI) ─
        // Some tunes (e.g. Contact_Us_tune_2) JMP here to finish IRQ.
        let febc: [u8; 6] = [0x68, 0xA8, 0x68, 0xAA, 0x68, 0x40];
        rom[0xFEBC - 0xE000..0xFEBC - 0xE000 + 6].copy_from_slice(&febc);

        // ── $FE43: KERNAL NMI entry ─────────────────────────────────────────
        // Reference: SEI; JMP($0318)  — just disables IRQs and dispatches.
        // Does NOT push A/X/Y.  Tunes save their own registers.
        // (Our old stub pushed A,X,Y but never popped them → 3-byte stack leak
        //  per NMI which would corrupt the stack within seconds of playback.)
        let kernal_nmi: [u8; 5] = [
            0x78,               // SEI
            0x6C, 0x18, 0x03,   // JMP ($0318)  (user NMI handler)
            0x40,               // RTI  (unreachable, just a safety net)
        ];
        rom[0xFE43 - 0xE000..0xFE43 - 0xE000 + 5].copy_from_slice(&kernal_nmi);

        // ── $FE72: Default NMI handler — ack CIA2 + RTI ────────────────────
        // FE43 does SEI; JMP($0318) without pushing any registers.
        // So the default handler at $0318 must NOT pop registers either.
        // Reference default $0318 points to FE47 = RTI (the last byte of FE43).
        // We use FE72 as our default, matching that: just ack CIA2 and RTI.
        // Tunes that need register save/restore install their own handler at $0318.
        s!(0xFE72, 0xAD); // LDA $DD0D  (ack CIA2 NMI, clears interrupt flag)
        s!(0xFE73, 0x0D);
        s!(0xFE74, 0xDD);
        s!(0xFE75, 0x40); // RTI        (return — hardware already saved/restores PC+P)

        // ── $FF6E: Schedule timer A (KERNAL routine used by some INIT code) ─
        // Enables CIA1 timer-A interrupt and starts the timer with force-load.
        // Then falls into $EE8E which raises the serial clock line on CIA2.
        // Reference: LDA #$81; STA $DC0D; LDA $DC0E; AND #$80; ORA #$11;
        //            STA $DC0E; JMP $EE8E
        let ff6e: [u8; 18] = [
            0xA9, 0x81,             // LDA #$81  (set CIA1 timer-A IRQ mask)
            0x8D, 0x0D, 0xDC,       // STA $DC0D
            0xAD, 0x0E, 0xDC,       // LDA $DC0E (read CRA)
            0x29, 0x80,             // AND #$80  (keep TOD-frequency bit only)
            0x09, 0x11,             // ORA #$11  (set start + force-load)
            0x8D, 0x0E, 0xDC,       // STA $DC0E (write CRA: start timer)
            0x4C, 0x8E, 0xEE,       // JMP $EE8E
        ];
        rom[0xFF6E - 0xE000..0xFF6E - 0xE000 + 18].copy_from_slice(&ff6e);

        // ── $EE8E: Serial clock high (tail of FF6E, also called directly) ───
        // Sets bit 4 of CIA2 port-A (serial clock line high), then RTS.
        let ee8e: [u8; 9] = [
            0xAD, 0x00, 0xDD,   // LDA $DD00
            0x09, 0x10,         // ORA #$10
            0x8D, 0x00, 0xDD,   // STA $DD00
            0x60,               // RTS
        ];
        rom[0xEE8E - 0xE000..0xEE8E - 0xE000 + 9].copy_from_slice(&ee8e);

        // ── $FDA3: Initialize I/O devices (KERNAL routine) ──────────────────
        // Disables ALL CIA1+CIA2 interrupts, stops all timers, resets DDRs/ports.
        // Tunes that call JSR $FDA3 expect the CIA to be in a clean disabled state
        // before they configure their own timer settings.
        let fda3: [u8; 83] = [
            0xA9, 0x7F, 0x8D, 0x0D, 0xDC,   // LDA #$7F; STA $DC0D (disable CIA1 irqs)
            0x8D, 0x0D, 0xDD,                // STA $DD0D (disable CIA2 irqs)
            0x8D, 0x00, 0xDC,                // STA $DC00 (CIA1 port A)
            0xA9, 0x08, 0x8D, 0x0E, 0xDC,   // LDA #$08; STA $DC0E (CRA: one-shot, stopped)
            0x8D, 0x0E, 0xDD,                // STA $DD0E (CIA2 CRA: one-shot, stopped)
            0xA9, 0x00, 0x8D, 0x0F, 0xDC,   // LDA #$00; STA $DC0F (CRB: stopped)
            0xA9, 0x00, 0x8D, 0x0F, 0xDD,   // LDA #$00; STA $DD0F (CRB: stopped)
            0xA9, 0xFF, 0x8D, 0x02, 0xDC,   // LDA #$FF; STA $DC02 (DDRA: all outputs)
            0xA9, 0x00, 0x8D, 0x03, 0xDC,   // LDA #$00; STA $DC03 (DDRB: all inputs)
            0x8D, 0x03, 0xDD,                // STA $DD03 (CIA2 DDRB)
            0xA9, 0x3F, 0x8D, 0x02, 0xDD,   // LDA #$3F; STA $DD02 (CIA2 DDRA)
            0xA9, 0x17, 0x8D, 0x00, 0xDD,   // LDA #$17; STA $DD00 (CIA2 PRA: VIC bank)
            0xA9, 0x7F, 0x8D, 0x01, 0xDD,   // LDA #$7F; STA $DD01 (CIA2 PRB)
            0xA2, 0x00,                      // LDX #$00
            0x8E, 0x12, 0xDC,                // STX $DC12 (CIA1 SDR)
            0x8E, 0x02, 0xDD,                // STX $DD02 (CIA2 DDRA = 0)
            0xA9, 0x00, 0x8D, 0x11, 0xDC,   // LDA #$00; STA $DC11 (VIC ctrl)
            0x8D, 0x08, 0xDC,                // STA $DC08 (CIA1 TOD 1/10s)
            0x8E, 0x09, 0xDC,                // STX $DC09 (CIA1 TOD sec)
            0x8E, 0x08, 0xDD,                // STX $DD08 (CIA2 TOD 1/10s)
            0x8E, 0x09, 0xDD,                // STX $DD09 (CIA2 TOD sec)
            0x60,                            // RTS
        ];
        rom[0xFDA3 - 0xE000..0xFDA3 - 0xE000 + 83].copy_from_slice(&fda3);

        // Hardware interrupt vectors
        s!(0xFFFA, 0x43);
        s!(0xFFFB, 0xFE); // NMI  → $FE43
        s!(0xFFFC, 0x00);
        s!(0xFFFD, 0xE0); // RESET
        s!(0xFFFE, 0x48);
        s!(0xFFFF, 0xFF); // IRQ  → $FF48
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
        // ── Phase 1: raw RAM writes (CPU port, ZP, stack sentinels) ──────────
        // Scoped so `ram` borrow is dropped before the set_byte() calls below.
        {
            let ram = &mut self.c64.ram.ram;

            // CPU port (will be properly committed via set_byte in Phase 2)
            ram[0x0000] = 0x2F; // DDR: bits 0-2,5 output
            ram[0x0001] = 0x37; // BASIC+KERNAL+I/O visible

            // Stack sentinel values (matching WebSid reference: $01FE=0xFF, $01FF=0x7F)
            ram[0x01FE] = 0xFF;
            ram[0x01FF] = 0x7F;

            // Flag used by some tunes to detect RSID environment
            ram[0xA003] = 0x80;

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
        } // `ram` borrow ends — self is now freely accessible

        // ── Phase 2: I/O writes through the MMU ──────────────────────────────

        // CPU port: goes through zero_ram bank so the MMU recalculates its
        // read/write maps (bank $37 → BASIC+KERNAL+I/O visible).
        self.c64.set_byte(0x0000, 0x2F);
        self.c64.set_byte(0x0001, 0x37);

        // VIC register initialisation.
        // These MUST go through c64.set_byte() so the actual VIC chip registers
        // are updated.  With bank=$37 the $D000-$DFFF page is I/O; writing
        // ram.ram[$D0xx] directly only touches the underlying RAM shadow and
        // never reaches the VIC registers.
        //
        //   $D018 = 0x15  (VIC memory control: char base $3800, screen $0400)
        //   $D020 = 0x0E  (border colour: light blue)
        //   $D021 = 0x06  (background colour: blue)
        //   $D011 = 0x1B  (screen ctrl: DEN=1, RSEL=1, YSCROLL=3 → enables badlines)
        //
        // $D011 with DEN=1 is critical: without it are_bad_lines_enabled is
        // never set, so BA never goes low and BA-stun never fires.
        self.c64.set_byte(0xD018, 0x15);
        self.c64.set_byte(0xD020, 0x0E);
        self.c64.set_byte(0xD021, 0x06);
        self.c64.set_byte(0xD011, 0x1B);

        // CIA DDRs: write directly to chip registers (not through set_byte,
        // which would route through I/O and the CIA write handler — DDR writes
        // via the Bus interface are fine but the regs[] shortcut is simpler here).
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
        self.c64.ram.ram[a] = 0x20; // JSR
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
        // Resolve opcode byte through the same MMU banking that single_step() uses,
        // so cycle counts match what the CPU actually executes.
        //
        // We replicate the read-map lookup manually (without &mut self) for the
        // two ROM regions that matter for SID tunes:
        //   $A000-$BFFF — BASIC ROM (when LORAM+HIRAM set, port $01 bits 0+1)
        //   $E000-$FFFF — KERNAL ROM (when HIRAM set, port $01 bit 1)
        // All other regions read from RAM, matching the MMU default for writes.
        let page = (pc >> 12) as usize;
        let byte = match self.c64.mmu.read_map[page] {
            PageMapping::BasicRom  => self.c64.basic_rom.peek(pc),
            PageMapping::KernalRom => self.c64.kernal_rom.peek(pc),
            _                      => self.c64.ram.ram[pc as usize],
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
        if addr >= 0xD400 && addr <= 0xD7FF && self.c64.mmu.read_map[0xD] == PageMapping::Io {
            return match (addr & 0x1F) as u8 {
                0x1B => {
                    self.osc3_seed = self.osc3_seed.wrapping_mul(1103515245).wrapping_add(12345);
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
        if addr >= 0xD400 && addr <= 0xD7FF && self.c64.mmu.write_map[0xD] == PageMapping::Io {
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
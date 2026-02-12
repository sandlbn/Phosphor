//! Top-level Commodore 64 emulation core.
//!
//! Wires together the `mos6502` crate CPU with the VIC-II, two CIAs,
//! memory banks, and the PLA/MMU.

use mos6502::memory::Bus;

use super::banks::*;
use super::banks::io_bank::IoChip;
use super::cia::Mos652x;
use super::cia::interrupt::CiaModel;
use super::mmu::{Mmu, PageMapping};
use super::vic_ii::{Mos656x, VicModel};

// ── C64 model definitions ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C64Model {
    PalB,
    NtscM,
    OldNtscM,
    PalN,
    PalM,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C64CiaModel {
    Old,
    New,
    Old4485,
}

struct ModelParams {
    color_burst: f64,
    divider: f64,
    power_freq: f64,
    vic_model: VicModel,
}

const MODELS: [ModelParams; 5] = [
    ModelParams { color_burst: 4_433_618.75,  divider: 18.0, power_freq: 50.0, vic_model: VicModel::Mos6569 },
    ModelParams { color_burst: 3_579_545.455, divider: 14.0, power_freq: 60.0, vic_model: VicModel::Mos6567R8 },
    ModelParams { color_burst: 3_579_545.455, divider: 14.0, power_freq: 60.0, vic_model: VicModel::Mos6567R56A },
    ModelParams { color_burst: 3_582_056.25,  divider: 14.0, power_freq: 50.0, vic_model: VicModel::Mos6572 },
    ModelParams { color_burst: 3_575_611.49,  divider: 14.0, power_freq: 50.0, vic_model: VicModel::Mos6573 },
];

fn cpu_freq(model: C64Model) -> f64 {
    let m = &MODELS[model as usize];
    (m.color_burst * 4.0) / m.divider
}

fn to_cia_model(m: C64CiaModel) -> CiaModel {
    match m {
        C64CiaModel::Old     => CiaModel::Mos6526,
        C64CiaModel::New     => CiaModel::Mos8521,
        C64CiaModel::Old4485 => CiaModel::Mos6526W4485,
    }
}

// ── C64 Machine ───────────────────────────────────────────────

/// The complete C64 machine state.
///
/// Implements `mos6502::memory::Bus` so it can be passed directly to
/// the `mos6502` CPU as its address bus.
pub struct C64 {
    // ── Chips ──
    pub vic: Mos656x,
    pub cia1: Mos652x,
    pub cia2: Mos652x,

    // ── Memory ──
    pub ram: SystemRamBank,
    pub kernal_rom: KernalRomBank,
    pub basic_rom: BasicRomBank,
    pub char_rom: CharacterRomBank,
    pub color_ram: ColorRamBank,
    pub sid_bank: SidBank,
    pub disconnected_bus: DisconnectedBusBank,
    pub zero_ram: ZeroRamBank,
    pub io_bank: IoBank,

    // ── PLA / mapping ──
    pub mmu: Mmu,

    // ── IRQ counting ──
    irq_count: i32,
    old_ba_state: bool,

    // ── Clock ──
    pub cpu_frequency: f64,
    pub cycle_count: u64,
}

impl C64 {
    pub fn new() -> Self {
        let mut c64 = Self {
            vic: Mos656x::new(),
            cia1: Mos652x::new(CiaModel::Mos6526),
            cia2: Mos652x::new(CiaModel::Mos6526),

            ram: SystemRamBank::new(),
            kernal_rom: KernalRomBank::new(),
            basic_rom: BasicRomBank::new(),
            char_rom: CharacterRomBank::new(),
            color_ram: ColorRamBank::new(),
            sid_bank: SidBank::new(),
            disconnected_bus: DisconnectedBusBank::new(),
            zero_ram: ZeroRamBank::new(),
            io_bank: IoBank::default(),

            mmu: Mmu::new(),

            irq_count: 0,
            old_ba_state: true,

            cpu_frequency: cpu_freq(C64Model::PalB),
            cycle_count: 0,
        };
        c64.kernal_rom.set(None);
        c64
    }

    // ── Model configuration ───────────────────────────────────

    pub fn set_model(&mut self, model: C64Model) {
        self.cpu_frequency = cpu_freq(model);
        let m = &MODELS[model as usize];
        self.vic.chip(m.vic_model);
        let rate = (self.cpu_frequency / m.power_freq) as u32;
        self.cia1.set_day_of_time_rate(rate);
        self.cia2.set_day_of_time_rate(rate);
    }

    pub fn set_cia_model(&mut self, model: C64CiaModel) {
        let cm = to_cia_model(model);
        self.cia1.set_model(cm);
        self.cia2.set_model(cm);
    }

    // ── ROM loading ───────────────────────────────────────────

    pub fn set_kernal(&mut self, rom: Option<&[u8]>) { self.kernal_rom.set(rom); }
    pub fn set_basic(&mut self, rom: Option<&[u8]>)  { self.basic_rom.set(rom); }
    pub fn set_chargen(&mut self, rom: Option<&[u8]>) { self.char_rom.set(rom); }

    // ── SID ───────────────────────────────────────────────────

    pub fn set_base_sid(&mut self, s: Option<Box<dyn sid_bank::SidChip>>) {
        self.sid_bank.set_sid(s);
    }

    // ── Reset ─────────────────────────────────────────────────

    pub fn reset(&mut self) {
        self.cia1.reset();
        self.cia2.reset();
        self.vic.reset();
        self.sid_bank.reset();
        self.color_ram.reset();
        self.ram.reset();
        self.zero_ram.reset();
        self.kernal_rom.reset();
        self.basic_rom.reset();
        self.mmu.reset();
        self.irq_count = 0;
        self.old_ba_state = true;
        self.cycle_count = 0;
    }

    // ── Per-cycle chip tick ───────────────────────────────────

    /// Call once per PHI2 cycle BEFORE the CPU step.
    /// Returns `(irq_asserted, nmi_asserted)`.
    pub fn tick_peripherals(&mut self) -> (bool, bool) {
        self.cycle_count += 1;
        let mut irq = false;
        let mut nmi = false;

        // VIC-II
        let vic_out = self.vic.tick();
        if let Some(true) = vic_out.irq {
            irq = true;
        }

        // CIA1 → IRQ
        if let Some(true) = self.cia1.tick() {
            irq = true;
        }

        // CIA2 → NMI
        if let Some(true) = self.cia2.tick() {
            nmi = true;
        }

        (irq, nmi)
    }

    // ── Time helpers ──────────────────────────────────────────

    pub fn get_time_ms(&self) -> u32 {
        ((self.cycle_count as f64 * 1000.0) / self.cpu_frequency) as u32
    }

    /// Helper: read memory the way the CPU sees it (with banking).
    /// Useful for debuggers / disassemblers.
    #[allow(dead_code)]
    fn cpu_read_internal(&self, addr: u16) -> u8 {
        let page = (addr >> 12) as usize;

        // Page 0: zero-page / CPU port
        if page == 0 && addr < 2 {
            // Need mutable access for side-effects; handled via Bus trait.
            // For const peek we return a reasonable default.
            return 0;
        }

        match self.mmu.read_map[page] {
            PageMapping::Ram => self.ram.peek(addr),
            PageMapping::BasicRom => self.basic_rom.peek(addr),
            PageMapping::KernalRom => self.kernal_rom.peek(addr),
            PageMapping::CharacterRom => self.char_rom.peek(addr),
            PageMapping::Io => self.io_read(addr),
        }
    }

    #[allow(dead_code)]
    fn io_read(&self, addr: u16) -> u8 {
        match self.io_bank.dispatch(addr) {
            IoChip::Vic => self.vic.read((addr & 0x3F) as u8),
            IoChip::Sid => self.sid_bank.peek(addr),
            IoChip::ColorRam => self.color_ram.peek(addr),
            IoChip::Cia1 | IoChip::Cia2 => {
                // CIA reads have side-effects; done in Bus::get_byte.
                0
            }
            IoChip::DisconnectedBus => self.disconnected_bus.peek(addr),
            IoChip::ExtraSid(_) => 0xFF,
        }
    }
}

impl Default for C64 {
    fn default() -> Self { Self::new() }
}

// ── mos6502 Bus implementation ────────────────────────────────
//
// This is the bridge between the `mos6502` crate's CPU and our C64.
// The CPU calls `get_byte` / `set_byte` and we route through the MMU.

impl Bus for C64 {
    fn get_byte(&mut self, addr: u16) -> u8 {
        let page = (addr >> 12) as usize;

        // Zero-page: CPU port
        if page == 0 && addr < 2 {
            return self.zero_ram.peek_mut(addr);
        }

        match self.mmu.read_map[page] {
            PageMapping::Ram => self.ram.peek(addr),
            PageMapping::BasicRom => self.basic_rom.peek(addr),
            PageMapping::KernalRom => self.kernal_rom.peek(addr),
            PageMapping::CharacterRom => self.char_rom.peek(addr),
            PageMapping::Io => {
                match self.io_bank.dispatch(addr) {
                    IoChip::Vic => self.vic.read((addr & 0x3F) as u8),
                    IoChip::Sid => self.sid_bank.peek(addr),
                    IoChip::ColorRam => self.color_ram.peek(addr),
                    IoChip::Cia1 => {
                        let (val, _irq_delta) = self.cia1.read((addr & 0x0F) as u8);
                        val
                    }
                    IoChip::Cia2 => {
                        let (val, _irq_delta) = self.cia2.read((addr & 0x0F) as u8);
                        val
                    }
                    IoChip::DisconnectedBus => self.mmu.last_read_byte(),
                    IoChip::ExtraSid(_) => 0xFF,
                }
            }
        }
    }

    fn set_byte(&mut self, addr: u16, val: u8) {
        let page = (addr >> 12) as usize;

        // Zero-page: CPU port (and also write to underlying RAM)
        if page == 0 {
            if addr < 2 {
                self.zero_ram.poke(addr, val);
                // Sync the PLA state from the zero_ram bank to the MMU.
                // We re-read the effective port value.
                let dir = self.zero_ram.peek_mut(0);
                let data = self.zero_ram.peek_mut(1);
                let state = (data | !dir) & 0x07;
                self.mmu.set_cpu_port(state);
            }
            self.ram.poke(addr, val);
            return;
        }

        match self.mmu.write_map[page] {
            PageMapping::Io => {
                // Also write to underlying RAM (C64 always writes to RAM).
                self.ram.poke(addr, val);

                match self.io_bank.dispatch(addr) {
                    IoChip::Vic => {
                        let _out = self.vic.write((addr & 0x3F) as u8, val);
                    }
                    IoChip::Sid => self.sid_bank.poke(addr, val),
                    IoChip::ColorRam => self.color_ram.poke(addr, val),
                    IoChip::Cia1 => {
                        let _irq_delta = self.cia1.write((addr & 0x0F) as u8, val);
                    }
                    IoChip::Cia2 => {
                        let _irq_delta = self.cia2.write((addr & 0x0F) as u8, val);
                    }
                    IoChip::DisconnectedBus => { /* no-op */ }
                    IoChip::ExtraSid(_) => { /* handled by extra sid bank */ }
                }
            }
            _ => {
                // RAM (ROM writes are ignored; writes always hit RAM).
                self.ram.poke(addr, val);
            }
        }
    }
}

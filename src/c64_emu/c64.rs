//! Top-level Commodore 64 emulation core.
//!
//! Wires together the `mos6502` crate CPU with the VIC-II, two CIAs,
//! memory banks, and the PLA/MMU.

use mos6502::memory::Bus;

use super::banks::io_bank::IoChip;
use super::banks::sid_bank::SidChip;
use super::banks::*;
use super::cia::interrupt::CiaModel;
use super::cia::Mos652x;
use super::mmu::{Mmu, PageMapping};
use super::roms::RomSet;
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
    ModelParams {
        color_burst: 4_433_618.75,
        divider: 18.0,
        power_freq: 50.0,
        vic_model: VicModel::Mos6569,
    },
    ModelParams {
        color_burst: 3_579_545.455,
        divider: 14.0,
        power_freq: 60.0,
        vic_model: VicModel::Mos6567R8,
    },
    ModelParams {
        color_burst: 3_579_545.455,
        divider: 14.0,
        power_freq: 60.0,
        vic_model: VicModel::Mos6567R56A,
    },
    ModelParams {
        color_burst: 3_582_056.25,
        divider: 14.0,
        power_freq: 50.0,
        vic_model: VicModel::Mos6572,
    },
    ModelParams {
        color_burst: 3_575_611.49,
        divider: 14.0,
        power_freq: 50.0,
        vic_model: VicModel::Mos6573,
    },
];

fn cpu_freq(model: C64Model) -> f64 {
    let m = &MODELS[model as usize];
    (m.color_burst * 4.0) / m.divider
}

fn to_cia_model(m: C64CiaModel) -> CiaModel {
    match m {
        C64CiaModel::Old => CiaModel::Mos6526,
        C64CiaModel::New => CiaModel::Mos8521,
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
    pub extra_sid: ExtraSidBank,
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
    // ── Constructors ──────────────────────────────────────────

    /// Create a C64 with the minimal stub ROMs (no real ROM files needed).
    ///
    /// Suitable for running raw machine-code / SID tunes that set up their
    /// own environment.  Call [`C64::with_roms`] for full BASIC/Kernal boot.
    pub fn new() -> Self {
        let mut c64 = Self::new_uninit();
        // Install stub ROM so vectors are valid even without real images.
        c64.kernal_rom.set(None);
        c64
    }

    /// Create a C64 and load the three standard ROM images from `roms`.
    ///
    /// # Example
    /// ```no_run
    /// use c64_emu::roms::RomSet;
    /// use c64_emu::c64::C64;
    ///
    /// let roms = RomSet::load().expect("could not find C64 ROMs");
    /// let mut c64 = C64::with_roms(&roms);
    /// ```
    pub fn with_roms(roms: &RomSet) -> Self {
        let mut c64 = Self::new_uninit();
        c64.kernal_rom.set(Some(&roms.kernal));
        c64.basic_rom.set(Some(&roms.basic));
        c64.char_rom.set(Some(&roms.chargen));
        c64
    }

    /// Create a C64 and attempt to auto-discover ROM files on disk.
    ///
    /// If ROM files cannot be found the emulator falls back to the minimal
    /// stub ROMs (same behaviour as [`C64::new`]) and the returned `bool`
    /// will be `false`.
    ///
    /// Search order for ROM files:
    /// 1. `$C64_ROM_DIR` environment variable
    /// 2. `./roms/`
    /// 3. `./`
    /// 4. `~/.local/share/c64/roms/`
    /// 5. `/usr/share/vice/C64/`
    ///
    /// Required files: `kernal.rom` (8 KiB), `basic.rom` (8 KiB),
    /// `chargen.rom` (4 KiB).
    pub fn new_with_auto_roms() -> (Self, bool) {
        match RomSet::load() {
            Ok(roms) => (Self::with_roms(&roms), true),
            Err(e) => {
                eprintln!("[c64] ROM auto-load failed: {e}");
                eprintln!("[c64] Falling back to stub ROMs.");
                (Self::new(), false)
            }
        }
    }

    /// Internal: allocate all fields but do not install any ROM content yet.
    fn new_uninit() -> Self {
        Self {
            vic: Mos656x::new(),
            cia1: Mos652x::new(CiaModel::Mos6526),
            cia2: Mos652x::new(CiaModel::Mos6526),

            ram: SystemRamBank::new(),
            kernal_rom: KernalRomBank::new(),
            basic_rom: BasicRomBank::new(),
            char_rom: CharacterRomBank::new(),
            color_ram: ColorRamBank::new(),
            sid_bank: SidBank::new(),
            extra_sid: ExtraSidBank::new(),
            disconnected_bus: DisconnectedBusBank::new(),
            zero_ram: ZeroRamBank::new(),
            io_bank: IoBank::default(),

            mmu: Mmu::new(),

            irq_count: 0,
            old_ba_state: true,

            cpu_frequency: cpu_freq(C64Model::PalB),
            cycle_count: 0,
        }
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

    /// Load or replace the Kernal ROM image at runtime.
    ///
    /// Pass `None` to revert to the minimal stub.
    pub fn set_kernal(&mut self, rom: Option<&[u8]>) {
        self.kernal_rom.set(rom);
    }

    /// Load or replace the BASIC ROM image at runtime.
    pub fn set_basic(&mut self, rom: Option<&[u8]>) {
        self.basic_rom.set(rom);
    }

    /// Load or replace the Character ROM image at runtime.
    pub fn set_chargen(&mut self, rom: Option<&[u8]>) {
        self.char_rom.set(rom);
    }

    /// Convenience: reload all three ROMs from a `RomSet`.
    pub fn load_roms(&mut self, roms: &RomSet) {
        self.kernal_rom.set(Some(&roms.kernal));
        self.basic_rom.set(Some(&roms.basic));
        self.char_rom.set(Some(&roms.chargen));
    }

    // ── SID ───────────────────────────────────────────────────

    pub fn set_base_sid(&mut self, s: Option<Box<dyn sid_bank::SidChip>>) {
        self.sid_bank.set_sid(s);
    }

    /// Register an extra SID chip at the given full C64 address
    /// (e.g. $D420, $D500, $DE00, $DF00).
    ///
    /// For addresses outside the primary SID page ($D4xx), the io_bank is
    /// updated to route that 256-byte page to ExtraSid automatically.
    /// For $D4xx addresses (e.g. $D420), the extra chip lives alongside
    /// the primary SID bank; get_byte/set_byte checks extra_sid first.
    pub fn set_extra_sid(&mut self, addr: u16, sid: Box<dyn SidChip>) {
        self.extra_sid.add_sid(sid, addr);
        let page = ((addr >> 8) & 0x0F) as usize;
        if page != 0x4 {
            self.io_bank.set_bank(page, IoChip::ExtraSid(0));
        }
    }

    // ── Reset ─────────────────────────────────────────────────

    pub fn reset(&mut self) {
        self.cia1.reset();
        self.cia2.reset();
        self.vic.reset();
        self.sid_bank.reset();
        self.extra_sid.reset();
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
        self.zero_ram.phi2_time = self.cycle_count as i64;
        let mut nmi = false;

        // VIC-II
        let vic_out = self.vic.tick();
        if let Some(changed) = vic_out.irq {
            self.irq_count += if changed { 1 } else { -1 };
            if self.irq_count < 0 {
                self.irq_count = 0;
            }
        }
        if let Some(ba) = vic_out.ba {
            self.old_ba_state = ba;
        }

        // CIA1 → IRQ
        if let Some(changed) = self.cia1.tick() {
            self.irq_count += if changed { 1 } else { -1 };
            if self.irq_count < 0 {
                self.irq_count = 0;
            }
        }

        // CIA2 → NMI
        if let Some(true) = self.cia2.tick() {
            nmi = true;
        }

        (self.irq_count > 0, nmi)
    }

    /// Returns true when the VIC is holding BA low (CPU bus not available).
    pub fn is_cpu_jammed(&self) -> bool {
        !self.vic.ba_state
    }

    /// Assert CIA1 FLAG pin (e.g. from serial bus or cassette).
    pub fn cia1_set_flag(&mut self) {
        if let Some(true) = self.cia1.set_flag() {
            self.irq_count += 1;
        }
    }

    /// Assert CIA2 FLAG pin (e.g. from serial bus).
    pub fn cia2_set_flag(&mut self) {
        let _ = self.cia2.set_flag();
    }

    // ── Time helpers ──────────────────────────────────────────

    pub fn get_time_ms(&self) -> u32 {
        let freq = self.cpu_frequency as u64;
        if freq == 0 {
            return 0;
        }
        (self.cycle_count * 1000 / freq) as u32
    }

    #[allow(dead_code)]
    fn cpu_read_internal(&self, addr: u16) -> u8 {
        let page = (addr >> 12) as usize;
        if page == 0 && addr < 2 {
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
            IoChip::Cia1 | IoChip::Cia2 => 0,
            IoChip::DisconnectedBus => self.disconnected_bus.peek(addr),
            IoChip::ExtraSid(_) => 0xFF,
        }
    }
}

impl Default for C64 {
    fn default() -> Self {
        Self::new()
    }
}

// ── mos6502 Bus implementation ────────────────────────────────

impl Bus for C64 {
    fn get_byte(&mut self, addr: u16) -> u8 {
        let page = (addr >> 12) as usize;

        if page == 0 && addr < 2 {
            return self.zero_ram.peek_mut(addr);
        }

        match self.mmu.read_map[page] {
            PageMapping::Ram => self.ram.peek(addr),
            PageMapping::BasicRom => self.basic_rom.peek(addr),
            PageMapping::KernalRom => self.kernal_rom.peek(addr),
            PageMapping::CharacterRom => self.char_rom.peek(addr),
            PageMapping::Io => match self.io_bank.dispatch(addr) {
                IoChip::Vic => self.vic.read((addr & 0x3F) as u8),
                IoChip::Sid => {
                    if self.extra_sid.has_slot(addr) {
                        self.extra_sid.peek(addr)
                    } else {
                        self.sid_bank.peek(addr)
                    }
                }
                IoChip::ColorRam => self.color_ram.peek(addr),
                IoChip::Cia1 => {
                    let (val, irq_delta) = self.cia1.read((addr & 0x0F) as u8);
                    if let Some(changed) = irq_delta {
                        self.irq_count += if changed { 1 } else { -1 };
                        if self.irq_count < 0 {
                            self.irq_count = 0;
                        }
                    }
                    val
                }
                IoChip::Cia2 => {
                    let (val, _irq_delta) = self.cia2.read((addr & 0x0F) as u8);
                    val
                }
                IoChip::DisconnectedBus => self.mmu.last_read_byte(),
                IoChip::ExtraSid(_) => self.extra_sid.peek(addr),
            },
        }
    }

    fn set_byte(&mut self, addr: u16, val: u8) {
        let page = (addr >> 12) as usize;

        if page == 0 {
            if addr < 2 {
                self.zero_ram.poke(addr, val);
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
                self.ram.poke(addr, val);
                match self.io_bank.dispatch(addr) {
                    IoChip::Vic => {
                        let out = self.vic.write((addr & 0x3F) as u8, val);
                        if let Some(changed) = out.irq {
                            self.irq_count += if changed { 1 } else { -1 };
                            if self.irq_count < 0 {
                                self.irq_count = 0;
                            }
                        }
                    }
                    IoChip::Sid => {
                        if self.extra_sid.has_slot(addr) {
                            self.extra_sid.poke(addr, val);
                        } else {
                            self.sid_bank.poke(addr, val);
                        }
                    }
                    IoChip::ColorRam => self.color_ram.poke(addr, val),
                    IoChip::Cia1 => {
                        let irq_delta = self.cia1.write((addr & 0x0F) as u8, val);
                        if let Some(changed) = irq_delta {
                            self.irq_count += if changed { 1 } else { -1 };
                            if self.irq_count < 0 {
                                self.irq_count = 0;
                            }
                        }
                    }
                    IoChip::Cia2 => {
                        let _irq_delta = self.cia2.write((addr & 0x0F) as u8, val);
                    }
                    IoChip::DisconnectedBus => {}
                    IoChip::ExtraSid(_) => self.extra_sid.poke(addr, val),
                }
            }
            _ => {
                self.ram.poke(addr, val);
            }
        }
    }

    fn irq_pending(&mut self) -> bool {
        self.irq_count > 0
    }

    fn nmi_pending(&mut self) -> bool {
        self.cia2.interrupt_asserted()
    }
}

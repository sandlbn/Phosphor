// C64 memory bus with SID write interception, CIA1/CIA2 timer emulation,
// VIC-II raster + badline emulation, memory banking ($0001),
// and proper KERNAL IRQ chain stubs for RSID support.

use mos6502::memory::Bus;

// ─────────────────────────────────────────────────────────────────────────────
//  SID address → USBSID register mapping
// ─────────────────────────────────────────────────────────────────────────────

pub const SID_REG_SIZE: u8 = 0x20;
pub const SID_VOL_REG: u8 = 0x18;

#[derive(Debug, Clone)]
pub struct SidMapper {
    ranges: Vec<(u16, u16)>,
}

impl SidMapper {
    pub fn new(bases: &[u16]) -> Self {
        let ranges = bases.iter().map(|&base| (base, base + 0x1F)).collect();
        Self { ranges }
    }

    pub fn map(&self, addr: u16) -> Option<u8> {
        for (slot, &(base, end)) in self.ranges.iter().enumerate() {
            if addr >= base && addr <= end {
                let reg_offset = (addr - base) as u8;
                return Some((slot as u8) * SID_REG_SIZE + reg_offset);
            }
        }
        None
    }

    pub fn num_sids(&self) -> usize {
        self.ranges.len()
    }

    #[allow(dead_code)]
    pub fn vol_reg(&self, sid: usize) -> u8 {
        (sid as u8) * SID_REG_SIZE + SID_VOL_REG
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  CIA Timer (shared by Timer A and Timer B in both CIAs)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CiaTimer {
    pub counter: u16,
    pub latch: u16,
    pub running: bool,
    pub oneshot: bool,
    pub underflow: bool,
}

impl CiaTimer {
    pub fn new() -> Self {
        Self {
            counter: 0xFFFF,
            latch: 0xFFFF,
            running: false,
            oneshot: false,
            underflow: false,
        }
    }

    /// Tick timer by `cycles` phi2 clocks. Returns number of underflows.
    pub fn tick(&mut self, cycles: u32) -> u32 {
        if !self.running {
            return 0;
        }

        let mut fires = 0u32;
        let mut remaining = cycles;

        while remaining > 0 && self.running {
            if remaining > self.counter as u32 {
                remaining -= self.counter as u32 + 1;
                self.underflow = true;
                fires += 1;

                if self.oneshot {
                    self.running = false;
                    self.counter = self.latch;
                } else {
                    self.counter = self.latch;
                }
            } else {
                self.counter -= remaining as u16;
                remaining = 0;
            }
        }
        fires
    }

    /// Tick timer by exactly 1 count (for Timer B chained to Timer A).
    pub fn tick_once(&mut self) -> bool {
        if !self.running {
            return false;
        }

        if self.counter == 0 {
            self.underflow = true;
            if self.oneshot {
                self.running = false;
            }
            self.counter = self.latch;
            true
        } else {
            self.counter -= 1;
            false
        }
    }

    pub fn write_lo(&mut self, value: u8) {
        self.latch = (self.latch & 0xFF00) | value as u16;
    }

    pub fn write_hi(&mut self, value: u8) {
        self.latch = (self.latch & 0x00FF) | ((value as u16) << 8);
        if !self.running {
            self.counter = self.latch;
        }
    }

    pub fn write_control(&mut self, value: u8) {
        let was_running = self.running;
        self.running = value & 0x01 != 0;
        self.oneshot = value & 0x08 != 0;
        if value & 0x10 != 0 {
            self.counter = self.latch;
        }
        if !was_running && self.running {
            self.counter = self.latch;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  CIA chip (shared structure for CIA1/IRQ and CIA2/NMI)
// ─────────────────────────────────────────────────────────────────────────────

/// BCD increment: increment a BCD byte, wrapping at max.
fn bcd_inc(val: u8, max: u8) -> u8 {
    let mut lo = val & 0x0F;
    let mut hi = val >> 4;
    lo += 1;
    if lo > 9 {
        lo = 0;
        hi += 1;
    }
    let result = (hi << 4) | lo;
    if result > max {
        0
    } else {
        result
    }
}

pub struct Cia {
    pub timer_a: CiaTimer,
    pub timer_b: CiaTimer,
    pub int_mask: u8,   // which sources can fire (bits 0-4)
    pub int_data: u8,   // which sources HAVE fired (bits 0-4)
    pub int_line: bool, // interrupt line asserted (latched until read)
    // Timer B input source: false = phi2 (normal), true = Timer A underflows
    timer_b_counts_a: bool,
    // Control register shadows (for read-back)
    cra: u8,
    crb: u8,
    // Data ports and DDR
    pub port_a: u8,
    pub port_b: u8,
    pub ddr_a: u8,
    pub ddr_b: u8,
    // External input lines (keyboard matrix / joystick for CIA1)
    // For SID player: all lines high = no input
    ext_a: u8,
    ext_b: u8,
    // PB6/PB7 timer output state (toggle mode)
    pb6_toggle: bool,
    pb7_toggle: bool,
    // TOD (simple free-running to prevent hangs)
    tod_10ths: u8,
    tod_sec: u8,
    tod_min: u8,
    tod_hr: u8,
    tod_tick: u32, // cycle accumulator for TOD advance
}

impl Cia {
    pub fn new() -> Self {
        Self {
            timer_a: CiaTimer::new(),
            timer_b: CiaTimer::new(),
            int_mask: 0,
            int_data: 0,
            int_line: false,
            timer_b_counts_a: false,
            cra: 0,
            crb: 0,
            port_a: 0xFF,
            port_b: 0xFF,
            ddr_a: 0x00,
            ddr_b: 0x00,
            ext_a: 0xFF, // no external input (all high)
            ext_b: 0xFF,
            pb6_toggle: true,
            pb7_toggle: true,
            tod_10ths: 0,
            tod_sec: 0,
            tod_min: 0,
            tod_hr: 0x01,
            tod_tick: 0,
        }
    }

    /// Pre-configure to RSID defaults for CIA1.
    pub fn setup_rsid_defaults(&mut self, is_pal: bool) {
        let latch = if is_pal { 0x4025 } else { 0x4295 };
        self.timer_a.latch = latch;
        self.timer_a.counter = latch;
        self.timer_a.running = true;
        self.timer_a.oneshot = false;
        self.cra = 0x01;
        self.int_mask = 0x01;
    }

    /// Tick both timers and TOD. Returns true if any enabled interrupt fired.
    pub fn tick(&mut self, cycles: u32) -> bool {
        // Timer A always counts phi2 cycles
        let a_fires = self.timer_a.tick(cycles);

        // Timer B: either counts phi2 or Timer A underflows
        let b_fired = if self.timer_b_counts_a {
            let mut fired = false;
            for _ in 0..a_fires {
                if self.timer_b.tick_once() {
                    fired = true;
                }
            }
            fired
        } else {
            self.timer_b.tick(cycles) > 0
        };

        if a_fires > 0 {
            self.int_data |= 0x01;
            // PB6 timer output: toggle on each underflow (if CRA bit 1 set)
            if self.cra & 0x02 != 0 {
                if self.cra & 0x04 != 0 {
                    // Toggle mode
                    for _ in 0..a_fires {
                        self.pb6_toggle = !self.pb6_toggle;
                    }
                } else {
                    // Pulse mode: one cycle high (we approximate as high)
                    self.pb6_toggle = true;
                }
            }
        }
        if b_fired {
            self.int_data |= 0x02;
            // PB7 timer output (CRB bit 1)
            if self.crb & 0x02 != 0 {
                if self.crb & 0x04 != 0 {
                    self.pb7_toggle = !self.pb7_toggle;
                } else {
                    self.pb7_toggle = true;
                }
            }
        }

        if self.int_data & self.int_mask != 0 {
            self.int_line = true;
        }

        // Simple TOD advance
        self.tod_tick += cycles;
        if self.tod_tick >= 100_000 {
            self.tod_tick -= 100_000;
            self.tod_10ths += 1;
            if self.tod_10ths >= 10 {
                self.tod_10ths = 0;
                self.tod_sec = bcd_inc(self.tod_sec, 0x59);
                if self.tod_sec == 0 {
                    self.tod_min = bcd_inc(self.tod_min, 0x59);
                    if self.tod_min == 0 {
                        self.tod_hr = bcd_inc(self.tod_hr, 0x12);
                    }
                }
            }
        }

        a_fires > 0 || b_fired
    }

    pub fn int_pending(&self) -> bool {
        self.int_line && (self.int_data & self.int_mask != 0)
    }

    /// Clear pending interrupt flags for timers that are no longer running.
    /// Called after INIT to prevent stale underflow flags from causing
    /// an IRQ flood (e.g., tune stopped CIA1 timer during INIT but didn't
    /// read $DC0D to acknowledge the underflow that occurred while running).
    pub fn clear_stale_ints(&mut self) {
        if !self.timer_a.running {
            self.int_data &= !0x01; // Clear Timer A underflow flag
            self.timer_a.underflow = false;
        }
        if !self.timer_b.running {
            self.int_data &= !0x02; // Clear Timer B underflow flag
            self.timer_b.underflow = false;
        }
        // Recalculate interrupt line
        if self.int_data & self.int_mask == 0 {
            self.int_line = false;
        }
    }

    pub fn write(&mut self, offset: u8, value: u8) {
        match offset {
            0x00 => self.port_a = value,
            0x01 => self.port_b = value,
            0x02 => self.ddr_a = value,
            0x03 => self.ddr_b = value,
            0x04 => self.timer_a.write_lo(value),
            0x05 => self.timer_a.write_hi(value),
            0x06 => self.timer_b.write_lo(value),
            0x07 => self.timer_b.write_hi(value),
            0x08 => self.tod_10ths = value,
            0x09 => self.tod_sec = value,
            0x0A => self.tod_min = value,
            0x0B => self.tod_hr = value,
            0x0D => {
                if value & 0x80 != 0 {
                    self.int_mask |= value & 0x1F;
                } else {
                    self.int_mask &= !(value & 0x1F);
                }
                if self.int_data & self.int_mask != 0 {
                    self.int_line = true;
                }
            }
            0x0E => {
                self.cra = value & 0xEF; // bit 4 is strobe
                self.timer_a.write_control(value);
            }
            0x0F => {
                self.crb = value & 0xEF;
                // Bit 6: Timer B input source (0=phi2, 1=Timer A underflows)
                self.timer_b_counts_a = value & 0x40 != 0;
                self.timer_b.write_control(value);
            }
            _ => {}
        }
    }

    pub fn read(&mut self, offset: u8) -> u8 {
        match offset {
            // Port reads: (port_output & ddr) | (external_input & ~ddr)
            0x00 => (self.port_a & self.ddr_a) | (self.ext_a & !self.ddr_a),
            0x01 => {
                let mut val = (self.port_b & self.ddr_b) | (self.ext_b & !self.ddr_b);
                // PB6 timer A output override (CRA bit 1)
                if self.cra & 0x02 != 0 {
                    if self.pb6_toggle {
                        val |= 0x40;
                    } else {
                        val &= !0x40;
                    }
                }
                // PB7 timer B output override (CRB bit 1)
                if self.crb & 0x02 != 0 {
                    if self.pb7_toggle {
                        val |= 0x80;
                    } else {
                        val &= !0x80;
                    }
                }
                val
            }
            0x02 => self.ddr_a,
            0x03 => self.ddr_b,
            0x04 => (self.timer_a.counter & 0xFF) as u8,
            0x05 => (self.timer_a.counter >> 8) as u8,
            0x06 => (self.timer_b.counter & 0xFF) as u8,
            0x07 => (self.timer_b.counter >> 8) as u8,
            0x08 => self.tod_10ths,
            0x09 => self.tod_sec,
            0x0A => self.tod_min,
            0x0B => self.tod_hr,
            0x0C => 0x00, // Serial data
            0x0D => {
                let mut val = self.int_data & 0x1F;
                if self.int_line {
                    val |= 0x80;
                }
                // Reading ICR clears all flags and deasserts interrupt line
                self.int_data = 0;
                self.int_line = false;
                val
            }
            0x0E => self.cra,
            0x0F => self.crb,
            _ => 0,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  VIC-II raster interrupt + badline emulation
// ─────────────────────────────────────────────────────────────────────────────
//
// Badlines: when DEN=1, raster in display area (0x30..0xF7), and
// (raster & 7) == (YSCROLL & 7), VIC steals ~40 cycles from CPU.

const FIRST_DMA_LINE: u16 = 0x30;
const LAST_DMA_LINE: u16 = 0xF7;
const BADLINE_STEAL_CYCLES: u32 = 40;

#[derive(Debug, Clone)]
pub struct Vic {
    pub raster_counter: u16,
    pub raster_compare: u16,
    pub raster_irq_enabled: bool,
    pub irq_status: u8,
    pub cycle_accum: u32,
    pub cycles_per_line: u32,
    pub lines_per_frame: u16,
    pub d011: u8,
    pub irq_line: bool,
    raster_triggered: bool,
    /// Cycles stolen by VIC badlines this tick (read by emulation loop)
    pub stolen_cycles: u32,
    /// Set when raster wraps to 0 (new frame started). Cleared by caller.
    pub new_frame: bool,
    /// Shadow registers for all 64 VIC registers (read-back support)
    regs: [u8; 64],
}

impl Vic {
    pub fn new(is_pal: bool) -> Self {
        let mut regs = [0u8; 64];
        regs[0x11] = 0x1B; // $D011 default
        regs[0x16] = 0xC8; // $D016 default
        regs[0x18] = 0x14; // $D018 default (screen $0400, charset $1000)
        regs[0x19] = 0x00; // $D019 no IRQs pending
        regs[0x1A] = 0x00; // $D01A no IRQs enabled
        regs[0x20] = 0x0E; // border color: light blue
        regs[0x21] = 0x06; // background: blue
        Self {
            raster_counter: 0,
            raster_compare: 0x137,
            raster_irq_enabled: false,
            irq_status: 0,
            cycle_accum: 0,
            cycles_per_line: if is_pal { 63 } else { 65 },
            lines_per_frame: if is_pal { 312 } else { 263 },
            d011: 0x1B,
            irq_line: false,
            raster_triggered: false,
            stolen_cycles: 0,
            new_frame: false,
            regs,
        }
    }

    fn is_badline(&self, line: u16) -> bool {
        let den = self.d011 & 0x10 != 0;
        let yscroll = (self.d011 & 0x07) as u16;
        den && line >= FIRST_DMA_LINE && line <= LAST_DMA_LINE && (line & 7) == (yscroll & 7)
    }

    pub fn tick(&mut self, cycles: u32) -> bool {
        self.cycle_accum += cycles;
        self.stolen_cycles = 0;
        let mut fired = false;

        while self.cycle_accum >= self.cycles_per_line {
            self.cycle_accum -= self.cycles_per_line;
            self.raster_counter += 1;

            if self.raster_counter >= self.lines_per_frame {
                self.raster_counter = 0;
                self.new_frame = true;
            }

            // Badline — steal cycles from CPU
            if self.is_badline(self.raster_counter) {
                self.stolen_cycles += BADLINE_STEAL_CYCLES;
            }

            if self.raster_counter != self.raster_compare {
                self.raster_triggered = false;
            }

            if self.raster_counter == self.raster_compare && !self.raster_triggered {
                self.raster_triggered = true;
                self.irq_status |= 0x01;

                if self.raster_irq_enabled {
                    self.irq_status |= 0x80;
                    self.irq_line = true;
                    fired = true;
                }
            }
        }
        fired
    }

    pub fn read(&self, offset: u16) -> u8 {
        let off = (offset & 0x3F) as usize;
        match off {
            0x11 => {
                let raster_hi = if self.raster_counter > 255 {
                    0x80
                } else {
                    0x00
                };
                (self.d011 & 0x7F) | raster_hi
            }
            0x12 => (self.raster_counter & 0xFF) as u8,
            0x19 => self.irq_status,
            0x1A => {
                if self.raster_irq_enabled {
                    0x01
                } else {
                    0x00
                }
            }
            0x1E | 0x1F => 0, // Sprite collision (auto-clear on read)
            _ => self.regs[off],
        }
    }

    pub fn write(&mut self, offset: u16, value: u8) {
        let off = (offset & 0x3F) as usize;
        // Store all writes for read-back
        if off < 0x20 || (off >= 0x20 && off <= 0x2E) {
            self.regs[off] = value;
        }
        match off {
            0x11 => {
                self.d011 = value;
                self.raster_compare =
                    (self.raster_compare & 0x00FF) | (((value as u16) & 0x80) << 1);
                self.regs[off] = value;
            }
            0x12 => {
                self.raster_compare = (self.raster_compare & 0x0100) | value as u16;
                self.regs[off] = value;
            }
            0x19 => {
                self.irq_status &= !value;
                if self.irq_status & 0x0F == 0 {
                    self.irq_status &= !0x80;
                    self.irq_line = false;
                }
            }
            0x1A => {
                self.raster_irq_enabled = value & 0x01 != 0;
                self.regs[off] = value;
                if self.raster_irq_enabled && (self.irq_status & 0x01 != 0) {
                    self.irq_status |= 0x80;
                    self.irq_line = true;
                }
            }
            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Approximate 6502 cycle counts per opcode
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

/// Read an opcode byte through the banking layer (same view as CPU).
/// This is critical for correct cycle counting when code executes in
/// KERNAL ROM ($E000-$FFFF) — raw RAM may contain tune data there,
/// but the CPU sees kernal_rom stubs instead.
pub fn opcode_cycles_banked(mem: &C64Memory, pc: u16) -> u32 {
    let port = mem.ram[0x0001];
    let byte = if pc >= 0xE000 && kernal_visible(port) {
        mem.kernal_rom[(pc - 0xE000) as usize]
    } else if pc >= 0xD000 && pc <= 0xDFFF && io_visible(port) {
        // Code executing in I/O area — shouldn't normally happen,
        // but return RAM as fallback
        mem.ram[pc as usize]
    } else {
        mem.ram[pc as usize]
    };
    OPCODE_CYCLES[byte as usize] as u32
}

// ─────────────────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────────────────

pub const PAL_CYCLES_PER_FRAME: u32 = 19705;
pub const NTSC_CYCLES_PER_FRAME: u32 = 17045;

/// A SID register write with cycle timestamp for accurate replay timing.
pub type SidWrite = (u32, u8, u8);

// ─────────────────────────────────────────────────────────────────────────────
//  C64 memory bus — with banking via processor port ($0001)
// ─────────────────────────────────────────────────────────────────────────────

pub struct C64Memory {
    pub ram: [u8; 65536],
    /// KERNAL ROM overlay for $E000-$FFFF (8 KiB)
    kernal_rom: Box<[u8; 8192]>,
    pub sid_writes: Vec<SidWrite>,
    mapper: SidMapper,
    mono: bool,
    pub sid_shadow: [u8; 128],
    pub cia1: Cia,
    pub cia2: Cia,
    pub vic: Vic,
    sid_osc3: u32,
    pub frame_cycle: u32,
}

/// Check if I/O is visible at $D000-$DFFF.
#[inline]
fn io_visible(port: u8) -> bool {
    let loram = port & 0x01 != 0;
    let hiram = port & 0x02 != 0;
    let charen = port & 0x04 != 0;
    (loram || hiram) && charen
}

/// Check if KERNAL ROM is visible at $E000-$FFFF.
#[inline]
fn kernal_visible(port: u8) -> bool {
    port & 0x02 != 0
}

impl C64Memory {
    pub fn new(is_pal: bool, mapper: SidMapper, mono: bool) -> Self {
        let mut ram = [0u8; 65536];
        ram[0x0000] = 0x2F; // DDR: bits 0-2,5 output
        ram[0x0001] = 0x37; // BASIC+KERNAL+I/O visible

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

        ram[0xD018] = 0x15;
        ram[0xD020] = 0x0E;
        ram[0xD021] = 0x06;
        ram[0xD011] = 0x1B;

        ram[0xDC02] = 0xFF;
        ram[0xDC03] = 0x00;
        ram[0xDD02] = 0x3F;
        ram[0xDD03] = 0x00;
        ram[0xDD00] = 0x17;

        install_kernal_stubs(&mut ram);

        // Copy KERNAL area into ROM overlay (separate from RAM)
        let mut kernal_rom = Box::new([0u8; 8192]);
        kernal_rom.copy_from_slice(&ram[0xE000..0x10000]);

        let mut cia1 = Cia::new();
        cia1.ddr_a = 0xFF;
        cia1.ddr_b = 0x00;

        let mut cia2 = Cia::new();
        cia2.ddr_a = 0x3F;
        cia2.ddr_b = 0x00;
        cia2.port_a = 0x17;

        Self {
            ram,
            kernal_rom,
            sid_writes: Vec::with_capacity(256),
            mapper,
            mono,
            sid_shadow: [0u8; 128],
            cia1,
            cia2,
            vic: Vic::new(is_pal),
            sid_osc3: 0x12345678,
            frame_cycle: 0,
        }
    }

    pub fn load(&mut self, addr: u16, data: &[u8]) {
        let a = addr as usize;
        let end = (a + data.len()).min(65536);
        self.ram[a..end].copy_from_slice(&data[..end - a]);
    }

    /// Rebuild kernal_rom overlay from current RAM content, then re-install
    /// our KERNAL stubs on top. MUST be called after load() when tune data
    /// may overlap $E000-$FFFF so that tune code/data is visible through
    /// the banking layer while KERNAL entry points still work.
    pub fn rebuild_kernal_rom(&mut self) {
        self.kernal_rom.copy_from_slice(&self.ram[0xE000..0x10000]);
        install_kernal_stubs_rom(&mut self.kernal_rom);
    }

    pub fn install_trampoline(&mut self, at: u16, target: u16) {
        let a = at as usize;
        self.ram[a] = 0x20;
        self.ram[a + 1] = (target & 0xFF) as u8;
        self.ram[a + 2] = (target >> 8) as u8;
        self.ram[a + 3] = 0x4C;
        self.ram[a + 4] = ((at + 3) & 0xFF) as u8;
        self.ram[a + 5] = ((at + 3) >> 8) as u8;
    }

    pub fn clear_writes(&mut self) {
        self.sid_writes.clear();
        self.frame_cycle = 0;
    }

    /// Increment the KERNAL jiffy clock at $00A0-$00A2.
    /// On a real C64, the KERNAL IRQ handler does this every 1/60th sec.
    /// Many RSID tunes poll $00A2 for timing without setting up their own
    /// interrupts. Call this whenever VIC signals a new frame.
    pub fn tick_jiffy_clock(&mut self) {
        let a2 = self.ram[0x00A2].wrapping_add(1);
        self.ram[0x00A2] = a2;
        if a2 == 0 {
            let a1 = self.ram[0x00A1].wrapping_add(1);
            self.ram[0x00A1] = a1;
            if a1 == 0 {
                self.ram[0x00A0] = self.ram[0x00A0].wrapping_add(1);
            }
        }
    }

    pub fn mapper(&self) -> &SidMapper {
        &self.mapper
    }

    /// Set a hardware vector address, updating both RAM and KERNAL ROM overlay.
    /// Used for $FFFA (NMI), $FFFE (IRQ) etc.
    pub fn set_hw_vector(&mut self, addr: u16, value: u16) {
        self.ram[addr as usize] = (value & 0xFF) as u8;
        self.ram[addr as usize + 1] = (value >> 8) as u8;
        if addr >= 0xE000 {
            let off = (addr - 0xE000) as usize;
            self.kernal_rom[off] = (value & 0xFF) as u8;
            self.kernal_rom[off + 1] = (value >> 8) as u8;
        }
    }

    /// Read byte through banking layer. Used by deliver_irq/deliver_nmi
    /// for vector fetches through the address bus.
    pub fn banked_read(&mut self, address: u16) -> u8 {
        self.get_byte(address)
    }

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
}

impl Bus for C64Memory {
    fn get_byte(&mut self, address: u16) -> u8 {
        let port = self.ram[0x0001];

        match address {
            0x0000 => self.ram[0x0000],
            0x0001 => port,

            // $A000-$BFFF: BASIC ROM when LORAM=1 AND HIRAM=1
            // We don't carry BASIC ROM — just return RAM (tunes don't use it)
            0xA000..=0xBFFF => self.ram[address as usize],

            // $D000-$DFFF: I/O / Char ROM / RAM depending on banking
            0xD000..=0xDFFF => {
                let loram = port & 0x01 != 0;
                let hiram = port & 0x02 != 0;
                let charen = port & 0x04 != 0;

                if !loram && !hiram {
                    // Both low → pure RAM
                    return self.ram[address as usize];
                }
                if !charen {
                    // Char ROM — return RAM (we don't carry char ROM data)
                    return self.ram[address as usize];
                }

                // I/O visible
                match address {
                    // VIC-II: all $D000-$D3FF reads go through VIC
                    0xD000..=0xD3FF => self.vic.read(address - 0xD000),
                    // SID ($D400-$D7FF)
                    0xD400..=0xD7FF => match (address & 0x1F) as u8 {
                        0x1B => {
                            self.sid_osc3 =
                                self.sid_osc3.wrapping_mul(1103515245).wrapping_add(12345);
                            (self.sid_osc3 >> 16) as u8
                        }
                        0x1C => 0xFF,
                        0x19 => 0x80,
                        0x1A => 0x80,
                        _ => 0,
                    },
                    // Color RAM
                    0xD800..=0xDBFF => self.ram[address as usize],
                    // CIA1
                    0xDC00..=0xDCFF => self.cia1.read(((address - 0xDC00) & 0x0F) as u8),
                    // CIA2
                    0xDD00..=0xDDFF => self.cia2.read(((address - 0xDD00) & 0x0F) as u8),
                    // Expansion I/O
                    _ => self.ram[address as usize],
                }
            }

            // $E000-$FFFF: KERNAL ROM or RAM
            0xE000..=0xFFFF => {
                if kernal_visible(port) {
                    self.kernal_rom[(address - 0xE000) as usize]
                } else {
                    self.ram[address as usize]
                }
            }

            _ => self.ram[address as usize],
        }
    }

    fn set_byte(&mut self, address: u16, value: u8) {
        // Writes ALWAYS go to underlying RAM
        self.ram[address as usize] = value;

        match address {
            0x0000 | 0x0001 => {} // processor port stored in RAM

            0xD000..=0xDFFF => {
                if !io_visible(self.ram[0x0001]) {
                    return; // I/O banked out, write goes to RAM only
                }
                match address {
                    // VIC-II: all $D000-$D3FF writes go through VIC
                    0xD000..=0xD3FF => {
                        self.vic.write(address - 0xD000, value);
                    }
                    0xDC00..=0xDCFF => {
                        self.cia1.write(((address - 0xDC00) & 0x0F) as u8, value);
                    }
                    0xDD00..=0xDDFF => {
                        self.cia2.write(((address - 0xDD00) & 0x0F) as u8, value);
                    }
                    _ => {
                        if self.mono {
                            if address >= 0xD400 && address <= 0xD7FF {
                                let reg = (address as u8) & 0x1F;
                                self.sid_writes.push((self.frame_cycle, reg, value));
                                self.sid_shadow[reg as usize] = value;
                            }
                        } else if let Some(reg) = self.mapper.map(address) {
                            self.sid_writes.push((self.frame_cycle, reg, value));
                            self.sid_shadow[reg as usize] = value;
                        }
                    }
                }
            }

            // Writes to KERNAL area: sync into kernal_rom overlay so that
            // custom hardware vectors ($FFFA-$FFFF) and any code the tune
            // installs in $E000-$FFFF are visible through the banking layer.
            0xE000..=0xFFFF => {
                self.kernal_rom[(address - 0xE000) as usize] = value;
            }

            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  KERNAL stubs
// ─────────────────────────────────────────────────────────────────────────────

/// Install KERNAL stubs into the ROM overlay (offset from $E000).
/// This is called by rebuild_kernal_rom() after copying RAM content,
/// so tune data at $E000+ is preserved except at our stub addresses.
fn install_kernal_stubs_rom(rom: &mut [u8; 8192]) {
    // Helper: write to rom at a $E000-based absolute address
    macro_rules! rom_set {
        ($addr:expr, $val:expr) => {
            rom[($addr as usize) - 0xE000] = $val;
        };
    }

    // RTS stubs for KERNAL entry points
    let rts_stubs: &[u16] = &[
        0xFF81, 0xFF84, 0xFF87, 0xFF8A, 0xFF8D, 0xFF90, 0xFF93, 0xFF96, 0xFF99, 0xFF9C, 0xFFA5,
        0xFFB1, 0xFFB4, 0xFFBD, 0xFFC0, 0xFFC3, 0xFFC6, 0xFFC9, 0xFFCC, 0xFFCF, 0xFFD2, 0xFFD5,
        0xFFD8, 0xFFDB, 0xFFDE, 0xFFE7,
    ];
    for &addr in rts_stubs {
        rom_set!(addr, 0x60);
    }

    // $FFE4 (GETIN) — LDA #$00; CLC; (RTS at $FFE7 above)
    rom_set!(0xFFE4, 0xA9);
    rom_set!(0xFFE5, 0x00);
    rom_set!(0xFFE6, 0x18);
    // $FFE1 (STOP) — CLC; RTS
    rom_set!(0xFFE1, 0x18);
    rom_set!(0xFFE2, 0x60);
    // $E544 (CLRSCR) — RTS
    rom_set!(0xE544, 0x60);

    // $FF48: KERNAL IRQ entry
    let kernal_irq: [u8; 19] = [
        0x48, 0x8A, 0x48, 0x98, 0x48, 0xBA, 0xBD, 0x04, 0x01, 0x29, 0x10, 0xD0, 0x03, 0x6C, 0x14,
        0x03, 0x6C, 0x16, 0x03,
    ];
    rom[0xFF48 - 0xE000..0xFF48 - 0xE000 + 19].copy_from_slice(&kernal_irq);

    // $EA31: Default IRQ handler — ack CIA1 + ack all VIC IRQs + bump jiffy clock
    // Must ack BOTH sources, otherwise unacknowledged VIC raster IRQ
    // causes an infinite IRQ flood.
    // LDA $DC0D; LDA #$FF; STA $D019; INC $00A2; JMP $EA81
    rom_set!(0xEA31, 0xAD);
    rom_set!(0xEA32, 0x0D);
    rom_set!(0xEA33, 0xDC); // LDA $DC0D
    rom_set!(0xEA34, 0xA9);
    rom_set!(0xEA35, 0xFF); // LDA #$FF
    rom_set!(0xEA36, 0x8D);
    rom_set!(0xEA37, 0x19);
    rom_set!(0xEA38, 0xD0); // STA $D019
    rom_set!(0xEA39, 0xEE);
    rom_set!(0xEA3A, 0xA2);
    rom_set!(0xEA3B, 0x00); // INC $00A2
    rom_set!(0xEA3C, 0x4C);
    rom_set!(0xEA3D, 0x81);
    rom_set!(0xEA3E, 0xEA); // JMP $EA81

    // $EA81: IRQ exit — PLA; TAY; PLA; TAX; PLA; RTI
    rom_set!(0xEA81, 0x68);
    rom_set!(0xEA82, 0xA8);
    rom_set!(0xEA83, 0x68);
    rom_set!(0xEA84, 0xAA);
    rom_set!(0xEA85, 0x68);
    rom_set!(0xEA86, 0x40);

    // $FE43: KERNAL NMI entry
    let kernal_nmi: [u8; 8] = [0x48, 0x8A, 0x48, 0x98, 0x48, 0x6C, 0x18, 0x03];
    rom[0xFE43 - 0xE000..0xFE43 - 0xE000 + 8].copy_from_slice(&kernal_nmi);

    // $FE72: Default NMI handler — LDA $DD0D; JMP $EA81
    rom_set!(0xFE72, 0xAD);
    rom_set!(0xFE73, 0x0D);
    rom_set!(0xFE74, 0xDD);
    rom_set!(0xFE75, 0x4C);
    rom_set!(0xFE76, 0x81);
    rom_set!(0xFE77, 0xEA);

    // Hardware interrupt vectors
    rom_set!(0xFFFA, 0x43);
    rom_set!(0xFFFB, 0xFE); // NMI → $FE43
    rom_set!(0xFFFC, 0x00);
    rom_set!(0xFFFD, 0xE0); // RESET
    rom_set!(0xFFFE, 0x48);
    rom_set!(0xFFFF, 0xFF); // IRQ → $FF48
}

/// Install KERNAL stubs into RAM and set up software vectors.
fn install_kernal_stubs(ram: &mut [u8; 65536]) {
    let rts_stubs: &[u16] = &[
        0xFF81, 0xFF84, 0xFF87, 0xFF8A, 0xFF8D, 0xFF90, 0xFF93, 0xFF96, 0xFF99, 0xFF9C, 0xFFA5,
        0xFFB1, 0xFFB4, 0xFFBD, 0xFFC0, 0xFFC3, 0xFFC6, 0xFFC9, 0xFFCC, 0xFFCF, 0xFFD2, 0xFFD5,
        0xFFD8, 0xFFDB, 0xFFDE, 0xFFE7,
    ];
    for &addr in rts_stubs {
        ram[addr as usize] = 0x60;
    }

    ram[0xFFE4] = 0xA9;
    ram[0xFFE5] = 0x00;
    ram[0xFFE6] = 0x18;
    ram[0xFFE1] = 0x18;
    ram[0xFFE2] = 0x60;
    ram[0xE544] = 0x60;

    let kernal_irq: [u8; 19] = [
        0x48, 0x8A, 0x48, 0x98, 0x48, 0xBA, 0xBD, 0x04, 0x01, 0x29, 0x10, 0xD0, 0x03, 0x6C, 0x14,
        0x03, 0x6C, 0x16, 0x03,
    ];
    ram[0xFF48..0xFF48 + 19].copy_from_slice(&kernal_irq);

    // $EA31: Default IRQ handler — ack CIA1 + ack VIC + bump jiffy
    ram[0xEA31] = 0xAD;
    ram[0xEA32] = 0x0D;
    ram[0xEA33] = 0xDC; // LDA $DC0D
    ram[0xEA34] = 0xA9;
    ram[0xEA35] = 0xFF; // LDA #$FF
    ram[0xEA36] = 0x8D;
    ram[0xEA37] = 0x19;
    ram[0xEA38] = 0xD0; // STA $D019
    ram[0xEA39] = 0xEE;
    ram[0xEA3A] = 0xA2;
    ram[0xEA3B] = 0x00; // INC $00A2
    ram[0xEA3C] = 0x4C;
    ram[0xEA3D] = 0x81;
    ram[0xEA3E] = 0xEA; // JMP $EA81

    ram[0xEA81] = 0x68;
    ram[0xEA82] = 0xA8;
    ram[0xEA83] = 0x68;
    ram[0xEA84] = 0xAA;
    ram[0xEA85] = 0x68;
    ram[0xEA86] = 0x40;

    let kernal_nmi: [u8; 8] = [0x48, 0x8A, 0x48, 0x98, 0x48, 0x6C, 0x18, 0x03];
    ram[0xFE43..0xFE43 + 8].copy_from_slice(&kernal_nmi);

    ram[0xFE72] = 0xAD;
    ram[0xFE73] = 0x0D;
    ram[0xFE74] = 0xDD;
    ram[0xFE75] = 0x4C;
    ram[0xFE76] = 0x81;
    ram[0xFE77] = 0xEA;

    ram[0x0314] = 0x31;
    ram[0x0315] = 0xEA;
    ram[0x0316] = 0x81;
    ram[0x0317] = 0xEA;
    ram[0x0318] = 0x72;
    ram[0x0319] = 0xFE;

    // Hardware interrupt vectors (these must be in KERNAL ROM overlay too)
    // $FFFA/$FFFB: NMI → $FE43 (KERNAL NMI entry)
    ram[0xFFFA] = 0x43;
    ram[0xFFFB] = 0xFE;
    // $FFFC/$FFFD: RESET (not used but set for completeness)
    ram[0xFFFC] = 0x00;
    ram[0xFFFD] = 0xE0;
    // $FFFE/$FFFF: IRQ → $FF48 (KERNAL IRQ entry)
    ram[0xFFFE] = 0x48;
    ram[0xFFFF] = 0xFF;
}

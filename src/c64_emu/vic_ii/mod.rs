//! MOS 6567/6569/6572/6573 (VIC-II) emulation.
//!
//! Not cycle-exact pixel rendering but accurate enough for SID playback:
//! raster IRQs, bad-line detection, sprite DMA, lightpen.

pub mod lightpen;
pub mod sprites;

use lightpen::Lightpen;
use sprites::Sprites;

// ── Model data ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VicModel {
    Mos6567R56A, // Old NTSC
    Mos6567R8,   // NTSC-M
    Mos6569,     // PAL-B
    Mos6572,     // PAL-N
    Mos6573,     // PAL-M
}

struct ModelData {
    raster_lines: u32,
    cycles_per_line: u32,
}

const MODEL_DATA: [ModelData; 5] = [
    ModelData {
        raster_lines: 262,
        cycles_per_line: 64,
    }, // Old NTSC
    ModelData {
        raster_lines: 263,
        cycles_per_line: 65,
    }, // NTSC-M
    ModelData {
        raster_lines: 312,
        cycles_per_line: 63,
    }, // PAL-B
    ModelData {
        raster_lines: 312,
        cycles_per_line: 65,
    }, // PAL-N
    ModelData {
        raster_lines: 263,
        cycles_per_line: 65,
    }, // PAL-M
];

// ── IRQ flags ─────────────────────────────────────────────────

const IRQ_RASTER: u8 = 1 << 0;
const IRQ_LIGHTPEN: u8 = 1 << 3;

const FIRST_DMA_LINE: u32 = 0x30;
const LAST_DMA_LINE: u32 = 0xF7;
const FETCH_CYCLE: u32 = 11;
const SCREEN_TEXTCOLS: u32 = 40;

// ── MOS656X ───────────────────────────────────────────────────

/// VIC-II chip state.
///
/// Call `tick()` once per PHI2 cycle.  The chip will report IRQ
/// and BA (bus-available) state changes via the returned `VicOutput`.
pub struct Mos656x {
    pub regs: [u8; 0x40],

    model: VicModel,
    cycles_per_line: u32,
    max_rasters: u32,

    line_cycle: u32,
    raster_y: u32,
    yscroll: u32,

    are_bad_lines_enabled: bool,
    is_bad_line: bool,
    raster_y_irq_condition: bool,
    vblanking: bool,
    lp_asserted: bool,

    irq_flags: u8,
    irq_mask: u8,

    lp: Lightpen,
    sprites: Sprites,

    /// Accumulated IRQ assertion state.
    pub irq_state: bool,
    /// Accumulated BA state.
    ba_state: bool,
    /// Set when raster wraps to line 0 (new frame). Cleared by caller.
    pub new_frame: bool,
}

/// Output of a single VIC tick.
#[derive(Debug, Clone, Copy)]
pub struct VicOutput {
    /// `Some(true)` = assert IRQ, `Some(false)` = deassert.
    pub irq: Option<bool>,
    /// `Some(true)` = BA high (CPU can run), `Some(false)` = BA low (CPU halted).
    pub ba: Option<bool>,
}

impl Mos656x {
    pub fn new() -> Self {
        let mut vic = Self {
            regs: [0; 0x40],
            model: VicModel::Mos6569,
            cycles_per_line: 63,
            max_rasters: 312,
            line_cycle: 0,
            raster_y: 311,
            yscroll: 0,
            are_bad_lines_enabled: false,
            is_bad_line: false,
            raster_y_irq_condition: false,
            vblanking: false,
            lp_asserted: false,
            irq_flags: 0,
            irq_mask: 0,
            lp: Lightpen::new(),
            sprites: Sprites::new(),
            irq_state: false,
            ba_state: true,
            new_frame: false,
        };
        vic.lp.set_screen_size(312, 63);
        vic
    }

    pub fn chip(&mut self, model: VicModel) {
        let md = &MODEL_DATA[model as usize];
        self.model = model;
        self.max_rasters = md.raster_lines;
        self.cycles_per_line = md.cycles_per_line;
        self.lp.set_screen_size(md.raster_lines, md.cycles_per_line);
        self.reset();
    }

    pub fn reset(&mut self) {
        self.irq_flags = 0;
        self.irq_mask = 0;
        self.yscroll = 0;
        self.raster_y = self.max_rasters - 1;
        self.line_cycle = 0;
        self.are_bad_lines_enabled = false;
        self.is_bad_line = false;
        self.raster_y_irq_condition = false;
        self.vblanking = false;
        self.lp_asserted = false;
        self.regs.fill(0);
        self.lp.reset();
        self.sprites.reset();
        self.irq_state = false;
        self.ba_state = true;
        self.new_frame = false;
    }

    // ── Register access ───────────────────────────────────────

    pub fn read(&self, addr: u8) -> u8 {
        let addr = (addr & 0x3F) as usize;
        match addr {
            0x11 => (self.regs[0x11] & 0x7F) | (((self.raster_y & 0x100) >> 1) as u8),
            0x12 => (self.raster_y & 0xFF) as u8,
            0x13 => self.lp.get_x(),
            0x14 => self.lp.get_y(),
            0x19 => self.irq_flags | 0x70,
            0x1A => self.irq_mask | 0xF0,
            _ if addr < 0x20 => self.regs[addr],
            _ if addr < 0x2F => self.regs[addr] | 0xF0,
            _ => 0xFF,
        }
    }

    /// Write a VIC register.  Returns a `VicOutput` with any immediate
    /// IRQ / BA changes.
    pub fn write(&mut self, addr: u8, data: u8) -> VicOutput {
        let a = (addr & 0x3F) as usize;
        self.regs[a] = data;
        let mut out = VicOutput {
            irq: None,
            ba: None,
        };

        match a {
            0x11 => {
                let old_yscroll = self.yscroll;
                self.yscroll = (data & 0x07) as u32;

                // Bad line trick handling
                if self.raster_y == FIRST_DMA_LINE && self.line_cycle == 0 {
                    self.are_bad_lines_enabled = self.read_den();
                }
                if self.old_raster_y() == FIRST_DMA_LINE && self.read_den() {
                    self.are_bad_lines_enabled = true;
                }

                if (old_yscroll != self.yscroll || true)
                    && self.raster_y >= FIRST_DMA_LINE
                    && self.raster_y <= LAST_DMA_LINE
                {
                    let was_bad = self.are_bad_lines_enabled && old_yscroll == (self.raster_y & 7);
                    let now_bad = self.are_bad_lines_enabled && self.yscroll == (self.raster_y & 7);
                    if now_bad != was_bad {
                        if now_bad && self.line_cycle <= FETCH_CYCLE + SCREEN_TEXTCOLS + 6 {
                            self.is_bad_line = true;
                        } else if was_bad && self.line_cycle < FETCH_CYCLE {
                            self.is_bad_line = false;
                        }
                        out.ba = Some(!self.is_bad_line);
                    }
                }

                // Edge-detect raster IRQ
                self.raster_y_irq_edge_detect();
                out.irq = self.handle_irq_state();
            }
            0x12 => {
                self.raster_y_irq_edge_detect();
                out.irq = self.handle_irq_state();
            }
            0x17 => {
                self.sprites.line_crunch(data, self.line_cycle);
            }
            0x19 => {
                // Acknowledge IRQ flags
                self.irq_flags &= (!data & 0x0F) | 0x80;
                out.irq = self.handle_irq_state();
            }
            0x1A => {
                self.irq_mask = data & 0x0F;
                out.irq = self.handle_irq_state();
            }
            _ => {}
        }
        out
    }

    // ── Tick ──────────────────────────────────────────────────

    /// Advance one PHI2 cycle.
    pub fn tick(&mut self) -> VicOutput {
        self.line_cycle += 1;
        if self.line_cycle >= self.cycles_per_line {
            self.line_cycle = 0;
        }

        let mut out = VicOutput {
            irq: None,
            ba: None,
        };

        // Beginning of line
        if self.line_cycle == 0 {
            self.check_vblank();
            out.irq = self.handle_irq_state();
        }
        if self.line_cycle == 1 {
            self.vblank();
        }

        // Bad line → BA low
        if self.line_cycle == FETCH_CYCLE && self.is_bad_line {
            self.ba_state = false;
            out.ba = Some(false);
        }

        // Sprite DMA checks
        if self.line_cycle == 14 {
            self.sprites.update_mc();
        }
        if self.line_cycle == 15 {
            self.sprites.update_mc_base();
        }
        if self.line_cycle == 55 || self.line_cycle == 56 {
            self.sprites.check_dma(self.raster_y, &self.regs);
            if self.line_cycle == 55 {
                self.sprites.check_exp();
            }
        }
        if self.line_cycle == 58 {
            self.sprites.check_display();
        }

        // End of bad-line fetch period
        if self.line_cycle == FETCH_CYCLE + SCREEN_TEXTCOLS + 3 && self.is_bad_line {
            if !self.sprites.is_dma(0x01) {
                self.ba_state = true;
                out.ba = Some(true);
            }
        }

        out
    }

    // ── Lightpen ──────────────────────────────────────────────

    /// Check if raster IRQ is enabled in the mask register.
    pub fn irq_mask_has_raster(&self) -> bool {
        self.irq_mask & 0x01 != 0
    }

    pub fn trigger_lightpen(&mut self) {
        self.lp_asserted = true;
        if self.lp.trigger(self.line_cycle, self.raster_y) {
            self.irq_flags |= IRQ_LIGHTPEN;
        }
    }

    pub fn clear_lightpen(&mut self) {
        self.lp_asserted = false;
    }

    // ── Internals ─────────────────────────────────────────────

    fn read_raster_line_irq(&self) -> u32 {
        self.regs[0x12] as u32 + (((self.regs[0x11] & 0x80) as u32) << 1)
    }

    fn read_den(&self) -> bool {
        (self.regs[0x11] & 0x10) != 0
    }

    fn evaluate_is_bad_line(&self) -> bool {
        self.are_bad_lines_enabled
            && self.raster_y >= FIRST_DMA_LINE
            && self.raster_y <= LAST_DMA_LINE
            && (self.raster_y & 7) == self.yscroll
    }

    fn old_raster_y(&self) -> u32 {
        if self.raster_y > 0 {
            self.raster_y - 1
        } else {
            self.max_rasters - 1
        }
    }

    fn raster_y_irq_edge_detect(&mut self) {
        let old = self.raster_y_irq_condition;
        self.raster_y_irq_condition = self.raster_y == self.read_raster_line_irq();
        if !old && self.raster_y_irq_condition {
            self.irq_flags |= IRQ_RASTER;
        }
    }

    fn handle_irq_state(&mut self) -> Option<bool> {
        let new_state = (self.irq_flags & self.irq_mask & 0x0F) != 0;
        if new_state && !self.irq_state {
            self.irq_flags |= 0x80;
            self.irq_state = true;
            return Some(true);
        }
        if !new_state && self.irq_state {
            self.irq_flags &= 0x7F;
            self.irq_state = false;
            return Some(false);
        }
        None
    }

    fn check_vblank(&mut self) {
        if self.raster_y == self.max_rasters - 1 {
            self.vblanking = true;
        }
        if self.raster_y == FIRST_DMA_LINE && !self.are_bad_lines_enabled && self.read_den() {
            self.are_bad_lines_enabled = true;
        }
        if self.raster_y == LAST_DMA_LINE {
            self.are_bad_lines_enabled = false;
        }

        self.is_bad_line = false;
        if !self.vblanking {
            self.raster_y += 1;
            self.raster_y_irq_edge_detect();
            if self.raster_y == FIRST_DMA_LINE && !self.are_bad_lines_enabled {
                self.are_bad_lines_enabled = self.read_den();
            }
        }
        if self.evaluate_is_bad_line() {
            self.is_bad_line = true;
        }
    }

    fn vblank(&mut self) {
        if self.vblanking {
            self.vblanking = false;
            self.raster_y = 0;
            self.new_frame = true;
            self.raster_y_irq_edge_detect();
            self.lp.untrigger();
            if self.lp_asserted {
                if self.lp.retrigger(self.cycles_per_line) {
                    self.irq_flags |= IRQ_LIGHTPEN;
                }
            }
        }
    }
}

impl Default for Mos656x {
    fn default() -> Self {
        Self::new()
    }
}

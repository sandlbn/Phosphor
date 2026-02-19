//! Sprite DMA handling for VIC-II.

const NUM_SPRITES: usize = 8;

pub struct Sprites {
    exp_flop: u8,
    pub dma: u8,
    mc_base: [u8; NUM_SPRITES],
    mc: [u8; NUM_SPRITES],
}

impl Sprites {
    pub fn new() -> Self {
        Self {
            exp_flop: 0xFF,
            dma: 0,
            mc_base: [0; NUM_SPRITES],
            mc: [0; NUM_SPRITES],
        }
    }

    pub fn reset(&mut self) {
        self.exp_flop = 0xFF;
        self.dma = 0;
        self.mc_base.fill(0);
        self.mc.fill(0);
    }

    pub fn update_mc(&mut self) {
        let mut mask: u8 = 1;
        for i in 0..NUM_SPRITES {
            if self.dma & mask != 0 {
                self.mc[i] = (self.mc[i] + 3) & 0x3F;
            }
            mask <<= 1;
        }
    }

    pub fn update_mc_base(&mut self) {
        let mut mask: u8 = 1;
        for i in 0..NUM_SPRITES {
            if self.exp_flop & mask != 0 {
                self.mc_base[i] = self.mc[i];
                if self.mc_base[i] == 0x3F {
                    self.dma &= !mask;
                }
            }
            mask <<= 1;
        }
    }

    pub fn check_exp(&mut self) {
        // regs[0x17] = y-expansion
        // This is called with the y_expansion register value
        // For now we just flip the expansion flop
        // In the full implementation we'd read from regs[0x17].
    }

    pub fn check_exp_with_reg(&mut self, y_expansion: u8) {
        self.exp_flop ^= self.dma & y_expansion;
    }

    pub fn check_display(&mut self) {
        for i in 0..NUM_SPRITES {
            self.mc[i] = self.mc_base[i];
        }
    }

    pub fn check_dma(&mut self, raster_y: u32, regs: &[u8; 0x40]) {
        let enable = regs[0x15];
        let y = (raster_y & 0xFF) as u8;
        let mut mask: u8 = 1;
        for i in 0..NUM_SPRITES {
            if (enable & mask != 0) && (y == regs[(i << 1) + 1]) && (self.dma & mask == 0) {
                self.dma |= mask;
                self.mc_base[i] = 0;
                self.exp_flop |= mask;
            }
            mask <<= 1;
        }
    }

    pub fn line_crunch(&mut self, data: u8, line_cycle: u32) {
        let mut mask: u8 = 1;
        for i in 0..NUM_SPRITES {
            if (data & mask == 0) && (self.exp_flop & mask == 0) {
                if line_cycle == 14 {
                    let mc_i = self.mc[i];
                    let mcb_i = self.mc_base[i];
                    self.mc[i] = (0x2A & (mcb_i & mc_i)) | (0x15 & (mcb_i | mc_i));
                }
                self.exp_flop |= mask;
            }
            mask <<= 1;
        }
    }

    pub fn is_dma(&self, val: u8) -> bool {
        (self.dma & val) != 0
    }
}

impl Default for Sprites {
    fn default() -> Self {
        Self::new()
    }
}

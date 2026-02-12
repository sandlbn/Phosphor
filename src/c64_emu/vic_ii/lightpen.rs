//! VIC-II lightpen emulation.

pub struct Lightpen {
    last_line: u32,
    cycles_per_line: u32,
    lpx: u8,
    lpy: u8,
    is_triggered: bool,
}

impl Lightpen {
    pub fn new() -> Self {
        Self {
            last_line: 0,
            cycles_per_line: 63,
            lpx: 0,
            lpy: 0,
            is_triggered: false,
        }
    }

    pub fn set_screen_size(&mut self, height: u32, width: u32) {
        self.last_line = height.saturating_sub(1);
        self.cycles_per_line = width;
    }

    pub fn reset(&mut self) {
        self.lpx = 0;
        self.lpy = 0;
        self.is_triggered = false;
    }

    pub fn get_x(&self) -> u8 {
        self.lpx
    }
    pub fn get_y(&self) -> u8 {
        self.lpy
    }

    pub fn retrigger(&mut self, cycles_per_line: u32) -> bool {
        if self.is_triggered {
            return false;
        }
        self.is_triggered = true;
        self.lpx = match cycles_per_line {
            65 => 0xD5,
            _ => 0xD1,
        };
        self.lpy = 0;
        true
    }

    pub fn trigger(&mut self, line_cycle: u32, raster_y: u32) -> bool {
        if self.is_triggered {
            return false;
        }
        self.is_triggered = true;

        if raster_y == self.last_line && line_cycle > 0 {
            return false;
        }

        let adjusted = if line_cycle < 13 {
            line_cycle + self.cycles_per_line
        } else {
            line_cycle
        } - 13;

        let adjusted = if self.cycles_per_line == 65 && adjusted > (61 - 13) {
            adjusted - 1
        } else {
            adjusted
        };

        self.lpx = ((adjusted << 2) + 2) as u8;
        self.lpy = raster_y as u8;
        true
    }

    pub fn untrigger(&mut self) {
        self.is_triggered = false;
    }
}

impl Default for Lightpen {
    fn default() -> Self {
        Self::new()
    }
}

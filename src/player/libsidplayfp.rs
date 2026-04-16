// libsidplayfp CPU engine — uses libsidplayfp's cycle-accurate C64 emulation
// to generate SID writes, which are then forwarded to the active SidDevice.
//
// This replaces the mos6502 crate's instruction-level emulation with
// libsidplayfp's per-cycle MOS6510 + CIA + VIC-II emulation.

use sidplayfp_sys::Player;

use super::memory::SidWrite;

/// ROM file names to search for.
const KERNAL_NAMES: &[&str] = &["kernal", "kernal.rom", "kernal.bin"];
const BASIC_NAMES: &[&str] = &["basic", "basic.rom", "basic.bin"];
const CHARGEN_NAMES: &[&str] = &["chargen", "chargen.rom", "chargen.bin"];

/// Standard ROM search directories.
fn rom_search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("C64_ROM_DIR") {
        dirs.push(std::path::PathBuf::from(d));
    }
    dirs.push(std::path::PathBuf::from("roms"));
    dirs.push(std::path::PathBuf::from("."));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/c64/roms"));
    }
    dirs.push(std::path::PathBuf::from("/usr/share/vice/C64"));
    dirs
}

fn find_rom(dirs: &[std::path::PathBuf], names: &[&str]) -> Option<Vec<u8>> {
    for dir in dirs {
        for name in names {
            let path = dir.join(name);
            if let Ok(data) = std::fs::read(&path) {
                return Some(data);
            }
        }
    }
    None
}

/// Wrapper around sidplayfp_sys::Player for use as a PlayEngine.
pub struct LibSidPlayFp {
    player: Player,
    /// Cached SID writes from the last frame (absolute cycle, mapped reg, val).
    pub sid_writes: Vec<SidWrite>,
    /// SID register shadow for UI.
    pub sid_shadow: [u8; 128],
    /// Number of SID chips.
    pub num_sids: usize,
    /// CIA1 Timer A latch.
    pub cia1_timer_a: u16,
    /// Diagnostic counter for periodic logging.
    frame_diag: u32,
}

impl LibSidPlayFp {
    /// Create a new player and load a SID file.
    pub fn new(sid_data: &[u8], subtune: u16) -> Result<Self, String> {
        let mut player = Player::new()?;

        // Try to load ROMs.
        let dirs = rom_search_dirs();
        let kernal = find_rom(&dirs, KERNAL_NAMES);
        let basic = find_rom(&dirs, BASIC_NAMES);
        let chargen = find_rom(&dirs, CHARGEN_NAMES);

        if kernal.is_some() {
            eprintln!("[libsidplayfp] ROM files found");
        } else {
            eprintln!("[libsidplayfp] ROM files not found — using built-in stubs");
        }

        // Set ROMs (libsidplayfp accepts raw pointers, None = built-in stubs).
        unsafe {
            let k = kernal.as_ref().and_then(|d| {
                if d.len() >= 8192 {
                    Some(&*(d[..8192].as_ptr() as *const [u8; 8192]))
                } else {
                    None
                }
            });
            let b = basic.as_ref().and_then(|d| {
                if d.len() >= 8192 {
                    Some(&*(d[..8192].as_ptr() as *const [u8; 8192]))
                } else {
                    None
                }
            });
            let c = chargen.as_ref().and_then(|d| {
                if d.len() >= 4096 {
                    Some(&*(d[..4096].as_ptr() as *const [u8; 4096]))
                } else {
                    None
                }
            });
            player.set_roms(k, b, c);
        }

        // Load the tune.
        player.load(sid_data, subtune)?;

        let num_sids = player.num_sids();
        let cia1_timer_a = player.cia1_timer_a();

        eprintln!(
            "[libsidplayfp] Loaded: {} SIDs, {}, CIA1 Timer A latch={}",
            num_sids,
            if player.is_pal() { "PAL" } else { "NTSC" },
            cia1_timer_a,
        );

        Ok(Self {
            player,
            sid_writes: Vec::with_capacity(512),
            sid_shadow: [0u8; 128],
            num_sids,
            cia1_timer_a,
            frame_diag: 0,
        })
    }

    /// Run one frame of emulation (cycles CPU cycles).
    /// Populates `self.sid_writes` with cycle-accurate SID writes.
    pub fn run_frame(&mut self, cycles: u32) {
        self.sid_writes.clear();

        let actual_cycles = match self.player.play(cycles) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[libsidplayfp] play error: {e}");
                return;
            }
        };

        let writes = self.player.get_writes();
        self.sid_writes.reserve(writes.len());

        for w in writes {
            // Map sid_num + reg to the unified register space:
            // SID0: 0x00-0x1F, SID1: 0x20-0x3F, SID2: 0x40-0x5F
            let mapped_reg = (w.sid_num as u8) * 0x20 + w.reg;
            // Clamp to requested frame size so downstream flush doesn't overshoot.
            let clamped_cycle = w.cycle.min(cycles);
            self.sid_writes.push((clamped_cycle, mapped_reg, w.val));

            // Update shadow.
            if (mapped_reg as usize) < self.sid_shadow.len() {
                self.sid_shadow[mapped_reg as usize] = w.val;
            }
        }

        if actual_cycles != cycles && self.frame_diag == 0 {
            eprintln!(
                "[libsidplayfp] frame: requested={} actual={} writes={}",
                cycles,
                actual_cycles,
                self.sid_writes.len(),
            );
        }
        self.frame_diag = (self.frame_diag + 1) % 250;
    }

    /// Clear writes and reset shadow.
    pub fn clear_writes(&mut self) {
        self.sid_writes.clear();
    }

    /// Compute voice levels from the shadow registers (approximation for visualization).
    pub fn voice_levels(&self) -> Vec<f32> {
        let mut levels = Vec::new();
        for sid in 0..self.num_sids {
            let base = sid * 0x20;
            for voice in 0..3 {
                let offset = base + voice * 7;
                // Use gate bit + sustain level as a rough approximation.
                let cr = self.sid_shadow.get(offset + 4).copied().unwrap_or(0);
                let sr = self.sid_shadow.get(offset + 6).copied().unwrap_or(0);
                let gate = (cr & 1) as f32;
                let sustain = ((sr >> 4) & 0xF) as f32 / 15.0;
                levels.push(gate * sustain);
            }
        }
        levels
    }
}

/// Home directory helper (minimal, no external crate dependency).
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

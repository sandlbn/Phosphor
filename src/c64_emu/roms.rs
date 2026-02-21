//! ROM file loader for the Commodore 64 emulator.
//!
//! Both `.bin` and `.rom` extensions are accepted (`.bin` tried first).
//!
//! # Search paths (tried in order)
//! 1. `$C64_ROM_DIR`  — environment variable
//! 2. `./roms/`       — next to the binary / working directory
//! 3. `./`            — working directory itself
//! 4. `~/.local/share/c64/roms/`
//! 5. `/usr/share/vice/C64/`

use std::path::{Path, PathBuf};
use std::{env, fs, io};

pub struct RomSet {
    pub kernal: Vec<u8>,
    pub basic: Vec<u8>,
    pub chargen: Vec<u8>,
}

impl RomSet {
    /// Search standard paths and load all three ROM images.
    pub fn load() -> io::Result<Self> {
        let dir = find_rom_dir()?;
        Self::load_from(&dir)
    }

    /// Load all three ROM images from an explicit directory.
    pub fn load_from<P: AsRef<Path>>(dir: P) -> io::Result<Self> {
        let dir = dir.as_ref();
        let kernal = load_rom(dir, "kernal", 0x2000)?;
        let basic = load_rom(dir, "basic", 0x2000)?;
        let chargen = load_rom(dir, "chargen", 0x1000)?;
        Ok(Self {
            kernal,
            basic,
            chargen,
        })
    }
}

// ── Internal helpers ──────────────────────────────────────────

fn find_rom_dir() -> io::Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(val) = env::var("C64_ROM_DIR") {
        candidates.push(PathBuf::from(val));
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("roms"));
        candidates.push(cwd.clone());
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(d) = exe.parent() {
            candidates.push(d.join("roms"));
            candidates.push(d.to_path_buf());
        }
    }
    if let Some(home) = dirs_home() {
        candidates.push(home.join(".local").join("share").join("c64").join("roms"));
    }
    candidates.push(PathBuf::from("/usr/share/vice/C64"));
    candidates.push(PathBuf::from("/usr/local/share/vice/C64"));

    for dir in &candidates {
        if has_all_roms(dir) {
            return Ok(dir.clone());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "C64 ROM files not found.  Searched:\n{}\n\
             Place kernal.bin/rom (8 KiB), basic.bin/rom (8 KiB), and \
             chargen.bin/rom (4 KiB) in one of those directories, \
             or set the C64_ROM_DIR environment variable.",
            candidates
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    ))
}

/// Returns true when all three ROMs exist (either .bin or .rom).
fn has_all_roms(dir: &Path) -> bool {
    ["kernal", "basic", "chargen"]
        .iter()
        .all(|base| rom_path(dir, base).is_some())
}

/// Find a ROM file by base name — tries .bin first, then .rom.
fn rom_path(dir: &Path, base: &str) -> Option<PathBuf> {
    for ext in &["bin", "rom"] {
        let p = dir.join(format!("{base}.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Read a ROM file, checking minimum size.
fn load_rom(dir: &Path, base: &str, expected_size: usize) -> io::Result<Vec<u8>> {
    let path = rom_path(dir, base).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}/{}.bin or .rom not found", dir.display(), base),
        )
    })?;

    let data = fs::read(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;

    if data.len() < expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: expected at least {} bytes, got {}",
                path.display(),
                expected_size,
                data.len()
            ),
        ));
    }

    eprintln!("[c64] Loaded ROM: {}", path.display());
    Ok(data)
}

fn dirs_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}

// PETSCII decoder for Commodore 64 .wds lyrics files.

use std::path::Path;

/// Decode a PETSCII byte buffer (Commodore 64 character set) into a UTF-8 string.
/// Strips control codes, converts shifted uppercase, preserves verse structure.
pub fn petscii_to_string(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    for &b in data {
        match b {
            0x0D => out.push('\n'),
            0x20..=0x7E => out.push(b as char),
            // PETSCII shifted uppercase A-Z
            0xC1..=0xDA => out.push((b - 0x80) as char),
            // Skip control codes and other non-printable bytes.
            _ => {}
        }
    }
    // Trim each line, collapse runs of 3+ blank lines into one blank line.
    let mut result = String::new();
    let mut consecutive_blanks = 0u32;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks <= 1 && !result.is_empty() {
                result.push('\n');
            }
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(trimmed);
            consecutive_blanks = 0;
        }
    }
    result.trim().to_string()
}

/// Try to load a companion .wds lyrics file for a MUS path.
/// Returns the decoded lyrics string, or None if no .wds file exists.
pub fn load_wds_lyrics(mus_path: &Path) -> Option<String> {
    let ext = mus_path.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("mus") {
        return None;
    }
    for wds_ext in &["wds", "WDS"] {
        let wds_path = mus_path.with_extension(wds_ext);
        if let Ok(data) = std::fs::read(&wds_path) {
            eprintln!("[phosphor] WDS lyrics loaded: {}", wds_path.display());
            return Some(petscii_to_string(&data));
        }
    }
    None
}

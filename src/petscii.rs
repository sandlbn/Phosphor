// PETSCII decoder for Commodore 64 .wds lyrics files.

use std::path::Path;

/// Decode a PETSCII byte buffer (Commodore 64 character set) into a UTF-8 string.
/// Strips control codes, converts shifted uppercase, preserves verse structure.
#[allow(dead_code)]
pub fn petscii_to_string(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    for &b in data {
        match b {
            0x0D => out.push('\n'),
            // ASCII printable — but PETSCII 0x60-0x7E are graphic chars, skip them.
            0x20..=0x5F => out.push(b as char),
            // PETSCII shifted uppercase A-Z
            0xC1..=0xDA => out.push((b - 0x80) as char),
            // Skip graphics, control codes, color codes.
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

/// Extract credit lines embedded in a MUS file.
///
/// MUS format: bytes 2-7 are three little-endian u16 voice lengths.
/// Credit text (PETSCII) starts after header (8 bytes) + voice1 + voice2 + voice3.
/// Each credit line is terminated by 0x0D; a 0x00 byte ends the block.
/// Returns cleaned lines suitable for title/author display.
fn _extract_mus_credits(data: &[u8]) -> Vec<String> {
    if data.len() < 8 {
        return Vec::new();
    }
    let voice1_len = u16::from_le_bytes([data[2], data[3]]) as usize;
    let voice2_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    let voice3_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let credits_offset = 8 + voice1_len + voice2_len + voice3_len;
    if credits_offset >= data.len() {
        return Vec::new();
    }

    // Decode PETSCII credit block into lines.
    let mut lines = Vec::new();
    let mut current = String::new();
    for &b in &data[credits_offset..] {
        match b {
            0x00 => break, // end of credits
            0x0D => {
                let cleaned = _clean_credit_line(&current);
                if !cleaned.is_empty() {
                    lines.push(cleaned);
                }
                current.clear();
            }
            // ASCII printable range — but PETSCII 0x60-0x7E are graphic
            // characters (not letters), so only keep 0x20-0x5F.
            0x20..=0x5F => current.push(b as char),
            // PETSCII shifted uppercase A-Z
            0xC1..=0xDA => current.push((b - 0x80) as char),
            _ => {} // skip graphics, control codes, color codes
        }
    }
    // Flush last line.
    let cleaned = _clean_credit_line(&current);
    if !cleaned.is_empty() {
        lines.push(cleaned);
    }
    lines
}

/// Strip PETSCII/ASCII graphic artifacts from a credit line.
/// Keeps only letters, digits, spaces, and minimal punctuation.
/// Collapses decorative characters and whitespace.
/// Trims decorative single-letter padding from edges (e.g. "XX TITLE XX").
fn _clean_credit_line(raw: &str) -> String {
    // First pass: collapse runs of 3+ identical characters to a single space.
    // In PETSCII credits, repeated letters (CCCCCC, SSSS) are decorative lines.
    let mut collapsed = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut run = 1;
        while i + run < chars.len() && chars[i + run] == c {
            run += 1;
        }
        if run >= 3 && c.is_alphabetic() {
            // Decorative run — replace with space
            collapsed.push(' ');
        } else {
            for _ in 0..run {
                collapsed.push(c);
            }
        }
        i += run;
    }

    // Second pass: keep only letters, digits, and minimal punctuation.
    let mut result = String::new();
    let mut prev_was_space = true;
    for c in collapsed.chars() {
        if c.is_alphanumeric() || matches!(c, '\'' | ',' | '.' | ':' | '!' | '?' | '&') {
            result.push(c);
            prev_was_space = false;
        } else {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        }
    }
    let result = result.trim();

    // Strip decorative single/double letter padding from edges.
    // Patterns like "XX TITLE XX", "X AUTHOR X", "SS TEXT SS".
    let words: Vec<&str> = result.split_whitespace().collect();
    let start = if words.len() > 2
        && words[0].len() <= 2
        && words[0]
            .chars()
            .all(|c| c == words[0].chars().next().unwrap())
    {
        1
    } else {
        0
    };
    let end = if words.len() > 2
        && words[words.len() - 1].len() <= 2
        && words[words.len() - 1]
            .chars()
            .all(|c| c == words[words.len() - 1].chars().next().unwrap())
    {
        words.len() - 1
    } else {
        words.len()
    };
    let result: String = words[start..end].join(" ");
    // Skip lines shorter than 2 chars (likely pure decoration).
    if result.len() < 2 {
        String::new()
    } else {
        result
    }
}

/// Extract raw credit text from a MUS file without cleaning.
/// Returns the PETSCII text decoded to ASCII/UTF-8 preserving decorative characters.
fn _extract_mus_credits_raw(data: &[u8]) -> String {
    if data.len() < 8 {
        return String::new();
    }
    let voice1_len = u16::from_le_bytes([data[2], data[3]]) as usize;
    let voice2_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    let voice3_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let credits_offset = 8 + voice1_len + voice2_len + voice3_len;
    if credits_offset >= data.len() {
        return String::new();
    }

    let mut out = String::new();
    for &b in &data[credits_offset..] {
        match b {
            0x00 => break,
            0x0D => out.push('\n'),
            0x20..=0x5F => out.push(b as char),
            0xC1..=0xDA => out.push((b - 0x80) as char),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Extract FLAG command timestamps from MUS Voice 1 data.
///
/// The MUS format uses 2-byte commands. The key rule from the 6502 source:
/// if `first_byte & 0x03 == 0`, it's a note/rest (has duration).
/// Otherwise it's a command (zero duration).
///
/// FLAG commands (0x46 XX) in Voice 1 trigger the display of the next
/// WDS lyrics line. We accumulate note durations in jiffies (1/60s) and
/// record the jiffy count at each FLAG command.
///
/// Returns a vector of timestamps (in seconds) — one per FLAG command found.
/// Returns (flag_timestamps, estimated_total_duration_secs).
#[allow(dead_code)]
pub fn extract_mus_flag_times(data: &[u8]) -> (Vec<f32>, Option<u32>) {
    if data.len() < 8 {
        return (Vec::new(), None);
    }
    let voice1_len = u16::from_le_bytes([data[2], data[3]]) as usize;
    let voice1_start = 8_usize;
    let voice1_end = voice1_start + voice1_len;
    if voice1_end > data.len() {
        return (Vec::new(), None);
    }

    let voice1 = &data[voice1_start..voice1_end];
    let mut timestamps = Vec::new();
    let mut jiffies: u32 = 0;
    let mut tempo: u32 = 0x90; // WorkTempo default
    let mut tempo_tbl: u32 = 0x60; // WorkTempoTableValue default
    let utl_value: u32 = 12; // UTILITY jiffy count default

    // CIA timer determines how long a "jiffy" is in real time.
    // Default: 60 Hz (NTSC: 17045 cycles, PAL: 16421 cycles).
    // JIF commands modify this. We track the ratio to convert jiffies→seconds.
    // Use PAL as default (most C64 music is PAL).
    let cia_default: f32 = 16421.0; // PAL default
    let cpu_clock: f32 = 985248.0; // PAL CPU clock
    let mut cia_timer: f32 = cia_default;
    let mut i = 0;

    while i + 1 < voice1.len() {
        let b0 = voice1[i];
        let b1 = voice1[i + 1];

        // Note or rest: first_byte & 0x03 == 0
        if b0 & 0x03 == 0 {
            jiffies += mus_note_jiffies(b0, tempo, tempo_tbl, utl_value);
            i += 2;
            continue;
        }

        // Command (first_byte & 0x03 != 0) — zero duration.
        // HALT
        if b0 == 0x01 && b1 == 0x4F {
            break;
        }
        // FLAG — record timestamp (convert jiffies to seconds using current CIA rate)
        if b0 == 0x46 {
            let jiffy_secs = cia_timer / cpu_clock;
            timestamps.push(jiffies as f32 * jiffy_secs);
        }
        // TEMPO — update tempo and table value
        if b0 == 0x06 {
            tempo = b1 as u32;
            let idx = (b1 as usize) >> 3;
            const TEMPO_TABLE: [u8; 32] = [
                0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x20, 0x00, 0x00, 0x30, 0x00, 0x00, 0x40, 0x00,
                0x00, 0x50, 0x00, 0x00, 0x60, 0x00, 0x00, 0x70, 0x00, 0x00, 0x80, 0x00, 0x00, 0x90,
                0x00, 0x00, 0xA0, 0x00,
            ];
            if idx < 32 {
                tempo_tbl = TEMPO_TABLE[idx] as u32;
            }
        }
        // JIF — adjust CIA timer (changes jiffy rate)
        // Pattern: aa111110 (b0 & 0x3F == 0x3E)
        // 6502: LSR, ROR, ROR on b0, then add to default CIA lo/hi
        if b0 & 0x3F == 0x3E {
            let a = b0 as u32;
            // Simulate 6502: LSR, ROR, ROR
            let (a2, c1) = (a >> 1, a & 1); // LSR
            let (a3, c2) = ((c1 << 7) | (a2 >> 1), a2 & 1); // ROR
            let (lo_adj, _c3) = ((c2 << 7) | (a3 >> 1), a3 & 1); // ROR
            let hi_adj = b1 as u32;
            let new_lo = (cia_default as u32 & 0xFF) + lo_adj;
            let carry = if new_lo > 0xFF { 1 } else { 0 };
            let new_hi = (cia_default as u32 >> 8) + hi_adj + carry;
            cia_timer = ((new_hi & 0xFF) << 8 | (new_lo & 0xFF)) as f32;
        }

        i += 2;
    }

    // Estimated total duration from Voice 1 jiffies.
    let total_secs = jiffies as f32 * (cia_timer / cpu_clock);
    let duration = if total_secs > 1.0 {
        Some(total_secs as u32)
    } else {
        None
    };

    (timestamps, duration)
}

/// Calculate the jiffy count for a single MUS note/rest command.
/// `dur_byte` is the first byte (bits 0-1 are always 0 for notes).
#[allow(dead_code)]
fn mus_note_jiffies(dur_byte: u8, tempo: u32, tempo_tbl: u32, utl_value: u32) -> u32 {
    let len_class = (dur_byte >> 2) & 0x07;
    let is_triplet = (dur_byte & 0x80) != 0 && (dur_byte & 0x20) == 0;
    let _is_dotted = (dur_byte & 0x20) != 0 && (dur_byte & 0x80) == 0;
    let is_double_dotted = (dur_byte & 0x80) != 0 && (dur_byte & 0x20) != 0;

    match len_class {
        0 => {
            // ABSOLUTE SET or triplet 64th
            if is_triplet {
                (tempo_tbl >> 6) & 3
            } else {
                let v = (tempo >> 6) & 3;
                if v == 0 {
                    4
                } else {
                    v
                }
            }
        }
        1 => {
            // UTILITY / UTILITY-VOICE
            utl_value
        }
        2..=7 => {
            let shift = len_class - 2;
            if is_triplet {
                // Triplet path (lookupTempoFromTbl): just base, no multiplier.
                tempo_tbl >> shift
            } else {
                // Standard path: base jiffies from tempo.
                // Dotted = 1.5x, double-dotted = 1.75x.
                let base = tempo >> shift;
                if is_double_dotted {
                    base + base / 2 + base / 4
                } else if _is_dotted {
                    base + base / 2
                } else {
                    base
                }
            }
        }
        _ => 0,
    }
}

/// Check if a MUS file contains any FLAG commands in any voice.
pub fn mus_has_flags(data: &[u8]) -> bool {
    if data.len() < 8 {
        return false;
    }
    let v1_len = u16::from_le_bytes([data[2], data[3]]) as usize;
    let v2_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    let v3_len = u16::from_le_bytes([data[6], data[7]]) as usize;

    for &(start, len) in &[
        (8, v1_len),
        (8 + v1_len, v2_len),
        (8 + v1_len + v2_len, v3_len),
    ] {
        let end = start + len;
        if end > data.len() {
            continue;
        }
        let voice = &data[start..end];
        let mut i = 0;
        while i + 1 < voice.len() {
            let b0 = voice[i];
            let b1 = voice[i + 1];
            if b0 & 0x03 != 0 {
                if b0 == 0x46 {
                    return true;
                }
                if b0 == 0x01 && b1 == 0x4F {
                    break;
                }
            }
            i += 2;
        }
    }
    false
}

/// Parse WDS lyrics into logical lyric groups.
///
/// WDS files store **screen rows**, not logical lyrics. A single lyric phrase
/// may span multiple rows due to line wrapping / indentation. Continuation
/// rows start with leading spaces (after stripping PETSCII control codes like
/// $92 reverse-off). Each group is one or more screen rows that belong together.
///
/// Returns `Vec<Vec<String>>` — each inner Vec is one logical lyric unit
/// containing 1+ trimmed display rows.
pub fn petscii_to_wds_groups(data: &[u8]) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();

    // Split raw bytes on 0x0D (carriage return) to get PETSCII rows.
    for row_bytes in data.split(|&b| b == 0x0D) {
        // Decode PETSCII → UTF-8.
        let mut decoded = String::new();
        for &b in row_bytes {
            match b {
                0x20..=0x5F => decoded.push(b as char),
                0xC1..=0xDA => decoded.push((b - 0x80) as char),
                _ => {}
            }
        }

        let trimmed = decoded.trim().to_string();

        // Blank row → keep as its own group (acts as a timing pause:
        // a FLAG that lands on it shows nothing, just like the original player).
        // Do NOT collapse consecutive blanks — each blank WDS row consumes
        // one FLAG in the original SIDplayer, so we must preserve that 1:1 mapping.
        if trimmed.is_empty() {
            groups.push(vec![String::new()]);
            continue;
        }

        // Continuation row: raw PETSCII starts with $92 $20 (reverse-off + space).
        // This is the standard WDS indentation for wrapped lyrics lines.
        // Centered text uses other control codes before spaces, so won't match.
        let is_continuation = row_bytes.len() >= 2
            && row_bytes[0] == 0x92
            && row_bytes[1] == 0x20;

        if is_continuation && !groups.is_empty() {
            groups.last_mut().unwrap().push(trimmed);
        } else {
            groups.push(vec![trimmed]);
        }
    }

    groups
}

/// Try to load a companion .wds lyrics file for a MUS path.
/// Returns logical lyric groups, or None if no .wds file exists.
pub fn load_wds_lyrics(mus_path: &Path) -> Option<Vec<Vec<String>>> {
    let ext = mus_path.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("mus") {
        return None;
    }
    for wds_ext in &["wds", "WDS"] {
        let wds_path = mus_path.with_extension(wds_ext);
        if let Ok(data) = std::fs::read(&wds_path) {
            eprintln!("[phosphor] WDS lyrics loaded: {}", wds_path.display());
            let groups = petscii_to_wds_groups(&data);
            if groups.is_empty() {
                continue;
            }
            return Some(groups);
        }
    }
    None
}

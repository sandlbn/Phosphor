// PSID / RSID header parser (v1–v4) with payload extraction.

/// Parsed SID file header.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SidHeader {
    pub magic: String,
    pub version: u16,
    pub data_offset: u16,
    pub load_address: u16,
    pub init_address: u16,
    pub play_address: u16,
    pub songs: u16,
    pub start_song: u16,
    pub speed: u32,
    pub name: String,
    pub author: String,
    pub released: String,
    pub is_pal: bool,
    pub is_rsid: bool,
    /// C64 addresses of extra SIDs (0 = unused). Index 0 = SID2, 1 = SID3.
    pub extra_sid_addrs: [u16; 2],
}

impl SidHeader {
    /// Number of SID chips the tune uses (1–3 from header alone).
    pub fn num_sids(&self) -> usize {
        1 + self.extra_sid_addrs.iter().filter(|&&a| a != 0).count()
    }

    /// Frame rate in Hz.
    #[allow(dead_code)]
    pub fn frame_rate(&self) -> f64 {
        if self.is_pal {
            50.0
        } else {
            60.0
        }
    }

    /// Frame duration in microseconds.
    pub fn frame_us(&self) -> u64 {
        if self.is_pal {
            20_000
        } else {
            16_667
        }
    }
}

/// A fully loaded SID file: header + extracted payload + its load address.
#[derive(Debug, Clone)]
pub struct SidFile {
    pub header: SidHeader,
    pub load_address: u16,
    pub payload: Vec<u8>,
    /// Full raw file bytes (needed for MD5 computation for Songlength).
    pub raw: Vec<u8>,
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn read_be_u16(d: &[u8], o: usize) -> u16 {
    ((d[o] as u16) << 8) | d[o + 1] as u16
}

fn read_be_u32(d: &[u8], o: usize) -> u32 {
    ((d[o] as u32) << 24) | ((d[o + 1] as u32) << 16) | ((d[o + 2] as u32) << 8) | d[o + 3] as u32
}

fn read_string(d: &[u8], o: usize, len: usize) -> String {
    let s = &d[o..o + len];
    let end = s.iter().position(|&b| b == 0).unwrap_or(len);
    String::from_utf8_lossy(&s[..end]).to_string()
}

/// Decode a SID address byte (from header offset $7A or $7B).
fn decode_sid_addr_byte(b: u8) -> u16 {
    if b >= 0x42 && (b <= 0x7F || b >= 0xE0) && (b & 1) == 0 {
        0xD000 | ((b as u16) << 4)
    } else {
        0
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Parse a SID file from raw bytes.
pub fn load_sid(data: &[u8]) -> Result<SidFile, String> {
    let header = parse_header(data)?;
    let ds = header.data_offset as usize;

    if ds >= data.len() {
        return Err("data_offset past end of file".into());
    }

    let (load_address, payload_start) = if header.load_address == 0 {
        if ds + 2 > data.len() {
            return Err("File too small for embedded load address".into());
        }
        let lo = data[ds] as u16;
        let hi = data[ds + 1] as u16;
        ((hi << 8) | lo, ds + 2)
    } else {
        (header.load_address, ds)
    };

    let payload = data[payload_start..].to_vec();

    Ok(SidFile {
        header,
        load_address,
        payload,
        raw: data.to_vec(),
    })
}

/// Parse just the header (no payload extraction).
pub fn parse_header(data: &[u8]) -> Result<SidHeader, String> {
    if data.len() < 0x76 {
        return Err("File too small for a SID header".into());
    }

    let magic = String::from_utf8_lossy(&data[0..4]).to_string();
    if magic != "PSID" && magic != "RSID" {
        return Err(format!("Not a SID file (magic={magic:?})"));
    }

    let is_rsid = magic == "RSID";
    let version = read_be_u16(data, 0x04);
    let mut is_pal = true;
    let mut extra_sid_addrs = [0u16; 2];

    if version >= 2 && data.len() >= 0x7C {
        let flags = read_be_u16(data, 0x76);
        is_pal = ((flags >> 2) & 0x03) != 2;

        if version >= 3 && data.len() > 0x7A {
            extra_sid_addrs[0] = decode_sid_addr_byte(data[0x7A]);
        }
        if version >= 4 && data.len() > 0x7B {
            extra_sid_addrs[1] = decode_sid_addr_byte(data[0x7B]);
        }
    }

    Ok(SidHeader {
        magic,
        version,
        data_offset: read_be_u16(data, 0x06),
        load_address: read_be_u16(data, 0x08),
        init_address: read_be_u16(data, 0x0A),
        play_address: read_be_u16(data, 0x0C),
        songs: read_be_u16(data, 0x0E),
        start_song: read_be_u16(data, 0x10),
        speed: read_be_u32(data, 0x12),
        name: read_string(data, 0x16, 32),
        author: read_string(data, 0x36, 32),
        released: read_string(data, 0x56, 32),
        is_pal,
        is_rsid,
        extra_sid_addrs,
    })
}

/// Compute the MD5 hash for Songlength database lookup.
///
///
/// https://hvsc.c64.org/download/C64Music/DOCUMENTS/Songlengths.faq
pub fn compute_hvsc_md5(sid: &SidFile) -> String {
    format!("{:x}", md5::compute(&sid.raw))
}

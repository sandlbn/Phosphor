// Persistent configuration: skip RSID, default song length, songlength download URL.
// Stored as JSON in <config_dir>/phosphor/config.json

use std::collections::HashSet;
use std::path::PathBuf;

/// Default HVSC Songlength.md5 download URL.
pub const DEFAULT_SONGLENGTH_URL: &str =
    "https://hvsc.c64.org/download/C64Music/DOCUMENTS/Songlengths.md5";

#[derive(Debug, Clone)]
pub struct Config {
    /// Skip RSID tunes during playback (auto-advance to next PSID).
    pub skip_rsid: bool,
    /// Default song length in seconds when Songlength DB has no entry.
    /// 0 = disabled (no auto-advance for unknown lengths).
    pub default_song_length_secs: u32,
    /// URL to download Songlength.md5 from.
    pub songlength_url: String,
    /// Audio output engine name ("auto", "usb", "emulated", "u64").
    pub output_engine: String,
    /// Ultimate 64 IP address or hostname (for "u64" engine).
    pub u64_address: String,
    /// Ultimate 64 network password (optional, empty = none).
    pub u64_password: String,
    /// Last directory used when opening SID files / folders.
    pub last_sid_dir: Option<String>,
    /// Last directory used when loading Songlength.md5.
    pub last_songlength_dir: Option<String>,
    /// Path to last successfully loaded Songlength.md5 file.
    pub last_songlength_file: Option<String>,
    /// Last directory used for playlists.
    pub last_playlist_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            skip_rsid: false,
            default_song_length_secs: 0,
            songlength_url: DEFAULT_SONGLENGTH_URL.to_string(),
            output_engine: "auto".to_string(),
            u64_address: String::new(),
            u64_password: String::new(),
            last_sid_dir: None,
            last_songlength_dir: None,
            last_songlength_file: None,
            last_playlist_dir: None,
        }
    }
}

impl Config {
    /// Path to the config file.
    pub fn config_path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("config.json"))
    }

    /// Load config from disk, or return defaults if not found / invalid.
    pub fn load() -> Self {
        let path = match Self::config_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => Self::parse_json(&content),
            Err(e) => {
                eprintln!("[phosphor] Cannot read config: {e}");
                Self::default()
            }
        }
    }

    /// Save config to disk.
    pub fn save(&self) {
        let path = match Self::config_path() {
            Some(p) => p,
            None => return,
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let json = self.to_json();
        if let Err(e) = std::fs::write(&path, json) {
            eprintln!("[phosphor] Cannot save config: {e}");
        } else {
            eprintln!("[phosphor] Config saved to {}", path.display());
        }
    }

    /// Parse config from a JSON string. Unknown fields are ignored,
    /// missing fields get defaults.
    fn parse_json(s: &str) -> Self {
        let mut config = Self::default();

        // Simple manual JSON parsing to avoid serde dependency.
        for line in s.lines() {
            let line = line.trim().trim_end_matches(',');
            if let Some(rest) = line.strip_prefix("\"skip_rsid\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val == "true" {
                    config.skip_rsid = true;
                } else {
                    config.skip_rsid = false;
                }
            } else if let Some(rest) = line.strip_prefix("\"default_song_length_secs\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<u32>() {
                    config.default_song_length_secs = n;
                }
            } else if let Some(rest) = line.strip_prefix("\"songlength_url\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.songlength_url = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"output_engine\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.output_engine = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"u64_address\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.u64_address = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"u64_password\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.u64_password = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"last_sid_dir\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.last_sid_dir = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"last_songlength_dir\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.last_songlength_dir = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"last_songlength_file\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.last_songlength_file = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"last_playlist_dir\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.last_playlist_dir = strip_json_string(val);
                }
            }
        }

        config
    }

    /// Serialize config to a JSON string.
    fn to_json(&self) -> String {
        let fmt_opt = |v: &Option<String>| -> String {
            match v {
                Some(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
                None => "null".to_string(),
            }
        };
        format!(
            concat!(
                "{{\n",
                "  \"skip_rsid\": {},\n",
                "  \"default_song_length_secs\": {},\n",
                "  \"songlength_url\": \"{}\",\n",
                "  \"output_engine\": \"{}\",\n",
                "  \"u64_address\": \"{}\",\n",
                "  \"u64_password\": \"{}\",\n",
                "  \"last_sid_dir\": {},\n",
                "  \"last_songlength_dir\": {},\n",
                "  \"last_songlength_file\": {},\n",
                "  \"last_playlist_dir\": {}\n",
                "}}\n",
            ),
            self.skip_rsid,
            self.default_song_length_secs,
            self.songlength_url,
            self.output_engine,
            self.u64_address.replace('\\', "\\\\").replace('"', "\\\""),
            self.u64_password.replace('\\', "\\\\").replace('"', "\\\""),
            fmt_opt(&self.last_sid_dir),
            fmt_opt(&self.last_songlength_dir),
            fmt_opt(&self.last_songlength_file),
            fmt_opt(&self.last_playlist_dir),
        )
    }

    /// Helper: get the output engine name.
    pub fn output_engine(&self) -> String {
        self.output_engine.clone()
    }

    /// Remember a directory from a file path (for SID file dialogs).
    pub fn remember_sid_dir(&mut self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            self.last_sid_dir = Some(parent.to_string_lossy().into_owned());
            self.save();
        }
    }

    /// Remember a directory from a songlength file path.
    pub fn remember_songlength_path(&mut self, path: &std::path::Path) {
        self.last_songlength_file = Some(path.to_string_lossy().into_owned());
        if let Some(parent) = path.parent() {
            self.last_songlength_dir = Some(parent.to_string_lossy().into_owned());
        }
        self.save();
    }

    /// Remember a directory from a playlist file path.
    pub fn remember_playlist_dir(&mut self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            self.last_playlist_dir = Some(parent.to_string_lossy().into_owned());
            self.save();
        }
    }
}

/// Strip surrounding quotes from a JSON string value and unescape.
fn strip_json_string(val: &str) -> Option<String> {
    if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
        Some(
            val[1..val.len() - 1]
                .replace("\\\\", "\x00")
                .replace("\\\"", "\"")
                .replace('\x00', "\\"),
        )
    } else {
        None
    }
}

/// Path to the Songlength.md5 file (in our config directory).
pub fn songlength_db_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("Songlengths.md5"))
}

/// Download Songlength.md5 from the given URL and save it.
/// Returns the path on success.
pub async fn download_songlength(url: String) -> Result<PathBuf, String> {
    let dest =
        songlength_db_path().ok_or_else(|| "Cannot determine config directory".to_string())?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create directory: {e}"))?;
    }

    eprintln!("[phosphor] Downloading Songlength.md5 from {url}...");

    // Use curl for the download (available on macOS and Linux).
    // This blocks briefly but Task::perform runs it off the main thread.
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "60",
            "-o",
            &dest.to_string_lossy(),
            &url,
        ])
        .output()
        .map_err(|e| format!("Failed to run curl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Download failed: {stderr}"));
    }

    // Verify the file was actually written
    let meta = std::fs::metadata(&dest).map_err(|e| format!("Downloaded file not found: {e}"))?;

    eprintln!(
        "[phosphor] Songlength.md5 saved to {} ({} bytes)",
        dest.display(),
        meta.len(),
    );
    Ok(dest)
}

/// Persistent set of favorite tunes, keyed by MD5 hash.
/// Stored as one hash per line in <config_dir>/favorites.txt
#[derive(Debug, Clone)]
pub struct FavoritesDb {
    pub hashes: HashSet<String>,
}

impl FavoritesDb {
    pub fn new() -> Self {
        Self {
            hashes: HashSet::new(),
        }
    }

    fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("favorites.txt"))
    }

    /// Load favorites from disk, or return empty set.
    pub fn load() -> Self {
        let path = match Self::path() {
            Some(p) if p.exists() => p,
            _ => return Self::new(),
        };

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let hashes: HashSet<String> = content
                    .lines()
                    .map(|l| l.trim().to_lowercase())
                    .filter(|l| !l.is_empty() && l.len() == 32)
                    .collect();
                eprintln!("[phosphor] Loaded {} favorites", hashes.len());
                Self { hashes }
            }
            Err(e) => {
                eprintln!("[phosphor] Cannot read favorites: {e}");
                Self::new()
            }
        }
    }

    /// Save favorites to disk.
    pub fn save(&self) {
        let path = match Self::path() {
            Some(p) => p,
            None => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut lines: Vec<&str> = self.hashes.iter().map(|s| s.as_str()).collect();
        lines.sort();
        let content = lines.join("\n") + "\n";
        if let Err(e) = std::fs::write(&path, content) {
            eprintln!("[phosphor] Cannot save favorites: {e}");
        }
    }

    /// Toggle a hash in/out of favorites. Returns new state (true = now favorite).
    pub fn toggle(&mut self, md5: &str) -> bool {
        let key = md5.to_lowercase();
        if self.hashes.contains(&key) {
            self.hashes.remove(&key);
            false
        } else {
            self.hashes.insert(key);
            true
        }
    }

    pub fn is_favorite(&self, md5: &str) -> bool {
        self.hashes.contains(&md5.to_lowercase())
    }

    pub fn count(&self) -> usize {
        self.hashes.len()
    }
}

/// Get the application config directory.
fn config_dir() -> Option<PathBuf> {
    // macOS:   ~/Library/Application Support/phosphor/
    // Linux:   ~/.config/phosphor/
    // Windows: %APPDATA%/phosphor/

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("phosphor"),
        )
    }

    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(appdata).join("phosphor"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".config").join("phosphor"))
    }
}

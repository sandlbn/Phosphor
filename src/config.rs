// Persistent configuration: skip RSID, default song length, songlength download URL.
// Stored as JSON in <config_dir>/phosphor/config.json

use std::collections::HashSet;
use std::path::PathBuf;

/// Default HTTPS URL for the HVSC C64Music/ tree. Single source of truth:
/// the full recursive sync crawls under this URL, AND `Songlengths.md5` +
/// `STIL.txt` are refreshed from `<this>/DOCUMENTS/{Songlengths.md5,STIL.txt}`.
/// prg.dtu.dk is the TU Denmark academic mirror — it serves real Apache
/// HTML directory listings, which our crawler needs.
pub const DEFAULT_HVSC_RSYNC_URL: &str = "https://www.prg.dtu.dk/HVSC/C64Music/";

/// Default window dimensions — used on first launch.
const DEFAULT_WINDOW_WIDTH: f32 = 900.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 600.0;

#[derive(Debug, Clone)]
pub struct Config {
    /// Skip RSID tunes during playback (auto-advance to next PSID).
    pub skip_rsid: bool,
    /// Default song length in seconds when Songlength DB has no entry.
    /// 0 = disabled (no auto-advance for unknown lengths).
    pub default_song_length_secs: u32,
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
    /// Path to last successfully loaded STIL.txt file.
    pub last_stil_file: Option<String>,
    /// Optional HVSC root directory — used to compute HVSC-relative paths for STIL lookup
    /// AND as the destination for the in-app rsync sync.
    pub hvsc_root: Option<String>,
    /// rsync URL to pull HVSC from. Default is sidplay5's mirror, confirmed
    /// working with our `arrsync-phosphor` fork.
    pub hvsc_rsync_url: String,
    /// ISO-8601 timestamp of the last successful HVSC sync (display only).
    pub hvsc_last_sync: Option<String>,
    /// Browser source toggle. "local" (default) or "a64". Persisted so
    /// the user's last-picked source survives restarts.
    pub browser_source: String,
    /// Last Assembly64 search query — restored into the search box on
    /// browser open. Small QOL.
    pub assembly64_last_query: Option<String>,
    /// UNIX timestamp (seconds) of the last successful Published Playlists
    /// sync. Drives the "Last synced: 4 min ago" indicator.
    pub published_playlists_last_synced: Option<i64>,
    /// Last HVSC version string fetched from the CDN (e.g. "HVSC #80").
    /// Used to detect when a new release is available.
    pub hvsc_known_version: Option<String>,
    /// Stream audio from the Ultimate 64 back to this machine via UDP.
    /// When enabled, Phosphor starts a UDP listener and asks the U64 to stream
    /// its audio output to us — letting you hear playback on the host computer.
    pub u64_audio_enabled: bool,
    /// UDP port to receive U64 audio stream on (default 11001).
    pub u64_audio_port: u16,
    /// Force stereo mirroring for 2SID tunes (duplicate SID1 writes to SID2).
    /// When enabled, 2SID tunes play in mono-stereo mode instead of true dual-SID.
    pub force_stereo_2sid: bool,
    /// Restart the USB device when loading a new SID file (macOS only).
    pub restart_usb_on_load: bool,
    /// macOS USB transport mode: "bridge" (default — talk to the root-owned
    /// usbsid-bridge LaunchDaemon over a Unix socket) or "direct"
    /// on Linux/Windows, which always use the direct path.
    pub macos_usb_mode: String,
    /// Enable the built-in HTTP server for remote control from a web browser.
    pub http_remote_enabled: bool,
    /// Port for the HTTP remote control server (default 8364).
    pub http_remote_port: u16,
    /// Last known window position — restored on next launch.
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
    /// Last known window size — restored on next launch.
    pub window_width_saved: f32,
    pub window_height_saved: f32,
    /// Base font size in points. The UI scales every text element relative
    /// to this — default 12.0 reproduces the original sizing exactly.
    /// Clamped to [8.0, 32.0] on read/write to keep the layout legible.
    pub base_font_size: f32,
    /// Host-side master volume in [0.0, 1.0]. Applied in the cpal output
    /// callbacks of emulated / sidlite / U64-streaming engines. Has no
    /// effect on the USB hardware engine (analog output, not reachable
    /// from host). Default 1.0 = unity gain (no change vs prior versions).
    pub master_volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            skip_rsid: false,
            default_song_length_secs: 0,
            output_engine: "auto".to_string(),
            u64_address: String::new(),
            u64_password: String::new(),
            last_sid_dir: None,
            last_songlength_dir: None,
            last_songlength_file: None,
            last_playlist_dir: None,
            last_stil_file: None,
            hvsc_root: None,
            hvsc_rsync_url: DEFAULT_HVSC_RSYNC_URL.to_string(),
            hvsc_last_sync: None,
            browser_source: "local".to_string(),
            assembly64_last_query: None,
            published_playlists_last_synced: None,
            hvsc_known_version: None,
            u64_audio_enabled: false,
            u64_audio_port: 11001,
            force_stereo_2sid: false,
            restart_usb_on_load: false,
            macos_usb_mode: "bridge".to_string(),
            http_remote_enabled: false,
            http_remote_port: 8364,
            window_x: None,
            window_y: None,
            window_width_saved: DEFAULT_WINDOW_WIDTH,
            window_height_saved: DEFAULT_WINDOW_HEIGHT,
            base_font_size: 12.0,
            master_volume: 1.0,
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
                config.skip_rsid = val == "true";
            } else if let Some(rest) = line.strip_prefix("\"default_song_length_secs\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<u32>() {
                    config.default_song_length_secs = n;
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
            } else if let Some(rest) = line.strip_prefix("\"last_stil_file\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.last_stil_file = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"hvsc_root\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.hvsc_root = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"hvsc_known_version\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.hvsc_known_version = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"hvsc_rsync_url\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.hvsc_rsync_url = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"hvsc_last_sync\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.hvsc_last_sync = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"browser_source\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    config.browser_source = s;
                }
            } else if let Some(rest) = line.strip_prefix("\"assembly64_last_query\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.assembly64_last_query = strip_json_string(val);
                }
            } else if let Some(rest) = line.strip_prefix("\"published_playlists_last_synced\"") {
                let val = rest
                    .trim()
                    .trim_start_matches(':')
                    .trim()
                    .trim_end_matches(',');
                if val != "null" {
                    config.published_playlists_last_synced = val.parse::<i64>().ok();
                }
            } else if let Some(rest) = line.strip_prefix("\"u64_audio_enabled\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                config.u64_audio_enabled = val == "true";
            } else if let Some(rest) = line.strip_prefix("\"u64_audio_port\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<u16>() {
                    config.u64_audio_port = n;
                }
            } else if let Some(rest) = line.strip_prefix("\"force_stereo_2sid\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                config.force_stereo_2sid = val == "true";
            } else if let Some(rest) = line.strip_prefix("\"restart_usb_on_load\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                config.restart_usb_on_load = val == "true";
            } else if let Some(rest) = line.strip_prefix("\"macos_usb_mode\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Some(s) = strip_json_string(val) {
                    // Whitelist known values; fall back to default on garbage.
                    if s == "bridge" || s == "direct" {
                        config.macos_usb_mode = s;
                    }
                }
            } else if let Some(rest) = line.strip_prefix("\"http_remote_enabled\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                config.http_remote_enabled = val == "true";
            } else if let Some(rest) = line.strip_prefix("\"http_remote_port\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<u16>() {
                    config.http_remote_port = n;
                }
            } else if let Some(rest) = line.strip_prefix("\"window_x\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.window_x = val.parse::<i32>().ok();
                }
            } else if let Some(rest) = line.strip_prefix("\"window_y\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if val != "null" {
                    config.window_y = val.parse::<i32>().ok();
                }
            } else if let Some(rest) = line.strip_prefix("\"window_width_saved\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<f32>() {
                    // Sanity clamp: ignore absurd values from a corrupt config
                    if n >= 400.0 && n <= 8000.0 {
                        config.window_width_saved = n;
                    }
                }
            } else if let Some(rest) = line.strip_prefix("\"window_height_saved\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<f32>() {
                    if n >= 300.0 && n <= 6000.0 {
                        config.window_height_saved = n;
                    }
                }
            } else if let Some(rest) = line.strip_prefix("\"base_font_size\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<f32>() {
                    config.base_font_size = n.clamp(8.0, 32.0);
                }
            } else if let Some(rest) = line.strip_prefix("\"master_volume\"") {
                let val = rest.trim().trim_start_matches(':').trim();
                if let Ok(n) = val.parse::<f32>() {
                    config.master_volume = n.clamp(0.0, 1.0);
                }
            }
        }

        config
    }

    /// Serialize config to a JSON string.
    fn to_json(&self) -> String {
        let fmt_opt_str = |v: &Option<String>| -> String {
            match v {
                Some(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
                None => "null".to_string(),
            }
        };
        let fmt_opt_i32 = |v: Option<i32>| -> String {
            match v {
                Some(n) => n.to_string(),
                None => "null".to_string(),
            }
        };
        let fmt_opt_i64 = |v: Option<i64>| -> String {
            match v {
                Some(n) => n.to_string(),
                None => "null".to_string(),
            }
        };
        format!(
            concat!(
                "{{\n",
                "  \"skip_rsid\": {},\n",
                "  \"default_song_length_secs\": {},\n",
                "  \"output_engine\": \"{}\",\n",
                "  \"u64_address\": \"{}\",\n",
                "  \"u64_password\": \"{}\",\n",
                "  \"last_sid_dir\": {},\n",
                "  \"last_songlength_dir\": {},\n",
                "  \"last_songlength_file\": {},\n",
                "  \"last_playlist_dir\": {},\n",
                "  \"last_stil_file\": {},\n",
                "  \"hvsc_root\": {},\n",
                "  \"hvsc_known_version\": {},\n",
                "  \"hvsc_rsync_url\": \"{}\",\n",
                "  \"hvsc_last_sync\": {},\n",
                "  \"browser_source\": \"{}\",\n",
                "  \"assembly64_last_query\": {},\n",
                "  \"published_playlists_last_synced\": {},\n",
                "  \"u64_audio_enabled\": {},\n",
                "  \"u64_audio_port\": {},\n",
                "  \"force_stereo_2sid\": {},\n",
                "  \"restart_usb_on_load\": {},\n",
                "  \"macos_usb_mode\": \"{}\",\n",
                "  \"http_remote_enabled\": {},\n",
                "  \"http_remote_port\": {},\n",
                "  \"window_x\": {},\n",
                "  \"window_y\": {},\n",
                "  \"window_width_saved\": {},\n",
                "  \"window_height_saved\": {},\n",
                "  \"base_font_size\": {},\n",
                "  \"master_volume\": {}\n",
                "}}\n",
            ),
            self.skip_rsid,
            self.default_song_length_secs,
            self.output_engine,
            self.u64_address.replace('\\', "\\\\").replace('"', "\\\""),
            self.u64_password.replace('\\', "\\\\").replace('"', "\\\""),
            fmt_opt_str(&self.last_sid_dir),
            fmt_opt_str(&self.last_songlength_dir),
            fmt_opt_str(&self.last_songlength_file),
            fmt_opt_str(&self.last_playlist_dir),
            fmt_opt_str(&self.last_stil_file),
            fmt_opt_str(&self.hvsc_root),
            fmt_opt_str(&self.hvsc_known_version),
            self.hvsc_rsync_url,
            fmt_opt_str(&self.hvsc_last_sync),
            self.browser_source,
            fmt_opt_str(&self.assembly64_last_query),
            fmt_opt_i64(self.published_playlists_last_synced),
            self.u64_audio_enabled,
            self.u64_audio_port,
            self.force_stereo_2sid,
            self.restart_usb_on_load,
            self.macos_usb_mode,
            self.http_remote_enabled,
            self.http_remote_port,
            fmt_opt_i32(self.window_x),
            fmt_opt_i32(self.window_y),
            self.window_width_saved,
            self.window_height_saved,
            self.base_font_size,
            self.master_volume,
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

    /// Remember a STIL.txt file path.
    pub fn remember_stil_path(&mut self, path: &std::path::Path) {
        self.last_stil_file = Some(path.to_string_lossy().into_owned());
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

/// Refresh Songlengths.md5 from the configured HVSC base URL.
/// Pure-Rust path via `hvsc_sync::fetch_hvsc_document` — no subprocess.
pub async fn download_songlength(hvsc_base: String) -> Result<PathBuf, String> {
    let dest =
        songlength_db_path().ok_or_else(|| "Cannot determine config directory".to_string())?;
    crate::hvsc_sync::fetch_hvsc_document(hvsc_base, "Songlengths.md5", dest).await
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
pub fn config_dir() -> Option<PathBuf> {
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

//! Minimal, flush-per-line debug log for diagnosing hard-to-reproduce issues
//! (notably the Windows "Surprise Me" hang).
//!
//! This is intentionally **not** a general logging framework — it writes a
//! small set of targeted diagnostic lines to `phosphor-debug.log` in the
//! config dir, flushing after every line so the *last* line survives even if
//! the app hangs or is force-killed mid-operation.
//!
//! Why a file at all: release builds set `windows_subsystem = "windows"`, so
//! there is no console and stderr is discarded on Windows — plain `eprintln!`
//! diagnostics are invisible on the exact platform where the hang was
//! reported. We still mirror every line to stderr for debug builds and other
//! platforms.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Process-start reference for the `[+<ms>ms]` prefix.
static START: OnceLock<Instant> = OnceLock::new();
/// Resolved log-file path (None if the config dir can't be determined).
static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
/// Serialises concurrent writes from the UI and player threads.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn resolve_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("phosphor-debug.log"))
}

/// Resolve the log-file path and truncate it so each run starts fresh.
/// Call once, early in `main`.
pub fn init() {
    START.get_or_init(Instant::now);
    let path = resolve_path();
    if let Some(p) = &path {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Truncate: one session's worth, so the file never grows unbounded.
        let _ = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p);
    }
    let _ = PATH.set(path);
    log(format_args!("--- phosphor-debug.log session start ---"));
}

/// Append one line (with an elapsed-ms prefix), flush immediately, and mirror
/// to stderr. Prefer the [`dlog!`] macro at call sites.
pub fn log(args: std::fmt::Arguments) {
    let ms = START.get_or_init(Instant::now).elapsed().as_millis();
    let line = format!("[+{ms}ms] {args}");

    // Mirror to stderr (visible in debug builds / when a console is attached).
    eprintln!("{line}");

    // Append + flush per line so a hang can't lose the last message.
    let path = PATH.get_or_init(resolve_path);
    if let Some(p) = path {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// `dlog!("fmt {}", arg)` — ergonomic entry point for a diagnostic line.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        $crate::debug_log::log(std::format_args!($($arg)*))
    };
}

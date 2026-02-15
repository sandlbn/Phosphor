// macOS only: auto-install the usbsid-bridge LaunchDaemon on first launch.
//
// When the bridge socket doesn't exist, this module:
//   1. Locates the bridge binary inside our .app bundle
//   2. Prompts the user for admin credentials via the native macOS dialog
//      (osascript "with administrator privileges")
//   3. Installs the LaunchDaemon plist and starts the daemon
//   4. Waits for the socket to appear
//
// This avoids forcing users to run install scripts from the Terminal.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SOCKET_PATH: &str = "/tmp/usbsid-bridge.sock";
const PLIST_LABEL: &str = "com.phosphor.usbsid-bridge";
const PLIST_DST: &str = "/Library/LaunchDaemons/com.phosphor.usbsid-bridge.plist";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Check if the bridge daemon is reachable (socket exists).
pub fn daemon_running() -> bool {
    Path::new(SOCKET_PATH).exists()
}

/// Check if the LaunchDaemon plist is installed.
fn plist_installed() -> bool {
    Path::new(PLIST_DST).exists()
}

/// Find the bridge binary inside our app bundle.
///
/// Layout:
///   Phosphor.app/Contents/MacOS/phosphor         ← we are here
///   Phosphor.app/Contents/Helpers/usbsid-bridge   ← we want this
///
/// Falls back to /usr/local/bin/usbsid-bridge for non-bundle installs.
fn find_bridge_binary() -> Option<PathBuf> {
    // Try app bundle path first
    if let Ok(exe) = std::env::current_exe() {
        // exe = .../Contents/MacOS/phosphor
        if let Some(macos_dir) = exe.parent() {
            let bundle_bridge = macos_dir
                .parent() // Contents/
                .map(|p| p.join("Helpers").join("usbsid-bridge"));

            if let Some(ref path) = bundle_bridge {
                if path.is_file() {
                    eprintln!(
                        "[daemon-installer] Found bridge in bundle: {}",
                        path.display()
                    );
                    return Some(path.clone());
                }
            }
        }
    }

    // Fallback: check /usr/local/bin (legacy install.sh path)
    let legacy = PathBuf::from("/usr/local/bin/usbsid-bridge");
    if legacy.is_file() {
        eprintln!(
            "[daemon-installer] Found bridge at legacy path: {}",
            legacy.display()
        );
        return Some(legacy);
    }

    None
}

/// Build the shell script that installs the LaunchDaemon.
/// This will be run as root via osascript.
fn build_install_script(bridge_path: &Path) -> String {
    let bridge = bridge_path.display();
    // Use heredoc-style to avoid escaping issues in osascript
    format!(
        r#"
# Stop any existing instance
/bin/launchctl bootout system/{label} 2>/dev/null || \
    /bin/launchctl unload {plist_dst} 2>/dev/null || true
/usr/bin/killall usbsid-bridge 2>/dev/null || true
/bin/rm -f {socket}

# Write the LaunchDaemon plist
/bin/cat > {plist_dst} << 'PLISTEOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bridge}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>/tmp/usbsid-bridge.log</string>
    <key>StandardOutPath</key>
    <string>/tmp/usbsid-bridge.log</string>
</dict>
</plist>
PLISTEOF

/usr/sbin/chown root:wheel {plist_dst}
/bin/chmod 644 {plist_dst}

# Start the daemon
/bin/launchctl bootstrap system {plist_dst} 2>/dev/null || \
    /bin/launchctl load {plist_dst}
"#,
        label = PLIST_LABEL,
        plist_dst = PLIST_DST,
        socket = SOCKET_PATH,
        bridge = bridge,
    )
}

/// Prompt the user for admin credentials and install the daemon.
///
/// Uses `osascript` to show the native macOS authorization dialog
/// ("Phosphor wants to make changes").
fn run_privileged_install(bridge_path: &Path) -> Result<(), String> {
    let script = build_install_script(bridge_path);

    // Write the install script to a temp file — this avoids all quoting/
    // escaping issues with multiline shell scripts inside AppleScript strings.
    let tmp_dir = std::env::temp_dir();
    let tmp_script = tmp_dir.join("phosphor-install-daemon.sh");
    std::fs::write(&tmp_script, &script)
        .map_err(|e| format!("Failed to write temp install script: {e}"))?;

    // Make it executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_script, std::fs::Permissions::from_mode(0o755));
    }

    // osascript: "do shell script ... with administrator privileges"
    // shows the standard macOS padlock/password dialog.
    let apple_script = format!(
        r#"do shell script "/bin/bash '{}'" with administrator privileges with prompt "Phosphor needs to install the USB bridge daemon for USBSID-Pico hardware access.""#,
        tmp_script.display(),
    );

    eprintln!("[daemon-installer] Requesting admin privileges to install bridge daemon...");

    let output = Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .output()
        .map_err(|e| format!("Failed to run osascript: {e}"))?;

    // Clean up temp file
    let _ = std::fs::remove_file(&tmp_script);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // User clicked Cancel → "User canceled" error
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Err(
                "Daemon installation cancelled by user. USB playback will not be available.".into(),
            );
        }
        return Err(format!("Daemon installation failed: {stderr}"));
    }

    eprintln!("[daemon-installer] Install script completed successfully");
    Ok(())
}

/// Wait for the bridge socket to appear after daemon start.
fn wait_for_socket() -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < SOCKET_TIMEOUT {
        if Path::new(SOCKET_PATH).exists() {
            eprintln!("[daemon-installer] Bridge socket ready");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "Bridge daemon started but socket not found after {}s. \
         Check: tail -f /tmp/usbsid-bridge.log",
        SOCKET_TIMEOUT.as_secs()
    ))
}

/// Ensure the bridge daemon is installed and running.
///
/// Called automatically when BridgeDevice::connect() fails.
/// Returns Ok(()) if the daemon is now running, or Err if
/// installation failed or was cancelled by the user.
pub fn ensure_daemon() -> Result<(), String> {
    // Already running? Nothing to do.
    if daemon_running() {
        return Ok(());
    }

    eprintln!("[daemon-installer] Bridge socket not found — attempting auto-install");

    // Find the bridge binary
    let bridge_path = find_bridge_binary().ok_or_else(|| {
        "Cannot find usbsid-bridge binary. \
         Make sure you're running Phosphor from the .app bundle, \
         or install manually with: ./macos/install-daemon.sh"
            .to_string()
    })?;

    // If the plist exists but the socket doesn't, the daemon may have crashed.
    // Try to restart it without re-prompting for admin if possible.
    if plist_installed() {
        eprintln!("[daemon-installer] Plist exists but daemon not running — attempting restart");
        let restart = Command::new("osascript")
            .arg("-e")
            .arg(format!(
                r#"do shell script "/bin/launchctl kickstart -k system/{}" with administrator privileges with prompt "Phosphor needs to restart the USB bridge daemon.""#,
                PLIST_LABEL
            ))
            .output();

        if let Ok(output) = restart {
            if output.status.success() {
                match wait_for_socket() {
                    Ok(()) => return Ok(()),
                    Err(_) => {
                        eprintln!("[daemon-installer] Restart didn't help — doing full reinstall")
                    }
                }
            }
        }
    }

    // Full install
    run_privileged_install(&bridge_path)?;
    wait_for_socket()
}

/// Check if the installed daemon's binary path still matches our bundle.
///
/// After an app update (new bundle path or updated binary), the plist
/// may point to a stale location. This detects that case.
pub fn daemon_needs_update() -> bool {
    if !plist_installed() {
        return true;
    }

    // Read the installed plist and check the ProgramArguments path
    let plist_contents = match std::fs::read_to_string(PLIST_DST) {
        Ok(c) => c,
        Err(_) => return true,
    };

    let current_bridge = match find_bridge_binary() {
        Some(p) => p,
        None => return false, // Can't find our binary, don't try to update
    };

    let current_str = current_bridge.display().to_string();

    // Simple check: does the plist contain our current bridge path?
    if plist_contents.contains(&current_str) {
        false
    } else {
        eprintln!("[daemon-installer] Installed daemon points to different binary — needs update");
        true
    }
}

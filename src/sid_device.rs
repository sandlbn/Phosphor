// Platform-agnostic SID output trait and engine registry.
//
// Current engines:
//   "usb"      — USBSID-Pico hardware. 
//   "emulated" — resid-rs software emulation + cpal audio output
//   "u64"      — Ultimate 64 / Ultimate-II+ via REST API (native SID playback)

/// Common interface for all SID output backends.
pub trait SidDevice: Send {
    fn init(&mut self) -> Result<(), String>;
    fn set_clock_rate(&mut self, is_pal: bool);
    fn reset(&mut self);
    fn set_stereo(&mut self, mode: i32);
    fn write(&mut self, reg: u8, val: u8);

    /// Send a batch of cycle-stamped SID writes.
    /// Each entry is (delta_cycles, register, value).
    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]);

    fn flush(&mut self);
    fn mute(&mut self);
    fn close(&mut self);
    fn shutdown(&mut self);

    /// Close and reopen the USB connection, clearing stale device state.
    /// Used on macOS to reset the USBSID-Pico when loading a new file.
    /// Default no-op — only meaningful for USB-backed engines.
    fn reinit(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Override cycles-per-frame for flush() audio generation.
    /// Only meaningful for emulated engine; hardware devices ignore this.
    fn set_cycles_per_frame(&mut self, _cycles: u32) {}

    /// Send a complete SID file for native playback on real hardware.
    ///
    /// Returns `Ok(true)` if the engine handles playback natively
    /// (host should skip CPU emulation). Returns `Ok(false)` by default.
    fn play_sid_native(&mut self, _data: &[u8], _song: u16) -> Result<bool, String> {
        Ok(false)
    }

    /// Freeze the machine mid-frame (clock + SID output paused).
    /// Default no-op — only meaningful for native engines (U64).
    fn pause_machine(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Resume a previously paused machine, continuing from exactly where it stopped.
    /// Default no-op — only meaningful for native engines (U64).
    fn resume_machine(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Start streaming audio from the device back to this machine on `port` (UDP).
    /// Default no-op — only meaningful for native engines (U64).
    fn start_audio(&mut self, _port: u16) -> Result<(), String> {
        Ok(())
    }

    /// Stop the audio stream started by `start_audio`.
    /// Default no-op — only meaningful for native engines (U64).
    fn stop_audio(&mut self) {}

    /// Read the elapsed playback time, in seconds, that the U64 firmware
    /// renders to its on-screen player UI. `None` means either the device
    /// doesn't expose one (USB / emulated / sidlite) or the layout couldn't
    /// be validated this run.
    fn read_screen_elapsed(&mut self) -> Option<u32> {
        None
    }

    /// Read the on-screen "total song length" the U64 firmware shows next
    /// to the elapsed time. Useful as a duration fallback when HVSC has no
    /// entry for the current tune. Default no-op for non-U64 engines.
    fn read_screen_total(&mut self) -> Option<u32> {
        None
    }

    /// Run a USBSID-Pico configuration operation against this device.
    ///
    /// Implemented only by the USB engines (`DirectDevice`, `BridgeDevice`).
    /// Other engines return an "unsupported" error so the GUI can surface
    /// it cleanly.
    ///
    /// Returns `Some(snapshot)` after a successful `Refresh`, `None` for
    /// action-only operations (preset, save, reset, auto-detect) that
    /// succeed without producing a payload.
    fn run_device_config(
        &mut self,
        _op: &crate::player::DeviceConfigCmd,
    ) -> Result<Option<crate::ui::DeviceConfigSnapshot>, String> {
        Err("device configuration is only available on the USB engine".into())
    }

    /// Whether the device is reachable. Default `true` for engines that are
    /// always-local (USB / emulated / sidlite). The U64 implementation flips
    /// this to `false` whenever a REST call fails (network drop, device
    /// reboot, etc.) and back to `true` on the next successful call.  Used
    /// by the GUI to render a "Disconnected" indicator.
    fn is_connected(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Engine registry
// ─────────────────────────────────────────────────────────────────────────────

/// List of engine names available at runtime.
pub fn available_engines() -> Vec<&'static str> {
    vec!["usb", "emulated", "sidlite", "u64"]
}

/// Create a SidDevice for the given engine name.
///
/// "auto" tries USB first, then emulated, then U64 (if address configured).
/// `macos_usb_mode` is "bridge" or "direct"; ignored on Linux/Windows.
pub fn create_engine(
    name: &str,
    u64_address: &str,
    u64_password: &str,
    macos_usb_mode: &str,
) -> Result<Box<dyn SidDevice>, String> {
    match name {
        "auto" => create_auto(u64_address, u64_password, macos_usb_mode),
        "usb" => create_usb(macos_usb_mode),
        "emulated" => create_emulated(),
        "sidlite" => create_sidlite(),
        "u64" => create_u64(u64_address, u64_password),
        other => Err(format!(
            "Unknown engine '{}'. Available: {:?}",
            other,
            available_engines()
        )),
    }
}

/// Auto: try USB → emulated → U64 (if address set).
fn create_auto(
    u64_address: &str,
    u64_password: &str,
    macos_usb_mode: &str,
) -> Result<Box<dyn SidDevice>, String> {
    match create_usb(macos_usb_mode) {
        Ok(dev) => return Ok(dev),
        Err(e) => eprintln!("[phosphor] USB unavailable: {e}"),
    }

    eprintln!("[phosphor] Falling back to software SID emulation");
    match create_emulated() {
        Ok(dev) => return Ok(dev),
        Err(e) => eprintln!("[phosphor] Emulation unavailable: {e}"),
    }

    if !u64_address.is_empty() {
        eprintln!("[phosphor] Trying Ultimate 64 at {u64_address}");
        return create_u64(u64_address, u64_password);
    }

    Err("No SID engine could be initialised".into())
}

/// USB hardware. On macOS the `macos_usb_mode` decides whether we go through
/// the root bridge daemon or open the device in-process via libusb
fn create_usb(_macos_usb_mode: &str) -> Result<Box<dyn SidDevice>, String> {
    #[cfg(all(feature = "usb", target_os = "macos"))]
    {
        if _macos_usb_mode == "direct" {
            eprintln!("[phosphor] Opening USBSID-Pico directly via libusb (macOS direct mode)…");
            // No fallback to the daemon — the user explicitly picked
            // "Direct (no daemon)". If libusb can't see the device,
            // surface the real error and let them decide whether to
            // replug or switch to Bridge mode.
            return Ok(Box::new(crate::sid_direct::DirectDevice::open().map_err(|e| {
                format!(
                    "{e}\n\
                     Replug the USB cable, or switch Settings → macOS USB transport → Bridge."
                )
            })?));
        }
        eprintln!("[phosphor] Connecting to usbsid-bridge daemon…");
        let dev = crate::usb_bridge::BridgeDevice::connect()?;
        return Ok(Box::new(dev));
    }

    #[cfg(all(feature = "usb", not(target_os = "macos")))]
    {
        eprintln!("[phosphor] Opening USBSID-Pico directly…");
        let dev = crate::sid_direct::DirectDevice::open()?;
        return Ok(Box::new(dev));
    }

    #[cfg(not(feature = "usb"))]
    Err("USB engine not compiled in. Build with --features usb".into())
}

/// Software SID emulation (resid-rs + cpal).
fn create_emulated() -> Result<Box<dyn SidDevice>, String> {
    eprintln!("[phosphor] Opening software SID (resid-rs + cpal)…");
    let dev = crate::sid_emulated::EmulatedDevice::open()?;
    Ok(Box::new(dev))
}

/// SIDLite emulation from libsidplayfp (sidlite-sys + cpal).
fn create_sidlite() -> Result<Box<dyn SidDevice>, String> {
    eprintln!("[phosphor] Opening SIDLite engine (libsidplayfp + cpal)...");
    let dev = crate::sid_sidlite::SidLiteDevice::open()?;
    Ok(Box::new(dev))
}

/// Ultimate 64 via REST API.
fn create_u64(address: &str, password: &str) -> Result<Box<dyn SidDevice>, String> {
    eprintln!("[phosphor] Connecting to Ultimate 64 at {address}…");
    let dev = crate::sid_u64::U64Device::connect(address, password)?;
    Ok(Box::new(dev))
}

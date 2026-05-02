// Platform-agnostic SID output trait and engine registry.
//
// Current engines:
//   "usb"      — USBSID-Pico hardware (BridgeDevice on macOS, DirectDevice elsewhere)
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
pub fn create_engine(
    name: &str,
    u64_address: &str,
    u64_password: &str,
) -> Result<Box<dyn SidDevice>, String> {
    match name {
        "auto" => create_auto(u64_address, u64_password),
        "usb" => create_usb(),
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
fn create_auto(u64_address: &str, u64_password: &str) -> Result<Box<dyn SidDevice>, String> {
    match create_usb() {
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

/// USB hardware — macOS uses BridgeDevice, others use DirectDevice.
fn create_usb() -> Result<Box<dyn SidDevice>, String> {
    #[cfg(all(feature = "usb", target_os = "macos"))]
    {
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

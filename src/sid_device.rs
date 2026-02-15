// Platform-agnostic SID output trait and engine registry.
//
// Current engines:
//   "usb"      — USBSID-Pico hardware (BridgeDevice on macOS, DirectDevice elsewhere)
//   "emulated" — resid-rs software emulation + cpal audio output
//
// To add a new engine (e.g. "u64" for Ultimate64 REST API):
//   1. Create src/sid_u64.rs implementing SidDevice
//   2. Add a feature flag in Cargo.toml:  u64 = ["dep:reqwest"]
//   3. Add a match arm in create_engine() below
//   4. Add a cfg(feature) mod declaration in main.rs

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
}

// ─────────────────────────────────────────────────────────────────────────────
//  Engine registry
// ─────────────────────────────────────────────────────────────────────────────

/// List of engine names that are available in this build.
/// Useful for populating a UI dropdown or validating config values.
pub fn available_engines() -> Vec<&'static str> {
    let mut engines = Vec::new();

    #[cfg(feature = "usb")]
    engines.push("usb");

    #[cfg(feature = "emulated")]
    engines.push("emulated");

    // To add a new engine, append here:
    // #[cfg(feature = "u64")]
    // engines.push("u64");

    engines
}

/// Create a SidDevice for the given engine name.
///
/// Known engines: "usb", "emulated".
/// "auto" tries USB first, falls back to emulated.
///
/// Returns an error if the requested engine isn't compiled in or fails to open.
pub fn create_engine(name: &str) -> Result<Box<dyn SidDevice>, String> {
    match name {
        "auto" => create_auto(),
        "usb" => create_usb(),
        "emulated" => create_emulated(),

        // ── Add new engines here ─────────────────────────────────────
        // "u64" => {
        //     #[cfg(feature = "u64")]
        //     { crate::sid_u64::U64Device::open().map(|d| Box::new(d) as _) }
        //     #[cfg(not(feature = "u64"))]
        //     { Err("Engine 'u64' not compiled in. Build with --features u64".into()) }
        // }
        other => Err(format!(
            "Unknown engine '{}'. Available: {:?}",
            other,
            available_engines()
        )),
    }
}

/// Try USB hardware first, fall back to emulated.
fn create_auto() -> Result<Box<dyn SidDevice>, String> {
    // Try USB if compiled in.
    #[cfg(feature = "usb")]
    {
        match create_usb() {
            Ok(dev) => return Ok(dev),
            Err(e) => eprintln!("[phosphor] USB unavailable: {e}"),
        }
    }

    // Fall back to emulated.
    #[cfg(feature = "emulated")]
    {
        eprintln!("[phosphor] Falling back to software SID emulation");
        return create_emulated();
    }

    #[cfg(not(any(feature = "usb", feature = "emulated")))]
    Err("No SID engines available. Build with --features usb and/or --features emulated".into())
}

/// Open the USB hardware backend.
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
    Err("Engine 'usb' not compiled in. Build with --features usb".into())
}

/// Open the software SID emulation backend.
fn create_emulated() -> Result<Box<dyn SidDevice>, String> {
    #[cfg(feature = "emulated")]
    {
        eprintln!("[phosphor] Opening software SID (resid-rs + cpal)…");
        let dev = crate::sid_emulated::EmulatedDevice::open()?;
        return Ok(Box::new(dev));
    }

    #[cfg(not(feature = "emulated"))]
    Err("Engine 'emulated' not compiled in. Build with --features emulated".into())
}

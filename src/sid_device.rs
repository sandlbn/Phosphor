// Platform-agnostic SID hardware trait.
//
// macOS:   BridgeDevice — connects to a LaunchDaemon via Unix socket
//          (required because USB access needs root).
// Windows: DirectDevice — owns UsbSid directly in the player thread
//          (libusb works from userspace via WinUSB/Zadig).
// Linux:   DirectDevice — same as Windows (udev rule grants access).

/// Common interface for SID hardware backends.
pub trait SidDevice: Send {
    fn init(&mut self) -> Result<(), String>;
    fn set_clock_rate(&mut self, is_pal: bool);
    fn reset(&mut self);
    fn set_stereo(&mut self, mode: i32);
    fn write(&mut self, reg: u8, val: u8);

    /// Send a batch of cycle-stamped SID writes, then flush to hardware.
    /// Each entry is (delta_cycles, register, value).
    /// The implementation packs writes into 64-byte bulk USB packets.
    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]);

    fn flush(&mut self);
    fn mute(&mut self);
    fn close(&mut self);
    fn shutdown(&mut self);
}

/// Create the appropriate SidDevice for the current platform.
pub fn create_device() -> Result<Box<dyn SidDevice>, String> {
    #[cfg(target_os = "macos")]
    {
        eprintln!("[phosphor] Connecting to usbsid-bridge daemon...");
        let dev = crate::usb_bridge::BridgeDevice::connect()?;
        Ok(Box::new(dev))
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("[phosphor] Opening USBSID-Pico directly...");
        let dev = crate::sid_direct::DirectDevice::open()?;
        Ok(Box::new(dev))
    }
}

// Windows / Linux: direct USB access to USBSID-Pico.
// No bridge daemon needed — libusb works from userspace.
//
// Uses the driver's built-in threaded ring buffer with cycle-accurate
// writes, matching the C++ players (SidBerry, SparkPlug, etc.):
//   init(true, true)          → start background writer thread
//   write_ring_cycled(r,v,c)  → push to ring buffer per write
//   set_flush()               → signal end-of-frame flush

use crate::sid_device::SidDevice;
use usbsid_pico::{ClockSpeed, UsbSid};
use usbsid_pico_config::transport::{Transport, TransportError};

/// Turn a libusb init failure into something the user can act on.
///
/// A board that is plugged in and enumerating still fails to open when the
/// usbfs node is root-only, which is the default on a machine that never
/// installed the udev rule. libusb reports that as a bare "Access denied",
/// which reads like a broken cable rather than a one-line fix, so name the
/// actual remedy on the platforms where it applies.
fn open_error(msg: &str) -> String {
    let lower = msg.to_ascii_lowercase();
    let permission_denied = lower.contains("access")
        || lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("insufficient");

    if !permission_denied {
        return format!("USB init failed: {msg}");
    }

    #[cfg(target_os = "linux")]
    return format!(
        "USB init failed: {msg}\n\
         The USBSID-Pico is connected but not accessible to your user.\n\
         Install the udev rule and re-apply it to the attached board:\n\
         \x20 sudo cp packaging/99-usbsid-pico.rules /etc/udev/rules.d/\n\
         \x20 sudo udevadm control --reload-rules\n\
         \x20 sudo udevadm trigger --subsystem-match=usb --attr-match=idVendor=cafe\n\
         The Arch package and the .deb both install this rule for you."
    );

    #[cfg(target_os = "windows")]
    return format!(
        "USB init failed: {msg}\n\
         The USBSID-Pico needs the WinUSB driver. Install it with Zadig\n\
         (https://zadig.akeo.ie/), selecting the USBSID-Pico interface."
    );

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    return format!("USB init failed: {msg}");
}

pub struct DirectDevice {
    dev: UsbSid,
    /// Tracked connection state for the GUI's "Disconnected" indicator.
    /// Starts `true`, flips to `false` the first time a USB call errors
    /// (likely a yank or device reset), and back to `true` on the next
    /// successful call. Logged on transitions so stderr has a breadcrumb.
    connected: bool,
}

impl DirectDevice {
    pub fn open() -> Result<Self, String> {
        let mut dev = UsbSid::new();
        dev.init(true, true).map_err(|e| open_error(&e.to_string()))?;
        eprintln!("[sid-direct] USBSID-Pico opened (threaded, cycled)");
        Ok(Self {
            dev,
            connected: true,
        })
    }

    /// Update `connected` based on a fresh USB call result and log the
    /// transition. Used after any libusb-touching call so the GUI's
    /// "Disconnected" pill reflects reality.
    fn note_call<T, E: std::fmt::Display>(&mut self, label: &str, result: &Result<T, E>) {
        let now_connected = result.is_ok();
        if now_connected != self.connected {
            if now_connected {
                eprintln!("[sid-direct] reconnected (via {label})");
            } else if let Err(e) = result {
                eprintln!("[sid-direct] disconnected (via {label}): {e}");
            }
            self.connected = now_connected;
        }
    }
}

impl SidDevice for DirectDevice {
    fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn set_clock_rate(&mut self, is_pal: bool) {
        let speed = if is_pal {
            ClockSpeed::Pal as i64
        } else {
            ClockSpeed::Ntsc as i64
        };
        self.dev.set_clock_rate(speed, true);
    }

    fn reset(&mut self) {
        // Zero every SID register first (works on cloned/emulated SIDs that
        // ignore the RES pin), then toggle RES for real chips.
        self.dev.reset_all_registers();
        self.dev.reset();
    }

    fn set_stereo(&mut self, mode: i32) {
        self.dev.set_stereo(mode);
    }

    fn write(&mut self, reg: u8, val: u8) {
        // Use single_write (not write()) because write() is blocked
        // in threaded mode. single_write goes straight to USB.
        let buf = [0x00, reg, val];
        let r = self.dev.single_write(&buf);
        self.note_call("write", &r);
    }

    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]) {
        for &(cycles, reg, val) in writes {
            let r = self.dev.write_ring_cycled(reg, val, cycles);
            self.note_call("ring_cycled", &r);
            // If the device went away mid-batch, stop hammering it —
            // subsequent writes will fail the same way and just spam
            // the log. The next user-initiated action will retry.
            if !self.connected {
                break;
            }
        }
    }

    fn flush(&mut self) {
        self.dev.set_flush();
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn mute(&mut self) {
        self.dev.mute();
    }

    fn close(&mut self) {
        self.dev.set_flush();
        self.dev.mute();
        self.dev.reset_all_registers();
        self.dev.reset();
        self.dev.close();
    }

    fn shutdown(&mut self) {
        self.close();
    }

    fn run_device_config(
        &mut self,
        op: &crate::player::DeviceConfigCmd,
    ) -> Result<Option<crate::ui::DeviceConfigSnapshot>, String> {
        self.run_device_config_op(op)
    }
}

impl Drop for DirectDevice {
    fn drop(&mut self) {
        self.dev.mute();
        self.dev.reset();
        self.dev.close();
    }
}

/// Adapter that lets the `usbsid-pico-config` crate talk to the same USB
/// session this `DirectDevice` already owns. The Phosphor playback path
/// and the config path share a single libusb connection — the OS would
/// reject a parallel session on the same device.
impl Transport for DirectDevice {
    fn send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.dev
            .send_raw(bytes)
            .map_err(|e| TransportError::Io(format!("send_raw: {e}")))
    }

    fn recv(&mut self, len: usize) -> Result<Vec<u8>, TransportError> {
        self.dev
            .recv_raw(len)
            .map_err(|e| TransportError::Io(format!("recv_raw: {e}")))
    }

    fn drain(&mut self) -> Result<(), TransportError> {
        // Up to 8 short reads with a 10 ms timeout each. Drain is
        // best-effort: any error (timeout / disconnect / anything else)
        // just means "stop draining"; bubbling it up would force callers
        // to handle a non-actionable failure on every operation.
        for _ in 0..8 {
            match self.dev.recv_raw_timeout(64, 10) {
                Ok(buf) if buf.is_empty() => return Ok(()),
                Ok(_) => continue,
                Err(_) => return Ok(()),
            }
        }
        Ok(())
    }
}

impl DirectDevice {
    /// Forward to the shared config helper, borrowing `self` as Transport.
    pub fn run_device_config_op(
        &mut self,
        op: &crate::player::DeviceConfigCmd,
    ) -> Result<Option<crate::ui::DeviceConfigSnapshot>, String> {
        crate::device_config::run(&mut *self, op)
    }
}

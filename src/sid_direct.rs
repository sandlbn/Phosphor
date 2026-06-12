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

pub struct DirectDevice {
    dev: UsbSid,
}

impl DirectDevice {
    pub fn open() -> Result<Self, String> {
        let mut dev = UsbSid::new();
        dev.init(true, true)
            .map_err(|e| format!("USB init failed: {e}"))?;
        eprintln!("[sid-direct] USBSID-Pico opened (threaded, cycled)");
        Ok(Self { dev })
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
        self.dev.reset();
    }

    fn set_stereo(&mut self, mode: i32) {
        self.dev.set_stereo(mode);
    }

    fn write(&mut self, reg: u8, val: u8) {
        // Use single_write (not write()) because write() is blocked
        // in threaded mode. single_write goes straight to USB.
        let buf = [0x00, reg, val];
        let _ = self.dev.single_write(&buf);
    }

    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]) {
        for &(cycles, reg, val) in writes {
            let _ = self.dev.write_ring_cycled(reg, val, cycles);
        }
    }

    fn flush(&mut self) {
        self.dev.set_flush();
    }

    fn mute(&mut self) {
        self.dev.mute();
    }

    fn close(&mut self) {
        self.dev.set_flush();
        self.dev.mute();
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
        // Up to 8 short reads with a 10 ms timeout each. Stop on the first
        // read that times out (= pipe is idle) or any successful read of
        // zero bytes. Any actual transport error is bubbled up.
        for _ in 0..8 {
            match self.dev.recv_raw_timeout(64, 10) {
                Ok(buf) if buf.is_empty() => return Ok(()),
                Ok(_) => continue,
                Err(usbsid_pico::UsbSidError::Usb(rusb::Error::Timeout)) => return Ok(()),
                Err(e) => return Err(TransportError::Io(format!("drain: {e}"))),
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

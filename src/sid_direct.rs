// Windows / Linux: direct USB access to USBSID-Pico.
// No bridge daemon needed — libusb works from userspace.
//
// Windows: user must install WinUSB driver via Zadig for VID=0xCAFE.
// Linux:   user must add a udev rule (or run as root once).
//
// Uses the driver's built-in threaded ring buffer with cycle-accurate
// writes, matching SidBerry's approach:
//   init(true, true)          → start background writer thread
//   write_ring_cycled(r,v,c)  → push to ring buffer per write
//   set_flush()               → signal end-of-frame flush

use crate::sid_device::SidDevice;
use usbsid_pico::{ClockSpeed, UsbSid};

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
        // Already initialized in open()
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
        let _ = self.dev.write(reg, val);
    }

    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]) {
        // Push each write into the driver's ring buffer.
        // The background thread drains it and packs OP_CYCLED_WRITE
        // packets to USB asynchronously.
        for &(cycles, reg, val) in writes {
            let _ = self.dev.write_ring_cycled(reg, val, cycles);
        }
    }

    fn flush(&mut self) {
        // Signal the background thread to flush any remaining buffered
        // writes — called at end of each frame, same as SidBerry's
        // USBSID_SetFlush().
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
}

impl Drop for DirectDevice {
    fn drop(&mut self) {
        self.dev.mute();
        self.dev.reset();
        self.dev.close();
    }
}

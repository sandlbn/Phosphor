// Windows / Linux: direct USB access to USBSID-Pico.
// No bridge daemon needed — libusb works from userspace.
//
// Windows: user must install WinUSB driver via Zadig for VID=0xCAFE.
// Linux:   user must add a udev rule (or run as root once).
//
// Batches SID writes into 64-byte USB bulk packets, same format
// as the macOS bridge daemon uses internally.

use crate::sid_device::SidDevice;
use usbsid_pico::{ClockSpeed, UsbSid};

/// Max reg/val pairs per 64-byte USB packet: (64 - 1 header) / 2 = 31
const MAX_PAIRS_PER_PACKET: usize = 31;

pub struct DirectDevice {
    dev: UsbSid,
    ring_buf: Vec<(u8, u8)>,
}

impl DirectDevice {
    pub fn open() -> Result<Self, String> {
        let mut dev = UsbSid::new();
        dev.init(false, false)
            .map_err(|e| format!("USB init failed: {e}"))?;
        eprintln!("[sid-direct] USBSID-Pico opened");
        Ok(Self {
            dev,
            ring_buf: Vec::with_capacity(128),
        })
    }

    /// Pack buffered writes into 64-byte bulk USB packets.
    /// Each packet: [data_len, reg1, val1, reg2, val2, ...]
    /// where data_len = num_pairs * 2 (OP_WRITE = 0, so top bits are 0).
    fn flush_ring_buf(&mut self) {
        if self.ring_buf.is_empty() {
            return;
        }

        let mut pkt = [0u8; 64];

        for chunk in self.ring_buf.chunks(MAX_PAIRS_PER_PACKET) {
            let data_len = (chunk.len() * 2) as u8;
            pkt[0] = data_len;
            for (i, &(reg, val)) in chunk.iter().enumerate() {
                pkt[1 + i * 2] = reg;
                pkt[2 + i * 2] = val;
            }
            let total = 1 + chunk.len() * 2;
            let _ = self.dev.single_write(&pkt[..total]);
        }

        self.ring_buf.clear();
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
        if writes.is_empty() {
            return;
        }

        // Buffer all writes, ignoring cycle values (frame pacing by player)
        for &(_cycles, reg, val) in writes {
            self.ring_buf.push((reg, val));
        }

        // Immediately flush as bulk USB packets
        self.flush_ring_buf();
    }

    fn flush(&mut self) {
        self.flush_ring_buf();
    }

    fn mute(&mut self) {
        self.dev.mute();
    }

    fn close(&mut self) {
        self.flush_ring_buf();
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

// Windows / Linux: direct USB access to USBSID-Pico.
// No bridge daemon needed — libusb works from userspace.
//
// Windows: user must install WinUSB driver via Zadig for VID=0xCAFE.
// Linux:   user must add a udev rule (or run as root once).
//
// Batches SID writes into 64-byte OP_CYCLED_WRITE USB packets via
// single_write().  Each write carries a cycle delta so the firmware
// can space them accurately within the frame.
//
// We use init(false, false) + manual packet packing rather than the
// driver's threaded ring buffer because the thread transport requires
// opening a second USB handle, which fails on Windows (WinUSB).
// The firmware receives identical OP_CYCLED_WRITE packets either way.

use crate::sid_device::SidDevice;
use usbsid_pico::{ClockSpeed, UsbSid};

/// OP_CYCLED_WRITE opcode (top 2 bits = 0b10).
const OP_CYCLED_WRITE: u8 = 2;

/// Max cycled-write tuples per 64-byte USB packet: (64 - 1 header) / 4 = 15
const MAX_CYCLED_PER_PACKET: usize = 15;

pub struct DirectDevice {
    dev: UsbSid,
}

impl DirectDevice {
    pub fn open() -> Result<Self, String> {
        let mut dev = UsbSid::new();
        dev.init(false, false)
            .map_err(|e| format!("USB init failed: {e}"))?;
        eprintln!("[sid-direct] USBSID-Pico opened");
        Ok(Self { dev })
    }

    /// Pack writes into 64-byte OP_CYCLED_WRITE USB bulk packets.
    ///
    /// Packet format (matches firmware expectation):
    ///   byte 0:    (OP_CYCLED_WRITE << 6) | byte_count
    ///   bytes 1+:  [reg, val, cycles_hi, cycles_lo] × N
    ///
    /// Max 15 tuples per packet (15 × 4 + 1 = 61 bytes).
    fn send_cycled_packets(&self, writes: &[(u16, u8, u8)]) {
        let mut pkt = [0u8; 64];

        for chunk in writes.chunks(MAX_CYCLED_PER_PACKET) {
            let data_len = (chunk.len() * 4) as u8;
            pkt[0] = (OP_CYCLED_WRITE << 6) | data_len;
            for (i, &(cycles, reg, val)) in chunk.iter().enumerate() {
                pkt[1 + i * 4] = reg;
                pkt[2 + i * 4] = val;
                pkt[3 + i * 4] = (cycles >> 8) as u8;
                pkt[4 + i * 4] = (cycles & 0xFF) as u8;
            }
            let total = 1 + chunk.len() * 4;
            let _ = self.dev.single_write(&pkt[..total]);
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
        self.send_cycled_packets(writes);
    }

    fn flush(&mut self) {
        // Manual mode: writes already sent in ring_cycled(), nothing to flush.
        // The per-frame flush() call from the player loop is still useful
        // when the bridge daemon uses threaded mode on macOS.
    }

    fn mute(&mut self) {
        self.dev.mute();
    }

    fn close(&mut self) {
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

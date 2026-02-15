// Ultimate 64 SID output via REST API.
//
// Sends the entire SID file to the Ultimate 64 (or Ultimate-II+) device
// over the network. The C64 hardware plays the SID natively — no CPU
// emulation or per-register writes needed on the host side.
//
// Requires: Ultimate 64 / Ultimate-II+ with firmware 3.11+ and REST API
// enabled. The device must be reachable on the local network.

use crate::sid_device::SidDevice;
use ultimate64::Rest;
use url::Host;

/// SID output device that sends files to an Ultimate 64 via REST API.
pub struct U64Device {
    rest: Rest,
}

impl U64Device {
    /// Connect to an Ultimate 64 at the given address.
    ///
    /// `address` is an IP or hostname (e.g. "192.168.1.64").
    /// `password` is optional — only needed if the device has a network password set.
    pub fn connect(address: &str, password: &str) -> Result<Self, String> {
        if address.is_empty() {
            return Err(
                "No Ultimate 64 address configured. Set it in Settings → U64 IP Address."
                    .to_string(),
            );
        }

        let host = Host::parse(address)
            .map_err(|e| format!("Invalid Ultimate 64 address '{}': {}", address, e))?;

        let pass = if password.is_empty() {
            None
        } else {
            Some(password.to_string())
        };

        let rest = Rest::new(&host, pass)
            .map_err(|e| format!("Cannot connect to Ultimate 64 at {}: {}", address, e))?;

        // Quick connectivity check: request device info.
        match rest.version() {
            Ok(ver) => eprintln!("[u64] Connected to Ultimate 64 at {} ({})", address, ver),
            Err(e) => {
                eprintln!("[u64] Warning: device at {} not responding: {}", address, e);
                // Don't fail here — the device might come online later.
            }
        }

        Ok(Self { rest })
    }
}

impl SidDevice for U64Device {
    fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn set_clock_rate(&mut self, _is_pal: bool) {
        // U64 handles PAL/NTSC natively from the SID file header.
    }

    fn reset(&mut self) {
        if let Err(e) = self.rest.reset() {
            eprintln!("[u64] Reset failed: {e}");
        }
    }

    fn set_stereo(&mut self, _mode: i32) {
        // U64 handles multi-SID natively.
    }

    fn write(&mut self, _reg: u8, _val: u8) {
        // No-op: U64 runs its own SID player on the real C64 hardware.
    }

    fn ring_cycled(&mut self, _writes: &[(u16, u8, u8)]) {
        // No-op: native playback — register writes handled by the C64.
    }

    fn flush(&mut self) {
        // No-op for native playback.
    }

    fn mute(&mut self) {
        // Reset stops all SID output.
        self.reset();
    }

    fn close(&mut self) {
        self.reset();
    }

    fn shutdown(&mut self) {
        self.reset();
    }

    /// Send the entire SID file to the Ultimate 64 for native playback.
    /// Returns `Ok(true)` on success, meaning the host should skip CPU
    /// emulation and let the real hardware handle everything.
    fn play_sid_native(&mut self, data: &[u8], song: u16) -> Result<bool, String> {
        let song_num = if song > 0 { Some(song as u8) } else { None };

        self.rest
            .sid_play(data, song_num)
            .map_err(|e| format!("U64 sid_play failed: {e}"))?;

        eprintln!("[u64] SID file sent ({} bytes, song {})", data.len(), song);
        Ok(true)
    }
}

impl Drop for U64Device {
    fn drop(&mut self) {
        // Best-effort reset on drop — don't panic if device is unreachable.
        let _ = self.rest.reset();
        eprintln!("[u64] Ultimate 64 device closed");
    }
}

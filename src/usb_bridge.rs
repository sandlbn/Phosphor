// macOS only: connects to the usbsid-bridge LaunchDaemon
// via a Unix domain socket. Fixed-size protocol.
//
// CMD_RING writes are collected locally in the daemon and pushed to the
// driver's ring buffer in a single batch on CMD_FLUSH — one mutex lock
// instead of hundreds.  The daemon blocks until the writer thread has
// drained all writes to USB, then sends RESP_OK back (backpressure).

use crate::sid_device::SidDevice;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use usbsid_pico_config::transport::{Transport, TransportError};

const SOCKET_PATH: &str = "/tmp/usbsid-bridge.sock";

const CMD_INIT: u8 = 0x01;
const CMD_CLOCK: u8 = 0x02;
const CMD_RESET: u8 = 0x03;
const CMD_STEREO: u8 = 0x04;
const CMD_WRITE: u8 = 0x05;
const CMD_MUTE: u8 = 0x07;
const CMD_CLOSE: u8 = 0x08;
const CMD_RING: u8 = 0x09;
const CMD_FLUSH: u8 = 0x0A;
/// Raw config-protocol passthrough (Phosphor "Device Config" tab).
const CMD_CFG_SEND: u8 = 0x0B;
const CMD_CFG_RECV: u8 = 0x0C;
const CMD_CFG_DRAIN: u8 = 0x0D;
const CMD_QUIT: u8 = 0xFF;

const RESP_OK: u8 = 0x00;
#[allow(dead_code)]
const RESP_ERR: u8 = 0x01;

pub struct BridgeDevice {
    stream: UnixStream,
}

impl BridgeDevice {
    pub fn connect() -> Result<Self, String> {
        eprintln!("[usb-bridge] connecting to {SOCKET_PATH}");

        // First attempt
        match UnixStream::connect(SOCKET_PATH) {
            Ok(stream) => {
                eprintln!("[usb-bridge] connected");
                return Ok(Self { stream });
            }
            Err(first_err) => {
                eprintln!("[usb-bridge] socket not available: {first_err}");

                // Auto-install the daemon (prompts user for admin password)
                eprintln!("[usb-bridge] attempting automatic daemon installation...");
                crate::daemon_installer::ensure_daemon()?;

                // Retry after install
                let stream = UnixStream::connect(SOCKET_PATH).map_err(|e| {
                    format!(
                        "Daemon was installed but still cannot connect to {SOCKET_PATH}: {e}\n\
                         Check logs: tail -f /tmp/usbsid-bridge.log"
                    )
                })?;
                eprintln!("[usb-bridge] connected (after daemon install)");
                return Ok(Self { stream });
            }
        }
    }

    fn send_cmd(&mut self, data: &[u8]) {
        let _ = self.stream.write_all(data);
        let _ = self.stream.flush();
    }

    fn read_response(&mut self) -> Result<(), String> {
        let mut resp = [0u8; 1];
        if self.stream.read_exact(&mut resp).is_err() {
            return Err("Bridge daemon disconnected".into());
        }
        if resp[0] == RESP_OK {
            return Ok(());
        }
        let mut len_buf = [0u8; 1];
        if self.stream.read_exact(&mut len_buf).is_err() {
            return Err("Bridge error (no message)".into());
        }
        let msg_len = len_buf[0] as usize;
        let mut msg_buf = vec![0u8; msg_len];
        if self.stream.read_exact(&mut msg_buf).is_err() {
            return Err("Bridge error (truncated)".into());
        }
        Err(String::from_utf8_lossy(&msg_buf).to_string())
    }
}

impl SidDevice for BridgeDevice {
    fn init(&mut self) -> Result<(), String> {
        self.send_cmd(&[CMD_INIT]);
        self.read_response()
    }

    fn set_clock_rate(&mut self, is_pal: bool) {
        self.send_cmd(&[CMD_CLOCK, if is_pal { 1 } else { 0 }]);
        let _ = self.read_response();
    }

    fn reset(&mut self) {
        self.send_cmd(&[CMD_RESET]);
        let _ = self.read_response();
    }

    fn set_stereo(&mut self, mode: i32) {
        self.send_cmd(&[CMD_STEREO, mode as u8]);
        let _ = self.read_response();
    }

    fn write(&mut self, reg: u8, val: u8) {
        self.send_cmd(&[CMD_WRITE, reg, val]);
    }

    fn ring_cycled(&mut self, writes: &[(u16, u8, u8)]) {
        if writes.is_empty() {
            return;
        }

        let mut buf = Vec::with_capacity(writes.len() * 5);
        for &(cycles, reg, val) in writes {
            buf.push(CMD_RING);
            buf.push(reg);
            buf.push(val);
            buf.push((cycles >> 8) as u8);
            buf.push((cycles & 0xFF) as u8);
        }
        // No CMD_FLUSH here — the player loop calls flush() after this.

        let t0 = std::time::Instant::now();
        let _ = self.stream.write_all(&buf);
        let _ = self.stream.flush();
        let dt = t0.elapsed();
        if dt.as_millis() > 10 {
            eprintln!(
                "[usb-bridge] SLOW ring_cycled: {} writes, {:.1}ms",
                writes.len(),
                dt.as_secs_f64() * 1000.0,
            );
        }
    }

    fn flush(&mut self) {
        let t0 = std::time::Instant::now();
        let _ = self.stream.write_all(&[CMD_FLUSH]);
        let _ = self.stream.flush();
        let dt = t0.elapsed();
        if dt.as_millis() > 10 {
            eprintln!(
                "[usb-bridge] SLOW flush: {:.1}ms",
                dt.as_secs_f64() * 1000.0,
            );
        }
    }

    fn reinit(&mut self) -> Result<(), String> {
        // Close the USB device on the daemon side, then reopen it.
        // This clears any stale firmware/driver state between tunes.
        self.send_cmd(&[CMD_CLOSE]);
        let _ = self.read_response();
        self.send_cmd(&[CMD_INIT]);
        self.read_response()
    }

    fn mute(&mut self) {
        self.send_cmd(&[CMD_MUTE]);
        let _ = self.read_response();
    }

    fn close(&mut self) {
        self.send_cmd(&[CMD_CLOSE]);
        let _ = self.read_response();
    }

    fn shutdown(&mut self) {
        let _ = self.stream.write_all(&[CMD_QUIT]);
        let _ = self.stream.flush();
    }

    fn run_device_config(
        &mut self,
        op: &crate::player::DeviceConfigCmd,
    ) -> Result<Option<crate::ui::DeviceConfigSnapshot>, String> {
        self.run_device_config_op(op)
    }
}

impl Drop for BridgeDevice {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl BridgeDevice {
    /// Forward to the shared config helper, borrowing `self` as Transport.
    pub fn run_device_config_op(
        &mut self,
        op: &crate::player::DeviceConfigCmd,
    ) -> Result<Option<crate::ui::DeviceConfigSnapshot>, String> {
        crate::device_config::run(&mut *self, op)
    }
}

/// Forwards `usbsid-pico-config` protocol bytes through the daemon's
/// `CMD_CFG_SEND` / `CMD_CFG_RECV` passthrough. The daemon performs the
/// actual bulk transfer on the device; we just pipe bytes over the Unix
/// socket. Requires daemon ≥ the version that exposes those commands.
impl Transport for BridgeDevice {
    fn send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if bytes.len() > 255 {
            return Err(TransportError::Io(format!(
                "bridge CFG_SEND payload too large: {} bytes (max 255)",
                bytes.len()
            )));
        }
        let mut frame = Vec::with_capacity(2 + bytes.len());
        frame.push(CMD_CFG_SEND);
        frame.push(bytes.len() as u8);
        frame.extend_from_slice(bytes);
        self.send_cmd(&frame);
        self.read_response()
            .map_err(|e| TransportError::Io(format!("CFG_SEND: {e}")))
    }

    fn recv(&mut self, len: usize) -> Result<Vec<u8>, TransportError> {
        let n = len.min(255) as u8;
        self.send_cmd(&[CMD_CFG_RECV, n]);
        // Response: [RESP_OK, n, ...bytes] or RESP_ERR + msg (read_response).
        let mut resp = [0u8; 1];
        if self.stream.read_exact(&mut resp).is_err() {
            return Err(TransportError::Io("bridge disconnected".into()));
        }
        if resp[0] == RESP_OK {
            let mut nbuf = [0u8; 1];
            if self.stream.read_exact(&mut nbuf).is_err() {
                return Err(TransportError::Io("CFG_RECV truncated length".into()));
            }
            let got = nbuf[0] as usize;
            let mut data = vec![0u8; got];
            if self.stream.read_exact(&mut data).is_err() {
                return Err(TransportError::Io("CFG_RECV truncated data".into()));
            }
            Ok(data)
        } else {
            // RESP_ERR + len + msg
            let mut lenbuf = [0u8; 1];
            let _ = self.stream.read_exact(&mut lenbuf);
            let mut msg = vec![0u8; lenbuf[0] as usize];
            let _ = self.stream.read_exact(&mut msg);
            Err(TransportError::Io(format!(
                "CFG_RECV: {}",
                String::from_utf8_lossy(&msg)
            )))
        }
    }

    fn drain(&mut self) -> Result<(), TransportError> {
        self.send_cmd(&[CMD_CFG_DRAIN]);
        self.read_response()
            .map_err(|e| TransportError::Io(format!("CFG_DRAIN: {e}")))
    }
}

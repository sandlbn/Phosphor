// macOS only: connects to the usbsid-bridge LaunchDaemon
// via a Unix domain socket. Fixed-size protocol.
//
// CMD_RING writes are buffered by the daemon and flushed as
// bulk USB packets on CMD_FLUSH — one transfer per 31 reg/val pairs.

use crate::sid_device::SidDevice;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

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
        let stream = UnixStream::connect(SOCKET_PATH).map_err(|e| {
            format!(
                "Cannot connect to usbsid-bridge daemon at {SOCKET_PATH}: {e}\n\
                 Install with: ./install.sh"
            )
        })?;
        eprintln!("[usb-bridge] connected");
        Ok(Self { stream })
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

        let mut buf = Vec::with_capacity(writes.len() * 5 + 1);
        for &(cycles, reg, val) in writes {
            buf.push(CMD_RING);
            buf.push(reg);
            buf.push(val);
            buf.push((cycles >> 8) as u8);
            buf.push((cycles & 0xFF) as u8);
        }
        buf.push(CMD_FLUSH);

        let _ = self.stream.write_all(&buf);
        let _ = self.stream.flush();
    }

    fn flush(&mut self) {
        let _ = self.stream.write_all(&[CMD_FLUSH]);
        let _ = self.stream.flush();
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
}

impl Drop for BridgeDevice {
    fn drop(&mut self) {
        self.shutdown();
    }
}

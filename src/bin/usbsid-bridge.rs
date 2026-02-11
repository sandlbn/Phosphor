// macOS/Linux only: privileged USB bridge daemon.
// Runs as root via launchd (LaunchDaemon) on macOS.
// Communicates with Phosphor over a Unix domain socket.
// Fixed-size protocol — every command has a known byte count.
//
// CMD_RING writes are buffered. CMD_FLUSH packs them into 64-byte
// USB bulk packets via single_write — one transfer per 31 reg/val pairs.
//
// Not needed on Windows — USB access works directly from userspace.

#[cfg(not(unix))]
fn main() {
    eprintln!("usbsid-bridge is only needed on macOS/Linux.");
    eprintln!("On Windows, Phosphor accesses USB directly.");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    unix_main::run();
}

#[cfg(unix)]
mod unix_main {

    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use usbsid_pico::{ClockSpeed, UsbSid};

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
    const RESP_ERR: u8 = 0x01;

    /// Max reg/val pairs per 64-byte USB packet: (64 - 1 header) / 2 = 31
    const MAX_PAIRS_PER_PACKET: usize = 31;

    fn send_ok(stream: &mut impl Write) {
        let _ = stream.write_all(&[RESP_OK]);
        let _ = stream.flush();
    }

    fn send_err(stream: &mut impl Write, msg: &str) {
        let bytes = msg.as_bytes();
        let len = bytes.len().min(255) as u8;
        let _ = stream.write_all(&[RESP_ERR, len]);
        let _ = stream.write_all(&bytes[..len as usize]);
        let _ = stream.flush();
    }

    /// Flush buffered writes as bulk USB packets.
    /// Each packet: [write_header, reg1, val1, reg2, val2, ...]
    /// write_header = data_len (since OP_WRITE = 0, top 2 bits are 0).
    fn flush_ring_buf(dev: &mut UsbSid, ring_buf: &[(u8, u8)]) {
        if ring_buf.is_empty() {
            return;
        }

        let mut pkt = [0u8; 64];

        for chunk in ring_buf.chunks(MAX_PAIRS_PER_PACKET) {
            let data_len = (chunk.len() * 2) as u8;
            pkt[0] = data_len; // USB protocol header (OP_WRITE=0 | byte_count)
            for (i, &(reg, val)) in chunk.iter().enumerate() {
                pkt[1 + i * 2] = reg;
                pkt[2 + i * 2] = val;
            }
            let total = 1 + chunk.len() * 2;
            let _ = dev.single_write(&pkt[..total]);
        }
    }

    fn handle_client(mut stream: std::os::unix::net::UnixStream) {
        let mut dev: Option<UsbSid> = None;
        let mut cmd = [0u8; 1];
        // Buffer for CMD_RING writes — flushed on CMD_FLUSH
        let mut ring_buf: Vec<(u8, u8)> = Vec::with_capacity(128);

        eprintln!("[usbsid-bridge] client connected");

        loop {
            if stream.read_exact(&mut cmd).is_err() {
                break;
            }

            match cmd[0] {
                CMD_INIT => {
                    if dev.is_some() {
                        send_ok(&mut stream);
                        continue;
                    }
                    let mut d = UsbSid::new();
                    match d.init(false, false) {
                        Ok(_) => {
                            eprintln!("[usbsid-bridge] USBSID-Pico opened");
                            dev = Some(d);
                            send_ok(&mut stream);
                        }
                        Err(e) => {
                            let msg = format!("USB init failed: {e}");
                            eprintln!("[usbsid-bridge] {msg}");
                            send_err(&mut stream, &msg);
                        }
                    }
                }

                CMD_CLOCK => {
                    let mut b = [0u8; 1];
                    if stream.read_exact(&mut b).is_err() {
                        break;
                    }
                    if let Some(ref mut d) = dev {
                        let speed = if b[0] != 0 {
                            ClockSpeed::Pal as i64
                        } else {
                            ClockSpeed::Ntsc as i64
                        };
                        d.set_clock_rate(speed, true);
                    }
                    send_ok(&mut stream);
                }

                CMD_RESET => {
                    if let Some(ref mut d) = dev {
                        d.reset();
                    }
                    send_ok(&mut stream);
                }

                CMD_STEREO => {
                    let mut b = [0u8; 1];
                    if stream.read_exact(&mut b).is_err() {
                        break;
                    }
                    if let Some(ref mut d) = dev {
                        d.set_stereo(b[0] as i32);
                    }
                    send_ok(&mut stream);
                }

                CMD_WRITE => {
                    // Immediate single register write (for init/setup)
                    let mut b = [0u8; 2];
                    if stream.read_exact(&mut b).is_err() {
                        break;
                    }
                    if let Some(ref mut d) = dev {
                        let _ = d.write(b[0], b[1]);
                    }
                }

                CMD_RING => {
                    // Fixed 4 bytes: reg, val, cycles_hi, cycles_lo
                    // Buffer the write — flushed as bulk USB packet on CMD_FLUSH
                    let mut b = [0u8; 4];
                    if stream.read_exact(&mut b).is_err() {
                        break;
                    }
                    ring_buf.push((b[0], b[1]));
                }

                CMD_FLUSH => {
                    // Pack buffered writes into bulk USB packets and send
                    if let Some(ref mut d) = dev {
                        flush_ring_buf(d, &ring_buf);
                    }
                    ring_buf.clear();
                }

                CMD_MUTE => {
                    if let Some(ref mut d) = dev {
                        d.mute();
                    }
                    send_ok(&mut stream);
                }

                CMD_CLOSE => {
                    if let Some(ref mut d) = dev {
                        if !ring_buf.is_empty() {
                            flush_ring_buf(d, &ring_buf);
                            ring_buf.clear();
                        }
                        d.mute();
                        d.reset();
                        d.close();
                    }
                    dev = None;
                    send_ok(&mut stream);
                }

                CMD_QUIT => {
                    if let Some(ref mut d) = dev {
                        if !ring_buf.is_empty() {
                            flush_ring_buf(d, &ring_buf);
                            ring_buf.clear();
                        }
                        d.mute();
                        d.reset();
                        d.close();
                    }
                    eprintln!("[usbsid-bridge] client quit");
                    break;
                }

                other => {
                    eprintln!("[usbsid-bridge] unknown command: 0x{other:02X}");
                }
            }
        }

        // Clean up if client disconnected without CMD_QUIT
        if let Some(ref mut d) = dev {
            d.mute();
            d.reset();
            d.close();
        }
        eprintln!("[usbsid-bridge] client disconnected");
    }

    pub fn run() {
        eprintln!(
            "[usbsid-bridge] daemon starting (pid={})",
            std::process::id()
        );

        let _ = std::fs::remove_file(SOCKET_PATH);

        let listener = match UnixListener::bind(SOCKET_PATH) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[usbsid-bridge] failed to bind {SOCKET_PATH}: {e}");
                std::process::exit(1);
            }
        };

        let _ = std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o777));

        eprintln!("[usbsid-bridge] listening on {SOCKET_PATH}");

        for stream in listener.incoming() {
            match stream {
                Ok(s) => handle_client(s),
                Err(e) => eprintln!("[usbsid-bridge] accept error: {e}"),
            }
        }
    }
} // mod unix_main

// macOS/Linux only: privileged USB bridge daemon.
// Runs as root via launchd (LaunchDaemon) on macOS.
// Communicates with Phosphor over a Unix domain socket.
// Fixed-size protocol — every command has a known byte count.
//
// Uses synchronous mode (init false, false) with manual
// OP_CYCLED_WRITE packet packing — identical firmware packets
// to C++ players but without the threading complexity.
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

    const OP_CYCLED_WRITE: u8 = 2;
    const MAX_CYCLED_PER_PACKET: usize = 15;

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

    fn handle_client(mut stream: std::os::unix::net::UnixStream) {
        let mut dev: Option<UsbSid> = None;
        let mut cmd = [0u8; 1];

        // Ring buffer for collecting cycled writes until flush
        let mut ring_buf: Vec<(u16, u8, u8)> = Vec::with_capacity(256);

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
                    match d.init(true, true) {
                        Ok(_) => {
                            eprintln!("[usbsid-bridge] USBSID-Pico opened (synchronous)");
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
                    ring_buf.clear();
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
                    let mut b = [0u8; 4];
                    if stream.read_exact(&mut b).is_err() {
                        break;
                    }
                    let cycles = ((b[2] as u16) << 8) | (b[3] as u16);
                    ring_buf.push((cycles, b[0], b[1]));
                }

                CMD_FLUSH => {
                    // Pack and send all buffered writes as OP_CYCLED_WRITE packets
                    if let Some(ref d) = dev {
                        let mut pkt = [0u8; 64];
                        for chunk in ring_buf.chunks(MAX_CYCLED_PER_PACKET) {
                            let data_len = (chunk.len() * 4) as u8;
                            pkt[0] = (OP_CYCLED_WRITE << 6) | data_len;
                            for (i, &(cycles, reg, val)) in chunk.iter().enumerate() {
                                pkt[1 + i * 4] = reg;
                                pkt[2 + i * 4] = val;
                                pkt[3 + i * 4] = (cycles >> 8) as u8;
                                pkt[4 + i * 4] = (cycles & 0xFF) as u8;
                            }
                            let total = 1 + chunk.len() * 4;
                            let _ = d.single_write(&pkt[..total]);
                        }
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
                        d.mute();
                        d.reset();
                        d.close();
                    }
                    dev = None;
                    ring_buf.clear();
                    send_ok(&mut stream);
                }

                CMD_QUIT => {
                    if let Some(ref mut d) = dev {
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

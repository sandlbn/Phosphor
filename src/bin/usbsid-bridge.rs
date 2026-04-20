// macOS/Linux only: privileged USB bridge daemon.
// Runs as root via launchd (LaunchDaemon) on macOS.
// Communicates with Phosphor over a Unix domain socket.
// Fixed-size protocol — every command has a known byte count.
//
// Uses the driver's threaded ring buffer with cycle-accurate writes:
// CMD_RING writes push to write_ring_cycled(), CMD_FLUSH signals
// end-of-frame via set_flush().
//
// Not needed on Windows — USB access works directly from userspace.
// Not needed when building with only the "emulated" feature.

#[cfg(not(feature = "usb"))]
fn main() {
    eprintln!("usbsid-bridge requires the 'usb' feature.");
    eprintln!("Rebuild with: cargo build --features usb --bin usbsid-bridge");
    std::process::exit(1);
}

#[cfg(all(feature = "usb", not(unix)))]
fn main() {
    eprintln!("usbsid-bridge is only needed on macOS/Linux.");
    eprintln!("On Windows, Phosphor accesses USB directly.");
    std::process::exit(1);
}

#[cfg(all(feature = "usb", unix))]
fn main() {
    unix_main::run();
}

#[cfg(all(feature = "usb", unix))]
mod unix_main {

    use std::io::{BufReader, Read, Write};
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

    fn handle_client(stream: std::os::unix::net::UnixStream) {
        // Split into buffered reader + raw writer.
        // BufReader reads large chunks from the kernel socket buffer in one
        // syscall (~8KB), then serves individual read_exact calls from memory.
        // This reduces 2500+ syscalls/frame to ~1 for heavy tunes.
        let mut writer = stream.try_clone().expect("stream clone failed");
        let mut reader = BufReader::with_capacity(8192, stream);
        let mut dev: Option<UsbSid> = None;
        let mut cmd = [0u8; 1];

        // Pending ring writes — collected locally and pushed to the driver
        // in one batch when CMD_FLUSH arrives.
        let mut pending_writes: Vec<(u8, u8, u16)> = Vec::with_capacity(512);

        eprintln!("[usbsid-bridge] client connected");

        loop {
            if reader.read_exact(&mut cmd).is_err() {
                break;
            }

            match cmd[0] {
                CMD_INIT => {
                    if dev.is_some() {
                        send_ok(&mut writer);
                        continue;
                    }
                    let mut d = UsbSid::new();
                    match d.init(true, true) {
                        Ok(_) => {
                            eprintln!("[usbsid-bridge] USBSID-Pico opened (threaded, cycled)");
                            dev = Some(d);
                            send_ok(&mut writer);
                        }
                        Err(e) => {
                            let msg = format!("USB init failed: {e}");
                            eprintln!("[usbsid-bridge] {msg}");
                            send_err(&mut writer, &msg);
                        }
                    }
                }

                CMD_CLOCK => {
                    let mut b = [0u8; 1];
                    if reader.read_exact(&mut b).is_err() {
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
                    send_ok(&mut writer);
                }

                CMD_RESET => {
                    pending_writes.clear();
                    if let Some(ref mut d) = dev {
                        d.reset();
                    }
                    send_ok(&mut writer);
                }

                CMD_STEREO => {
                    let mut b = [0u8; 1];
                    if reader.read_exact(&mut b).is_err() {
                        break;
                    }
                    if let Some(ref mut d) = dev {
                        d.set_stereo(b[0] as i32);
                    }
                    send_ok(&mut writer);
                }

                CMD_WRITE => {
                    // Immediate register write (init/setup).
                    // Uses single_write because write() is blocked in threaded mode.
                    let mut b = [0u8; 2];
                    if reader.read_exact(&mut b).is_err() {
                        break;
                    }
                    if let Some(ref d) = dev {
                        let buf = [0x00, b[0], b[1]];
                        let _ = d.single_write(&buf);
                    }
                }

                CMD_RING => {
                    // Collect writes locally — they will be pushed to the
                    // driver in a single batch when CMD_FLUSH arrives.
                    let mut b = [0u8; 4];
                    if reader.read_exact(&mut b).is_err() {
                        break;
                    }
                    let cycles = ((b[2] as u16) << 8) | (b[3] as u16);
                    pending_writes.push((b[0], b[1], cycles));
                }

                CMD_FLUSH => {
                    if let Some(ref mut d) = dev {
                        if !pending_writes.is_empty() {
                            let _ = d.write_ring_cycled_batch(&pending_writes);
                            pending_writes.clear();
                        }
                        d.set_flush();
                    }
                }

                CMD_MUTE => {
                    pending_writes.clear();
                    if let Some(ref mut d) = dev {
                        d.mute();
                    }
                    send_ok(&mut writer);
                }

                CMD_CLOSE => {
                    pending_writes.clear();
                    if let Some(ref mut d) = dev {
                        d.flush();
                        d.mute();
                        d.reset();
                        d.close();
                    }
                    dev = None;
                    send_ok(&mut writer);
                }

                CMD_QUIT => {
                    pending_writes.clear();
                    if let Some(ref mut d) = dev {
                        d.flush();
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

    const LOG_PATH: &str = "/tmp/usbsid-bridge.log";
    const LOG_MAX_BYTES: u64 = 512 * 1024; // 512 KB

    pub fn run() {
        // Rotate log if it has grown too large
        if let Ok(meta) = std::fs::metadata(LOG_PATH) {
            if meta.len() > LOG_MAX_BYTES {
                let _ = std::fs::copy(LOG_PATH, format!("{LOG_PATH}.old"));
                let _ = std::fs::File::create(LOG_PATH);
            }
        }

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

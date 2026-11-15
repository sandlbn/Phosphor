// Headless command-line interface.
//
//   phosphor cli play <file.sid> [--song N] [--secs S] [--engine E]
//                                [--usb-mode bridge|direct] [--stereo]
//   phosphor cli info [--engine E] [--usb-mode bridge|direct]
//
// Drives the exact same player thread the GUI uses (`spawn_player`), so a
// sound problem reproduced here is the same code path as in the app. Meant
// for hardware debugging (record the USBSID-Pico output while a tune plays)
// and scripting. Defaults come from the saved config; flags override them.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::player::{self, DeviceConfigCmd, PlayState, PlayerCmd};

const USAGE: &str = "\
Usage:
  phosphor cli play <file.sid> [options]   Play a tune headless
  phosphor cli info [options]              Print USBSID-Pico firmware/PCB/config
  phosphor cli raw <hex bytes...>          Send raw bytes to the USBSID-Pico (direct libusb),
                                           print any reply, e.g. `cli raw D2 83 00`
  phosphor cli jitter [--mode burst|carry] [--secs S] [--record F --input N]
                                           Timing test tone: onset every PAL frame
  phosphor cli config change-detect on|off  Edit via Phosphor's Device-panel path (RAM, no save)
  phosphor cli set rx-ring on|off          fw >= 0.9: USB RX ringbuffer (board saves + reboots)

Options:
  --song N              Subtune, 1-based (default: tune's start song)
  --secs S              Stop after S seconds (default: run until Ctrl-C)
  --engine E            usb | emulated | sidlite | u64 | auto (default: config)
  --usb-mode M          macOS only: bridge | direct (default: config)
  --stereo              Force 2SID stereo routing
  --record FILE.wav     Record an audio input while playing (16-bit WAV)
  --tail S              With --record: keep recording S seconds after stop
  --input NAME          Input device name substring for --record (default: system default)
  --list-inputs         List audio input devices and exit
  -q, --quiet           Don't print the once-per-second status line";

struct Opts {
    file: Option<PathBuf>,
    song: u16,
    secs: Option<f64>,
    engine: Option<String>,
    usb_mode: Option<String>,
    stereo: bool,
    quiet: bool,
    record: Option<PathBuf>,
    input: Option<String>,
    tail: f64,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        file: None,
        song: 0,
        secs: None,
        engine: None,
        usb_mode: None,
        stereo: false,
        quiet: false,
        record: None,
        input: None,
        tail: 0.0,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--song" => {
                o.song = val("--song")?
                    .parse()
                    .map_err(|_| "--song must be a number".to_string())?
            }
            "--secs" => {
                o.secs = Some(
                    val("--secs")?
                        .parse()
                        .map_err(|_| "--secs must be a number".to_string())?,
                )
            }
            "--engine" => o.engine = Some(val("--engine")?),
            "--usb-mode" => o.usb_mode = Some(val("--usb-mode")?),
            "--stereo" => o.stereo = true,
            "--record" => o.record = Some(PathBuf::from(val("--record")?)),
            "--input" => o.input = Some(val("--input")?),
            "--tail" => {
                o.tail = val("--tail")?
                    .parse()
                    .map_err(|_| "--tail must be a number".to_string())?
            }
            "--list-inputs" => {
                crate::cli::recorder::list_inputs();
                std::process::exit(0);
            }
            "-q" | "--quiet" => o.quiet = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => {
                if o.file.is_some() {
                    return Err(format!("unexpected argument {s}"));
                }
                o.file = Some(PathBuf::from(s));
            }
        }
    }
    Ok(o)
}

/// Entry point for `phosphor cli ...`. `args` excludes the program name and
/// the `cli` word. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    if sub == "-h" || sub == "--help" || sub == "help" {
        println!("{USAGE}");
        return 0;
    }
    if sub == "raw" {
        return raw(&args[1..]);
    }
    if sub == "set" {
        return set(&args[1..]);
    }
    if sub == "config" {
        return config_edit(&args[1..]);
    }
    if sub == "jitter" {
        return jitter(&args[1..]);
    }
    let opts = match parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let config = Config::load();
    let engine = opts
        .engine
        .clone()
        .unwrap_or_else(|| config.output_engine());
    let usb_mode = opts
        .usb_mode
        .clone()
        .unwrap_or_else(|| config.macos_usb_mode.clone());

    match sub.as_str() {
        "play" => play(&opts, &config, engine, usb_mode),
        "info" => info(engine, usb_mode),
        other => {
            eprintln!("error: unknown command '{other}'\n\n{USAGE}");
            2
        }
    }
}

fn install_ctrlc() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    static STOP: AtomicBool = AtomicBool::new(false);
    #[cfg(unix)]
    unsafe {
        extern "C" fn on_sig(_: libc::c_int) {
            STOP.store(true, Ordering::SeqCst);
        }
        let handler = on_sig as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    // Bridge the static into an Arc the caller can poll.
    let flag = Arc::new(AtomicBool::new(false));
    let f2 = flag.clone();
    std::thread::spawn(move || loop {
        if STOP.load(Ordering::SeqCst) {
            f2.store(true, Ordering::SeqCst);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    });
    flag
}

fn play(opts: &Opts, config: &Config, engine: String, usb_mode: String) -> i32 {
    let Some(file) = opts.file.clone() else {
        eprintln!("error: play needs a .sid file\n\n{USAGE}");
        return 2;
    };
    if !file.exists() {
        eprintln!("error: {} not found", file.display());
        return 1;
    }
    let stop = install_ctrlc();
    let recorder = match &opts.record {
        Some(p) => match recorder::Recorder::start(opts.input.as_deref(), p.clone()) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        },
        None => None,
    };
    eprintln!("[cli] engine={engine} usb_mode={usb_mode}");
    let (cmd_tx, status_rx, _cfg_rx) = player::spawn_player(
        engine,
        config.u64_address.clone(),
        config.u64_password.clone(),
        usb_mode,
    );
    let _ = cmd_tx.send(PlayerCmd::Play {
        path: file,
        song: opts.song,
        force_stereo: opts.stereo || config.force_stereo_2sid,
        pseudo_stereo: None,
        sid4_addr: 0,
        audio_port: None,
        restart_usb_on_load: config.restart_usb_on_load,
    });

    let started = Instant::now();
    let mut announced = false;
    let mut was_playing = false;
    let mut last_print = Instant::now() - Duration::from_secs(2);
    let mut code = 0;
    loop {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            eprintln!("\n[cli] interrupted");
            break;
        }
        if let Some(s) = opts.secs {
            if started.elapsed().as_secs_f64() >= s {
                break;
            }
        }
        match status_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(st) => {
                if let Some(e) = &st.error {
                    eprintln!("[cli] error: {e}");
                    code = 1;
                    break;
                }
                if !announced {
                    if let Some(t) = &st.track_info {
                        eprintln!(
                            "[cli] {} — {} ({}) song {}/{} {} {}SID {}",
                            t.name,
                            t.author,
                            t.released,
                            t.current_song,
                            t.songs,
                            if t.is_pal { "PAL" } else { "NTSC" },
                            t.num_sids,
                            if t.is_rsid { "RSID" } else { "PSID" },
                        );
                        announced = true;
                    }
                }
                if st.state == PlayState::Playing {
                    was_playing = true;
                } else if was_playing && st.state == PlayState::Stopped {
                    eprintln!("[cli] playback stopped");
                    break;
                }
                if !opts.quiet && last_print.elapsed() >= Duration::from_secs(1) {
                    last_print = Instant::now();
                    eprint!(
                        "\r[cli] {:?} {:>6.1}s  writes/frame {:>3}  {}   ",
                        st.state,
                        st.elapsed.as_secs_f64(),
                        st.writes_per_frame,
                        if st.device_connected {
                            ""
                        } else {
                            "DISCONNECTED"
                        }
                    );
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => {
                eprintln!("[cli] player thread exited");
                code = 1;
                break;
            }
        }
    }
    eprintln!();
    if let Some(r) = &recorder {
        eprintln!("[cli] stop at {:.3} s into the recording", r.elapsed());
    }
    let _ = cmd_tx.send(PlayerCmd::Stop);
    let _ = cmd_tx.send(PlayerCmd::Quit);
    // Give the player thread a moment to mute/reset/close the device.
    std::thread::sleep(Duration::from_millis(400));
    if let Some(r) = recorder {
        std::thread::sleep(Duration::from_secs_f64(opts.tail));
        match r.finish() {
            Ok(msg) => eprintln!("[cli] {msg}"),
            Err(e) => {
                eprintln!("[cli] recording failed: {e}");
                code = 1;
            }
        }
    }
    code
}

fn info(engine: String, usb_mode: String) -> i32 {
    let config = Config::load();
    let (cmd_tx, _status_rx, cfg_rx) = player::spawn_player(
        engine,
        config.u64_address.clone(),
        config.u64_password.clone(),
        usb_mode,
    );
    let _ = cmd_tx.send(PlayerCmd::DeviceConfig(DeviceConfigCmd::Refresh));
    let code = match cfg_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(snap)) => {
            println!("firmware: {}", snap.firmware_version);
            println!(
                "pcb:      {} (raw {})",
                snap.pcb_version, snap.pcb_version_raw
            );
            println!("capabilities: {:#?}", snap.capabilities);
            println!("config: {:#?}", snap.config);
            if snap.config.need_confirmation {
                println!(
                    "WARNING: board reports an unconfirmed config (socket/chip change detected).\n\
                     SID writes are dropped until it is confirmed. Check the chip types/voltages,\n\
                     then confirm in Phosphor's Device panel."
                );
            }
            0
        }
        Ok(Err(e)) => {
            eprintln!("error: {e}");
            1
        }
        Err(_) => {
            eprintln!("error: no reply from device within 10 s");
            1
        }
    };
    let _ = cmd_tx.send(PlayerCmd::Quit);
    std::thread::sleep(Duration::from_millis(300));
    code
}

/// Minimal cpal input capture to a 16-bit PCM WAV, for recording the
/// USBSID-Pico's analog output through an audio interface.
pub mod recorder {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    pub fn list_inputs() {
        let host = cpal::default_host();
        let default = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        match host.input_devices() {
            Ok(devs) => {
                for d in devs {
                    let name = d.name().unwrap_or_else(|_| "?".into());
                    let cfg = d
                        .default_input_config()
                        .map(|c| {
                            format!(
                                "{} Hz, {} ch, {:?}",
                                c.sample_rate().0,
                                c.channels(),
                                c.sample_format()
                            )
                        })
                        .unwrap_or_else(|e| e.to_string());
                    let mark = if name == default { "*" } else { " " };
                    println!("{mark} {name}  [{cfg}]");
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }

    pub struct Recorder {
        stream: cpal::Stream,
        samples: Arc<Mutex<Vec<i16>>>,
        rate: u32,
        channels: u16,
        path: PathBuf,
        started: std::time::Instant,
    }

    impl Recorder {
        /// Seconds of audio captured so far (by sample count, not wall clock).
        pub fn elapsed(&self) -> f64 {
            let n = self.samples.lock().map(|v| v.len()).unwrap_or(0);
            let _ = self.started;
            n as f64 / (self.rate as f64 * self.channels as f64)
        }

        pub fn start(input: Option<&str>, path: PathBuf) -> Result<Self, String> {
            let host = cpal::default_host();
            let dev = match input {
                Some(want) => {
                    let want = want.to_lowercase();
                    host.input_devices()
                        .map_err(|e| e.to_string())?
                        .find(|d| {
                            d.name()
                                .map(|n| n.to_lowercase().contains(&want))
                                .unwrap_or(false)
                        })
                        .ok_or_else(|| {
                            format!("no input device matching '{want}' (try --list-inputs)")
                        })?
                }
                None => host
                    .default_input_device()
                    .ok_or("no default input device")?,
            };
            let name = dev.name().unwrap_or_default();
            let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
            let rate = cfg.sample_rate().0;
            let channels = cfg.channels();
            let samples = Arc::new(Mutex::new(Vec::<i16>::with_capacity(
                rate as usize * channels as usize * 60,
            )));
            let err_fn = |e| eprintln!("[rec] stream error: {e}");
            let sc: cpal::StreamConfig = cfg.clone().into();
            let s2 = samples.clone();
            let stream = match cfg.sample_format() {
                cpal::SampleFormat::F32 => dev.build_input_stream(
                    &sc,
                    move |d: &[f32], _| {
                        let mut v = s2.lock().unwrap();
                        v.extend(d.iter().map(|x| (x.clamp(-1.0, 1.0) * 32767.0) as i16));
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => dev.build_input_stream(
                    &sc,
                    move |d: &[i16], _| s2.lock().unwrap().extend_from_slice(d),
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I32 => dev.build_input_stream(
                    &sc,
                    move |d: &[i32], _| {
                        let mut v = s2.lock().unwrap();
                        v.extend(d.iter().map(|x| (x >> 16) as i16));
                    },
                    err_fn,
                    None,
                ),
                f => return Err(format!("unsupported input sample format {f:?}")),
            }
            .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            eprintln!(
                "[rec] recording '{name}' {rate} Hz {channels} ch -> {}",
                path.display()
            );
            Ok(Self {
                stream,
                samples,
                rate,
                channels,
                path,
                started: std::time::Instant::now(),
            })
        }

        pub fn finish(self) -> Result<String, String> {
            drop(self.stream);
            let data = std::mem::take(&mut *self.samples.lock().unwrap());
            let peak = data.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            let bytes = (data.len() * 2) as u32;
            let mut f = std::fs::File::create(&self.path).map_err(|e| e.to_string())?;
            let ba = self.channels as u32 * 2;
            let mut h = Vec::with_capacity(44);
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&(36 + bytes).to_le_bytes());
            h.extend_from_slice(b"WAVEfmt ");
            h.extend_from_slice(&16u32.to_le_bytes());
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&self.channels.to_le_bytes());
            h.extend_from_slice(&self.rate.to_le_bytes());
            h.extend_from_slice(&(self.rate * ba).to_le_bytes());
            h.extend_from_slice(&(ba as u16).to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(b"data");
            h.extend_from_slice(&bytes.to_le_bytes());
            f.write_all(&h).map_err(|e| e.to_string())?;
            let mut body = Vec::with_capacity(data.len() * 2);
            for s in &data {
                body.extend_from_slice(&s.to_le_bytes());
            }
            f.write_all(&body).map_err(|e| e.to_string())?;
            let secs = data.len() as f64 / (self.rate as f64 * self.channels as f64);
            Ok(format!(
                "wrote {} ({secs:.1} s, peak {:.1} dBFS)",
                self.path.display(),
                20.0 * ((peak.max(1) as f64) / 32768.0).log10()
            ))
        }
    }
}

/// Open the board straight through libusb, unthreaded, for one-off commands.
fn open_raw() -> Result<usbsid_pico::UsbSid, String> {
    let mut dev = usbsid_pico::UsbSid::new();
    dev.init(false, false)
        .map_err(|e| format!("USB open failed: {e}"))?;
    Ok(dev)
}

fn raw(args: &[String]) -> i32 {
    let bytes: Result<Vec<u8>, _> = args
        .iter()
        .flat_map(|a| a.split(|c: char| c == ',' || c.is_whitespace()))
        .filter(|t| !t.is_empty())
        .map(|t| u8::from_str_radix(t.trim_start_matches("0x").trim_start_matches("0X"), 16))
        .collect();
    let bytes = match bytes {
        Ok(b) if !b.is_empty() && b.len() <= 64 => b,
        _ => {
            eprintln!("error: give 1..64 hex bytes, e.g. `phosphor cli raw D2 83 00`");
            return 2;
        }
    };
    let dev = match open_raw() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if let Err(e) = dev.send_raw(&bytes) {
        eprintln!("error: send: {e}");
        return 1;
    }
    eprintln!("[cli] sent {}", hex(&bytes));
    match dev.recv_raw_timeout(64, 300) {
        Ok(r) if !r.is_empty() => println!("{}", hex(&r)),
        _ => eprintln!("[cli] (no reply)"),
    }
    0
}

fn set(args: &[String]) -> i32 {
    let (Some(key), Some(val)) = (args.first(), args.get(1)) else {
        eprintln!("usage: phosphor cli set rx-ring on|off");
        return 2;
    };
    let on = match val.as_str() {
        "on" | "1" | "true" => 1u8,
        "off" | "0" | "false" => 0u8,
        _ => {
            eprintln!("error: value must be on or off");
            return 2;
        }
    };
    let cmd: Vec<u8> = match key.as_str() {
        // CONFIG command (0xD2) + SET_RX_RING (0x83) + value. Firmware
        // ignores an unchanged value; a change saves to flash and reboots.
        "rx-ring" => vec![0xD2, 0x83, on],
        other => {
            eprintln!("error: unknown setting '{other}'");
            return 2;
        }
    };
    let dev = match open_raw() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    match dev.send_raw(&cmd) {
        Ok(()) => {
            eprintln!(
                "[cli] sent {} — board reboots if the value changed",
                hex(&cmd)
            );
            0
        }
        Err(e) => {
            eprintln!("error: send: {e}");
            1
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Frame-timing test. Every PAL frame (19656 cycles) voice 1 switches a
/// triangle on at cycle ON_AT and off ON_LEN cycles later, so the recording
/// shows one burst per frame. Onset spacing must be exactly 19.950 ms;
/// its spread is the frame-to-frame timing error of the whole chain.
///
/// --mode burst: what Phosphor does today — deltas restart at 0 each frame,
///               frame start is set by when the USB data arrives.
/// --mode carry: self-clocked — the first delta of a frame carries the rest
///               of the previous frame, and PREROLL frames are sent ahead so
///               the board never waits for the host.
fn jitter(args: &[String]) -> i32 {
    const CPF: u32 = 19_656; // PAL cycles per frame
    const ON_AT: u32 = 200;
    const ON_LEN: u32 = 8_000;
    const PREROLL: u32 = 3;
    let mut mode = "burst".to_string();
    let mut secs = 10.0f64;
    let mut record: Option<PathBuf> = None;
    let mut input: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => mode = it.next().cloned().unwrap_or_default(),
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(10.0),
            "--record" => record = it.next().map(PathBuf::from),
            "--input" => input = it.next().cloned(),
            o => {
                eprintln!("error: unknown option {o}");
                return 2;
            }
        }
    }
    let carry = match mode.as_str() {
        "burst" => false,
        "carry" => true,
        m => {
            eprintln!("error: --mode must be burst or carry, got {m}");
            return 2;
        }
    };
    let rec = match &record {
        Some(p) => match recorder::Recorder::start(input.as_deref(), p.clone()) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        },
        None => None,
    };
    let mut dev = usbsid_pico::UsbSid::new();
    if let Err(e) = dev.init(true, true) {
        eprintln!("error: USB open failed: {e}");
        return 1;
    }
    dev.set_clock_rate(usbsid_pico::ClockSpeed::Pal as i64, true);
    dev.reset_all_registers();
    std::thread::sleep(Duration::from_millis(100));
    // Voice 1 setup: ~2 kHz triangle, instant attack, full sustain; parked
    // with the TEST bit so every burst restarts the oscillator at phase 0.
    for (r, v) in [
        (0x00u8, 0x00u8),
        (0x01, 0x80),
        (0x05, 0x00),
        (0x06, 0xF0),
        (0x18, 0x0F),
        (0x04, 0x09),
    ] {
        let _ = dev.write_ring_cycled(r, v, 8);
    }
    dev.set_flush();
    let frame = Duration::from_nanos(19_950_124); // 19656 / 985248 Hz
    let frames = (secs / 0.019_950_124) as u64;
    let start = Instant::now() + Duration::from_millis(50);
    let mut prev_end: u32 = 0; // cycle of the last write in the previous frame
    eprintln!("[jitter] mode={mode} frames={frames}");
    for n in 0..frames {
        // carry: send PREROLL frames early so the board is always ahead.
        let send_at = if carry && n >= PREROLL as u64 {
            start + frame * (n - PREROLL as u64) as u32
        } else if carry {
            start
        } else {
            start + frame * n as u32
        };
        wait_until(send_at);
        let first = if carry && n > 0 {
            CPF - prev_end + ON_AT
        } else {
            ON_AT
        };
        let _ = dev.write_ring_cycled(0x04, 0x11, first.min(0xFFFF) as u16); // tone on
        let _ = dev.write_ring_cycled(0x04, 0x09, ON_LEN as u16); // tone off (TEST)
        prev_end = ON_AT + ON_LEN;
        dev.set_flush();
    }
    std::thread::sleep(Duration::from_millis(200 + PREROLL as u64 * 20));
    dev.discard_pending();
    dev.mute();
    dev.reset_all_registers();
    dev.close();
    if let Some(r) = rec {
        std::thread::sleep(Duration::from_millis(300));
        match r.finish() {
            Ok(m) => eprintln!("[jitter] {m}"),
            Err(e) => {
                eprintln!("[jitter] recording failed: {e}");
                return 1;
            }
        }
    }
    0
}

fn wait_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if left > Duration::from_micros(1500) {
            std::thread::sleep(left - Duration::from_micros(1000));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Run one Device-panel edit through the player thread, exactly like the
/// GUI does (read config, apply edit, write config, APPLY — RAM only).
fn config_edit(args: &[String]) -> i32 {
    use crate::player::DeviceConfigEdit;
    let (Some(key), Some(val)) = (args.first(), args.get(1)) else {
        eprintln!("usage: phosphor cli config change-detect on|off");
        return 2;
    };
    let on = matches!(val.as_str(), "on" | "1" | "true");
    let edit = match key.as_str() {
        "change-detect" => DeviceConfigEdit::DisableChangeDetect(!on),
        other => {
            eprintln!("error: unknown config key '{other}'");
            return 2;
        }
    };
    let config = Config::load();
    let (cmd_tx, _st, cfg_rx) = player::spawn_player(
        "usb".into(),
        config.u64_address.clone(),
        config.u64_password.clone(),
        config.macos_usb_mode.clone(),
    );
    let _ = cmd_tx.send(PlayerCmd::DeviceConfig(DeviceConfigCmd::Edit(edit)));
    let code = match cfg_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(snap)) => {
            println!(
                "disable_changedetect now {} (need_confirmation {})",
                snap.config.disable_changedetect, snap.config.need_confirmation
            );
            0
        }
        Ok(Err(e)) => {
            eprintln!("error: {e}");
            1
        }
        Err(_) => {
            eprintln!("error: no reply within 10 s");
            1
        }
    };
    let _ = cmd_tx.send(PlayerCmd::Quit);
    std::thread::sleep(Duration::from_millis(300));
    code
}

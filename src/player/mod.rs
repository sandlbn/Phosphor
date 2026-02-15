// Background player engine. Runs in its own thread, communicates
// with the GUI via crossbeam channels. USB I/O goes through the
// setuid usbsid-bridge helper (fixed-size protocol, async ring buffer).
pub mod memory;
pub mod rsid_bus;
pub mod sid_file;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, select, tick, Receiver, Sender};
use mos6502::cpu::CPU;
use mos6502::instruction::Nmos6502;
use mos6502::memory::Bus;
use mos6502::registers::{StackPointer, Status};

use crate::sid_device::{create_engine, SidDevice};
use memory::*;
use rsid_bus::RsidBus;
use sid_file::*;

// ─────────────────────────────────────────────────────────────────────────────
//  Public message types
// ─────────────────────────────────────────────────────────────────────────────

/// Commands sent from GUI → player thread.
#[derive(Debug, Clone)]
pub enum PlayerCmd {
    Play {
        path: PathBuf,
        song: u16,
        force_stereo: bool,
        sid4_addr: u16,
    },
    Stop,
    TogglePause,
    SetSubtune(u16),
    SetEngine(String, String, String), // (engine_name, u64_address, u64_password)
    Quit,
}

/// Status updates sent from player thread → GUI.
#[derive(Debug, Clone)]
pub struct PlayerStatus {
    pub state: PlayState,
    pub track_info: Option<TrackInfo>,
    pub elapsed: Duration,
    pub voice_levels: Vec<f32>,
    pub writes_per_frame: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TrackInfo {
    pub path: PathBuf,
    pub name: String,
    pub author: String,
    pub released: String,
    pub songs: u16,
    pub current_song: u16,
    pub is_pal: bool,
    pub is_rsid: bool,
    pub num_sids: usize,
    pub sid_type: String,
    pub md5: String,
}

// ─────────────────────────────────────────────────────────────────────────────
//  USB write helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Send cycle-stamped SID writes to hardware via async ring buffer.
///
/// Converts absolute frame cycles to delta format (cycles since previous
/// write) and pushes them through `ring_cycled`. The device's background
/// thread drains the ring buffer to USB asynchronously with cycle-accurate
/// timing on the firmware side.
fn send_sid_writes(bridge: &mut dyn SidDevice, writes: &[(u32, u8, u8)], mirror_mono: bool) {
    if writes.is_empty() {
        return;
    }

    if mirror_mono {
        // Mono: duplicate each write for SID2 at delta=0 (same cycle position).
        let mut cycled: Vec<(u16, u8, u8)> = Vec::with_capacity(writes.len() * 2);
        let mut prev_cycle: u32 = 0;

        for &(cycle, reg, val) in writes {
            let delta = cycle.saturating_sub(prev_cycle).min(0xFFFF) as u16;
            cycled.push((delta, reg, val));
            if reg <= SID_VOL_REG {
                cycled.push((0, reg + SID_REG_SIZE, val));
            }
            prev_cycle = cycle;
        }

        bridge.ring_cycled(&cycled);
    } else {
        // Multi-SID: mapper already assigned reg offsets, send as-is.
        let mut cycled: Vec<(u16, u8, u8)> = Vec::with_capacity(writes.len());
        let mut prev_cycle: u32 = 0;

        for &(cycle, reg, val) in writes {
            let delta = cycle.saturating_sub(prev_cycle).min(0xFFFF) as u16;
            cycled.push((delta, reg, val));
            prev_cycle = cycle;
        }

        bridge.ring_cycled(&cycled);
    }
}

/// Wait until `deadline` using sleep for bulk + spin for precision.
/// Used for frame pacing — sleeps most of the duration, then spin-waits
/// the last ~1ms for sub-millisecond accuracy without burning 100% CPU.
fn wait_until(deadline: Instant) {
    let now = Instant::now();
    if now >= deadline {
        return;
    }
    let remaining = deadline - now;
    // Sleep if > 1.5ms remaining (sleep granularity is ~1ms on most OSes)
    if remaining > Duration::from_micros(1500) {
        thread::sleep(remaining - Duration::from_micros(1000));
    }
    // Spin-wait the final stretch for precision
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  6502 runners
// ─────────────────────────────────────────────────────────────────────────────

/// Run CPU until it hits `halt` address or exceeds `max_steps`.
/// Used for PSID play calls. Tracks frame_cycle so SID writes
/// get proper cycle timestamps for the firmware's intra-frame timing.
fn run_until(cpu: &mut CPU<C64Memory, Nmos6502>, halt: u16, max_steps: u32) {
    for _ in 0..max_steps {
        if cpu.registers.program_counter == halt {
            return;
        }
        let cycles = opcode_cycles_banked(&cpu.memory, cpu.registers.program_counter);
        cpu.single_step();
        cpu.memory.frame_cycle += cycles;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Player thread
// ─────────────────────────────────────────────────────────────────────────────

pub fn spawn_player(
    engine_name: String,
    u64_address: String,
    u64_password: String,
) -> (Sender<PlayerCmd>, Receiver<PlayerStatus>) {
    let (cmd_tx, cmd_rx) = bounded::<PlayerCmd>(64);
    let (status_tx, status_rx) = bounded::<PlayerStatus>(16);

    thread::Builder::new()
        .name("sid-player".into())
        .spawn(move || {
            player_loop(cmd_rx, status_tx, engine_name, u64_address, u64_password);
        })
        .expect("Failed to spawn player thread");

    (cmd_tx, status_rx)
}

fn player_loop(
    cmd_rx: Receiver<PlayerCmd>,
    status_tx: Sender<PlayerStatus>,
    mut engine_name: String,
    mut u64_address: String,
    mut u64_password: String,
) {
    let mut bridge: Option<Box<dyn SidDevice>> = None;
    let mut state = PlayState::Stopped;
    let mut play_ctx: Option<PlayContext> = None;
    let mut last_error: Option<String> = None;

    let idle_tick = tick(Duration::from_millis(100));

    loop {
        match state {
            PlayState::Stopped | PlayState::Paused => {
                select! {
                    recv(cmd_rx) -> msg => {
                        match msg {
                            Ok(PlayerCmd::Quit) => break,
                            Ok(cmd) => handle_cmd(
                                cmd, &mut state, &mut play_ctx,
                                &mut bridge, &mut last_error, &status_tx,
                                &mut engine_name, &mut u64_address, &mut u64_password,
                            ),
                            Err(_) => break,
                        }
                    }
                    recv(idle_tick) -> _ => {
                        send_status(&state, &play_ctx, &last_error, &status_tx);
                    }
                }
            }
            PlayState::Playing => {
                if let Some(ref mut ctx) = play_ctx {
                    let frame_dur = Duration::from_micros(ctx.frame_us);

                    // Drain commands (also detect GUI shutdown)
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(PlayerCmd::Quit) => {
                                cleanup(&mut bridge);
                                return;
                            }
                            Ok(other) => handle_cmd(
                                other,
                                &mut state,
                                &mut play_ctx,
                                &mut bridge,
                                &mut last_error,
                                &status_tx,
                                &mut engine_name,
                                &mut u64_address,
                                &mut u64_password,
                            ),
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                // GUI dropped the sender — shut down
                                cleanup(&mut bridge);
                                return;
                            }
                        }
                    }

                    if state != PlayState::Playing {
                        continue;
                    }

                    if let Some(ref mut ctx) = play_ctx {
                        match &mut ctx.engine {
                            PlayEngine::Rsid { cpu, prev_nmi } => {
                                // ── RSID (c64_emu) ───────────────────────────
                                cpu.memory.clear_writes();
                                run_rsid_sub_emu(cpu, ctx.cycles_per_frame, prev_nmi);

                                if let Some(ref mut br) = bridge {
                                    send_sid_writes(
                                        br.as_mut(),
                                        &cpu.memory.sid_writes,
                                        ctx.mirror_mono,
                                    );
                                }
                            }
                            PlayEngine::Psid(cpu) => {
                                // ── PSID ─────────────────────────────────────
                                cpu.memory.clear_writes();
                                cpu.registers.program_counter = ctx.trampoline;
                                cpu.registers.stack_pointer = StackPointer(0xFD);
                                run_until(cpu, ctx.halt_pc, 200_000);

                                if let Some(ref mut br) = bridge {
                                    send_sid_writes(
                                        br.as_mut(),
                                        &cpu.memory.sid_writes,
                                        ctx.mirror_mono,
                                    );
                                }
                            }
                            PlayEngine::Native => {
                                // ── U64 native — real hardware plays the SID ─
                                // Nothing to do here; the U64 handles playback.
                                // We just pace frames for elapsed time tracking.
                            }
                        }

                        // Signal the device to flush any remaining buffered
                        // writes for this frame (no-op for Native engine).
                        if !ctx.is_native() {
                            if let Some(ref mut br) = bridge {
                                br.flush();
                            }
                        }

                        // ── Absolute-timeline frame pacing ───────────────────
                        // Advance deadline by exactly one frame period.
                        // This prevents inter-frame overhead from accumulating
                        // as drift (was ~8% slow with per-frame Instant::now).
                        ctx.next_frame += frame_dur;

                        // If we've fallen behind (e.g. after a pause or load),
                        // snap the deadline to now rather than fast-forwarding.
                        let now = Instant::now();
                        if ctx.next_frame < now {
                            ctx.next_frame = now;
                        }

                        wait_until(ctx.next_frame);

                        ctx.frame_count += 1;
                        ctx.elapsed += frame_dur;
                    }

                    send_status(&state, &play_ctx, &last_error, &status_tx);
                } else {
                    state = PlayState::Stopped;
                }
            }
        }
    }

    cleanup(&mut bridge);
}

fn cleanup(bridge: &mut Option<Box<dyn SidDevice>>) {
    if let Some(ref mut br) = bridge {
        br.flush();
        br.mute();
        br.reset();
        br.close();
        br.shutdown();
    }
    *bridge = None;
    eprintln!("[phosphor] Player thread exiting");
}

fn ensure_hardware(
    bridge: &mut Option<Box<dyn SidDevice>>,
    engine_name: &str,
    u64_address: &str,
    u64_password: &str,
) -> Result<(), String> {
    if bridge.is_some() {
        return Ok(());
    }
    let mut br = create_engine(engine_name, u64_address, u64_password)?;
    br.init()?;
    *bridge = Some(br);
    Ok(())
}

fn send_status(
    state: &PlayState,
    ctx: &Option<PlayContext>,
    error: &Option<String>,
    tx: &Sender<PlayerStatus>,
) {
    let (info, elapsed, levels, writes) = match ctx {
        Some(c) => (
            Some(c.track_info.clone()),
            c.elapsed,
            c.voice_levels(),
            c.sid_writes().len(),
        ),
        None => (None, Duration::ZERO, vec![], 0),
    };

    let _ = tx.try_send(PlayerStatus {
        state: state.clone(),
        track_info: info,
        elapsed,
        voice_levels: levels,
        writes_per_frame: writes,
        error: error.clone(),
    });
}

fn handle_cmd(
    cmd: PlayerCmd,
    state: &mut PlayState,
    play_ctx: &mut Option<PlayContext>,
    bridge: &mut Option<Box<dyn SidDevice>>,
    last_error: &mut Option<String>,
    status_tx: &Sender<PlayerStatus>,
    engine_name: &mut String,
    u64_address: &mut String,
    u64_password: &mut String,
) {
    match cmd {
        PlayerCmd::Play {
            path,
            song,
            force_stereo,
            sid4_addr,
        } => {
            *last_error = None;
            stop_playback(play_ctx, bridge);

            if let Err(e) = ensure_hardware(bridge, engine_name, u64_address, u64_password) {
                *last_error = Some(e);
                *state = PlayState::Stopped;
                send_status(state, play_ctx, last_error, status_tx);
                return;
            }

            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Cannot read {}: {e}", path.display());
                    eprintln!("[phosphor] {msg}");
                    *last_error = Some(msg);
                    send_status(state, play_ctx, last_error, status_tx);
                    return;
                }
            };

            let sid_file = match load_sid(&data) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[phosphor] SID parse error: {e}");
                    *last_error = Some(e);
                    send_status(state, play_ctx, last_error, status_tx);
                    return;
                }
            };

            let is_rsid = sid_file.header.is_rsid
                || (sid_file.header.play_address == 0 && sid_file.header.magic == "PSID");

            eprintln!(
                "[phosphor] Loading: \"{}\" by {} — song {}/{} [{}]",
                sid_file.header.name,
                sid_file.header.author,
                song,
                sid_file.header.songs,
                if is_rsid { "RSID" } else { "PSID" },
            );

            // ── Try native playback (U64) ────────────────────────────────
            // If the engine supports play_sid_native, skip CPU emulation
            // entirely and let the real hardware do everything.
            let native = if let Some(ref mut br) = bridge {
                match br.play_sid_native(&data, song) {
                    Ok(true) => {
                        eprintln!("[phosphor] Native playback active — skipping CPU emulation");
                        true
                    }
                    Ok(false) => false,
                    Err(e) => {
                        eprintln!("[phosphor] Native playback failed: {e}");
                        false
                    }
                }
            } else {
                false
            };

            if native {
                // Build a lightweight context — only for time tracking.
                let header = &sid_file.header;
                let num_sids = 1
                    + (header.extra_sid_addrs[0] != 0) as usize
                    + (header.extra_sid_addrs[1] != 0) as usize;
                let sid_type = match num_sids {
                    1 => "Mono".to_string(),
                    2 => "2SID Stereo".to_string(),
                    3 => "3SID".to_string(),
                    n => format!("{}SID", n),
                };
                let md5 = compute_hvsc_md5(&sid_file);
                let frame_us = header.frame_us();
                let cycles_per_frame = if header.is_pal {
                    PAL_CYCLES_PER_FRAME
                } else {
                    NTSC_CYCLES_PER_FRAME
                };
                let track_info = TrackInfo {
                    path,
                    name: header.name.clone(),
                    author: header.author.clone(),
                    released: header.released.clone(),
                    songs: header.songs,
                    current_song: song,
                    is_pal: header.is_pal,
                    is_rsid,
                    num_sids,
                    sid_type,
                    md5,
                };
                *play_ctx = Some(PlayContext {
                    engine: PlayEngine::Native,
                    trampoline: 0,
                    halt_pc: 0,
                    frame_us,
                    cycles_per_frame,
                    elapsed: Duration::ZERO,
                    mirror_mono: false,
                    track_info,
                    frame_count: 0,
                    next_frame: Instant::now(),
                });
            } else {
                let ctx = setup_playback(
                    sid_file,
                    path,
                    song,
                    force_stereo,
                    sid4_addr,
                    is_rsid,
                    bridge,
                );
                *play_ctx = Some(ctx);
            }

            *state = PlayState::Playing;
            send_status(state, play_ctx, last_error, status_tx);
        }

        PlayerCmd::Stop => {
            stop_playback(play_ctx, bridge);
            *state = PlayState::Stopped;
            send_status(state, play_ctx, last_error, status_tx);
        }

        PlayerCmd::TogglePause => {
            match state {
                PlayState::Playing => *state = PlayState::Paused,
                PlayState::Paused => *state = PlayState::Playing,
                _ => {}
            }
            send_status(state, play_ctx, last_error, status_tx);
        }

        PlayerCmd::SetSubtune(song) => {
            *last_error = None;
            if let Some(ref ctx) = play_ctx {
                let path = ctx.track_info.path.clone();
                let stereo = ctx.mirror_mono;
                let is_rsid = ctx.is_rsid();
                let was_native = ctx.is_native();
                let sid4 = 0;
                stop_playback(play_ctx, bridge);

                if was_native {
                    // For native playback, re-send the SID file with new song number.
                    if let Ok(data) = std::fs::read(&path) {
                        if let Some(ref mut br) = bridge {
                            match br.play_sid_native(&data, song) {
                                Ok(true) => {
                                    if let Ok(sid_file) = load_sid(&data) {
                                        let header = &sid_file.header;
                                        let num_sids = 1
                                            + (header.extra_sid_addrs[0] != 0) as usize
                                            + (header.extra_sid_addrs[1] != 0) as usize;
                                        let sid_type = match num_sids {
                                            1 => "Mono".to_string(),
                                            2 => "2SID Stereo".to_string(),
                                            3 => "3SID".to_string(),
                                            n => format!("{}SID", n),
                                        };
                                        let md5 = compute_hvsc_md5(&sid_file);
                                        let frame_us = header.frame_us();
                                        let cycles_per_frame = if header.is_pal {
                                            PAL_CYCLES_PER_FRAME
                                        } else {
                                            NTSC_CYCLES_PER_FRAME
                                        };
                                        let track_info = TrackInfo {
                                            path,
                                            name: header.name.clone(),
                                            author: header.author.clone(),
                                            released: header.released.clone(),
                                            songs: header.songs,
                                            current_song: song,
                                            is_pal: header.is_pal,
                                            is_rsid,
                                            num_sids,
                                            sid_type,
                                            md5,
                                        };
                                        *play_ctx = Some(PlayContext {
                                            engine: PlayEngine::Native,
                                            trampoline: 0,
                                            halt_pc: 0,
                                            frame_us,
                                            cycles_per_frame,
                                            elapsed: Duration::ZERO,
                                            mirror_mono: false,
                                            track_info,
                                            frame_count: 0,
                                            next_frame: Instant::now(),
                                        });
                                        *state = PlayState::Playing;
                                    }
                                }
                                _ => {
                                    eprintln!("[phosphor] Native subtune change failed");
                                }
                            }
                        }
                    }
                } else if let Ok(data) = std::fs::read(&path) {
                    if let Ok(sid_file) = load_sid(&data) {
                        let new_ctx =
                            setup_playback(sid_file, path, song, stereo, sid4, is_rsid, bridge);
                        *play_ctx = Some(new_ctx);
                        *state = PlayState::Playing;
                    }
                }
            }
            send_status(state, play_ctx, last_error, status_tx);
        }

        PlayerCmd::SetEngine(name, addr, pass) => {
            eprintln!("[phosphor] Engine switch → '{name}'");
            stop_playback(play_ctx, bridge);
            // Drop old device.
            if let Some(ref mut br) = bridge {
                br.mute();
                br.close();
                br.shutdown();
            }
            *bridge = None;
            *engine_name = name;
            *u64_address = addr;
            *u64_password = pass;
            *state = PlayState::Stopped;
            send_status(state, play_ctx, last_error, status_tx);
        }

        PlayerCmd::Quit => {}
    }
}

fn stop_playback(ctx: &mut Option<PlayContext>, bridge: &mut Option<Box<dyn SidDevice>>) {
    if ctx.is_some() {
        if let Some(ref mut br) = bridge {
            br.flush();
            br.mute();
            br.set_stereo(0);
            br.reset();
            thread::sleep(Duration::from_millis(50));
        }
        *ctx = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Playback setup
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct PlayContext {
    engine: PlayEngine,
    trampoline: u16,
    halt_pc: u16,
    frame_us: u64,
    cycles_per_frame: u32,
    elapsed: Duration,
    mirror_mono: bool,
    track_info: TrackInfo,
    frame_count: u32,
    next_frame: Instant, // absolute deadline for next frame
}

enum PlayEngine {
    Psid(CPU<C64Memory, Nmos6502>),
    Rsid {
        cpu: CPU<RsidBus, Nmos6502>,
        prev_nmi: bool,
    },
    /// U64 native playback — the real C64 handles SID output.
    /// We only track time; no CPU emulation or register writes.
    Native,
}
#[allow(dead_code)]
impl PlayContext {
    fn is_rsid(&self) -> bool {
        matches!(self.engine, PlayEngine::Rsid { .. })
    }

    fn is_native(&self) -> bool {
        matches!(self.engine, PlayEngine::Native)
    }

    fn sid_writes(&self) -> &[SidWrite] {
        match &self.engine {
            PlayEngine::Psid(cpu) => &cpu.memory.sid_writes,
            PlayEngine::Rsid { cpu, .. } => &cpu.memory.sid_writes,
            PlayEngine::Native => &[],
        }
    }

    fn voice_levels(&self) -> Vec<f32> {
        match &self.engine {
            PlayEngine::Psid(cpu) => cpu.memory.voice_levels(),
            PlayEngine::Rsid { cpu, .. } => cpu.memory.voice_levels(),
            PlayEngine::Native => vec![],
        }
    }

    fn clear_writes(&mut self) {
        match &mut self.engine {
            PlayEngine::Psid(cpu) => cpu.memory.clear_writes(),
            PlayEngine::Rsid { cpu, .. } => cpu.memory.clear_writes(),
            PlayEngine::Native => {}
        }
    }
}

fn setup_playback(
    sid_file: SidFile,
    path: PathBuf,
    song: u16,
    _force_stereo: bool,
    sid4_addr: u16,
    is_rsid: bool,
    bridge: &mut Option<Box<dyn SidDevice>>,
) -> PlayContext {
    let header = &sid_file.header;

    let mut sid_bases: Vec<u16> = vec![0xD400];
    if header.extra_sid_addrs[0] != 0 {
        sid_bases.push(header.extra_sid_addrs[0]);
    }
    if header.extra_sid_addrs[1] != 0 {
        sid_bases.push(header.extra_sid_addrs[1]);
    }
    if sid4_addr != 0 {
        sid_bases.push(sid4_addr);
    }

    let num_sids = sid_bases.len();
    let is_multi = num_sids > 1;
    // Always use stereo mode — mono tunes get mirrored to both channels
    // so sound comes from both speakers.
    let use_stereo = true;
    let mono_mode = !is_multi;
    let mirror_mono = mono_mode; // always mirror single-SID tunes

    let mapper = SidMapper::new(&sid_bases);
    let frame_us = header.frame_us();
    let cycles_per_frame = if header.is_pal {
        PAL_CYCLES_PER_FRAME
    } else {
        NTSC_CYCLES_PER_FRAME
    };

    let sid_type = match num_sids {
        1 => "Mono".to_string(),
        2 => "2SID Stereo".to_string(),
        3 => "3SID".to_string(),
        n => format!("{}SID", n),
    };

    let md5 = compute_hvsc_md5(&sid_file);

    let track_info = TrackInfo {
        path: path.clone(),
        name: header.name.clone(),
        author: header.author.clone(),
        released: header.released.clone(),
        songs: header.songs,
        current_song: song,
        is_pal: header.is_pal,
        is_rsid,
        num_sids,
        sid_type: sid_type.clone(),
        md5,
    };

    // ── Configure hardware ───────────────────────────────────────────────
    if let Some(ref mut br) = bridge {
        br.set_clock_rate(header.is_pal);
        br.reset();
        thread::sleep(Duration::from_millis(50));

        if use_stereo {
            br.set_stereo(1);
        } else {
            br.set_stereo(0);
        }

        let active_sids = if use_stereo && mono_mode { 2 } else { num_sids };
        for i in 0..active_sids {
            let vol_reg = (i as u8) * SID_REG_SIZE + SID_VOL_REG;
            br.write(vol_reg, 0x0F);
        }

        eprintln!(
            "[phosphor] HW: {} {} {} {}, active_sids={}",
            if is_rsid { "RSID" } else { "PSID" },
            if header.is_pal { "PAL" } else { "NTSC" },
            sid_type,
            if header.is_pal { "50Hz" } else { "60Hz" },
            active_sids,
        );
    }

    // ── Build C64 + CPU — branch on RSID vs PSID ─────────────────────

    let trampoline: u16 = 0x0300;
    let halt_pc = trampoline + 3;

    let engine = if is_rsid {
        setup_rsid_engine(
            &sid_file,
            song,
            &mapper,
            mono_mode,
            cycles_per_frame,
            trampoline,
            halt_pc,
        )
    } else {
        setup_psid_engine(&sid_file, song, &mapper, mono_mode, trampoline, halt_pc)
    };

    // Send INIT writes to hardware
    let empty_writes: Vec<(u32, u8, u8)> = Vec::new();
    let init_writes = match &engine {
        PlayEngine::Psid(cpu) => &cpu.memory.sid_writes,
        PlayEngine::Rsid { cpu, .. } => &cpu.memory.sid_writes,
        PlayEngine::Native => &empty_writes,
    };

    if let Some(ref mut br) = bridge {
        for &(_cycle, reg, val) in init_writes {
            br.write(reg, val);
        }
        eprintln!(
            "[phosphor] INIT done, {} SID writes sent",
            init_writes.len()
        );
    }

    // Clear writes and install play trampoline for PSID
    let engine = match engine {
        PlayEngine::Psid(mut cpu) => {
            cpu.memory.clear_writes();
            if header.play_address != 0 {
                cpu.memory
                    .install_trampoline(trampoline, header.play_address);
            }
            PlayEngine::Psid(cpu)
        }
        PlayEngine::Rsid { mut cpu, prev_nmi } => {
            cpu.memory.clear_writes();
            PlayEngine::Rsid { cpu, prev_nmi }
        }
        PlayEngine::Native => PlayEngine::Native,
    };

    PlayContext {
        engine,
        trampoline,
        halt_pc,
        frame_us,
        cycles_per_frame,
        elapsed: Duration::ZERO,
        mirror_mono,
        track_info,
        frame_count: 0,
        next_frame: Instant::now(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  PSID engine setup (unchanged logic, uses C64Memory)
// ─────────────────────────────────────────────────────────────────────────────

fn setup_psid_engine(
    sid_file: &SidFile,
    song: u16,
    mapper: &SidMapper,
    mono_mode: bool,
    trampoline: u16,
    halt_pc: u16,
) -> PlayEngine {
    let header = &sid_file.header;

    let mut mem = C64Memory::new(header.is_pal, mapper.clone(), mono_mode);
    mem.load(sid_file.load_address, &sid_file.payload);
    mem.rebuild_kernal_rom();

    mem.set_hw_vector(0xFFFA, 0xFE43); // NMI
    mem.set_hw_vector(0xFFFE, halt_pc); // IRQ → halt

    mem.install_trampoline(trampoline, header.init_address);

    let mut cpu = CPU::new(mem, Nmos6502);
    cpu.registers.program_counter = trampoline;
    cpu.registers.stack_pointer = StackPointer(0xFD);
    cpu.registers.accumulator = song.saturating_sub(1) as u8;

    run_until(&mut cpu, halt_pc, 2_000_000);
    let init_returned = cpu.registers.program_counter == halt_pc;

    if !init_returned {
        eprintln!(
            "[phosphor] PSID INIT did not return (PC=${:04X})",
            cpu.registers.program_counter
        );
    }

    PlayEngine::Psid(cpu)
}

// ─────────────────────────────────────────────────────────────────────────────
//  RSID engine setup (uses c64_emu for accurate emulation)
// ─────────────────────────────────────────────────────────────────────────────
fn setup_rsid_engine(
    sid_file: &SidFile,
    song: u16,
    mapper: &SidMapper,
    mono_mode: bool,
    _cycles_per_frame: u32,
    trampoline: u16,
    _halt_pc: u16,
) -> PlayEngine {
    let header = &sid_file.header;

    let mut bus = RsidBus::new(header.is_pal, mapper.clone(), mono_mode);

    // Load tune data into RAM
    bus.load(sid_file.load_address, &sid_file.payload);

    // Set up C64 machine state (CPU port, zero-page, DDRs, VIC)
    bus.setup_machine_state(header.is_pal);

    // Build KERNAL stubs (overlays tune data at $E000+ then patches stubs)
    bus.install_kernal_stubs();

    // Install software vectors
    bus.install_software_vectors();

    // Hardware vectors
    bus.set_hw_vector(0xFFFA, 0xFE43); // NMI → KERNAL NMI entry
    bus.set_hw_vector(0xFFFE, 0xFF48); // IRQ → KERNAL IRQ entry

    // Pre-initialize CIA1 for RSID
    bus.setup_rsid_cia_defaults(header.is_pal);

    let load_end = sid_file.load_address as u32 + sid_file.payload.len() as u32;
    if load_end > 0xE000 {
        eprintln!(
            "[phosphor] RSID: tune loads into KERNAL area: ${:04X}-${:04X}",
            sid_file.load_address,
            load_end.min(0xFFFF),
        );
    }

    // Install INIT trampoline + CLI idle loop
    bus.install_trampoline(trampoline, header.init_address);
    bus.c64.ram.ram[0x0303] = 0x58; // CLI
    bus.c64.ram.ram[0x0304] = 0x4C; // JMP $0304
    bus.c64.ram.ram[0x0305] = 0x04;
    bus.c64.ram.ram[0x0306] = 0x03;
    let idle_pc: u16 = 0x0304;

    let mut cpu = CPU::new(bus, Nmos6502);
    cpu.registers.program_counter = trampoline;
    cpu.registers.stack_pointer = StackPointer(0xFD);
    cpu.registers.accumulator = song.saturating_sub(1) as u8;

    // Run INIT with full cycle-accurate hardware emulation.
    // Budget: 30M cycles (~30s C64 time).
    let (init_returned, init_prev_nmi) = run_rsid_init_emu(&mut cpu, idle_pc, 30_000_000);

    // Clear stale CIA interrupt flags
    cpu.memory.clear_stale_ints();

    eprintln!(
        "[phosphor] RSID (c64_emu): load=${:04X} init=${:04X} play=${:04X}",
        sid_file.load_address, header.init_address, header.play_address,
    );
    if init_returned {
        eprintln!("[phosphor] RSID: INIT returned, idle loop active");
    } else {
        cpu.registers.status.remove(Status::PS_DISABLE_INTERRUPTS);
        eprintln!(
            "[phosphor] RSID: INIT did not return (non-returning), PC=${:04X}",
            cpu.registers.program_counter,
        );
    }

    eprintln!(
        "[phosphor] RSID: CIA1 TA latch={}, mask={:#04x}, started={}",
        cpu.memory.c64.cia1.timer_a.latch,
        cpu.memory.c64.cia1.interrupt.icr_mask(),
        cpu.memory.c64.cia1.timer_a.started(),
    );
    eprintln!(
        "[phosphor] RSID: VIC raster_irq={}, irq_state={}",
        cpu.memory.c64.vic.irq_mask_has_raster(),
        cpu.memory.c64.vic.irq_state,
    );
    eprintln!(
        "[phosphor] RSID: CIA2 TA latch={}, mask={:#04x}, started={}",
        cpu.memory.c64.cia2.timer_a.latch,
        cpu.memory.c64.cia2.interrupt.icr_mask(),
        cpu.memory.c64.cia2.timer_a.started(),
    );
    eprintln!(
        "[phosphor] RSID: IRQ vector $0314=${:04X}, NMI vector $0318=${:04X}",
        cpu.memory.c64.ram.ram[0x0314] as u16 | ((cpu.memory.c64.ram.ram[0x0315] as u16) << 8),
        cpu.memory.c64.ram.ram[0x0318] as u16 | ((cpu.memory.c64.ram.ram[0x0319] as u16) << 8),
    );

    PlayEngine::Rsid {
        cpu,
        prev_nmi: init_prev_nmi,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  RSID emulation loops (cycle-accurate, using c64_emu)
// ─────────────────────────────────────────────────────────────────────────────

/// Run RSID INIT with full per-cycle hardware emulation.
fn run_rsid_init_emu(
    cpu: &mut CPU<RsidBus, Nmos6502>,
    idle_pc: u16,
    max_cycles: u32,
) -> (bool, bool) {
    let mut cycles_done: u32 = 0;
    let mut prev_nmi = false;
    let mut check_cycles: u32 = 0;

    while cycles_done < max_cycles {
        if cpu.registers.program_counter == idle_pc {
            return (true, prev_nmi);
        }

        let inst_cycles = cpu.memory.opcode_cycles(cpu.registers.program_counter);
        cpu.single_step();
        cycles_done += inst_cycles;

        // Tick all peripherals for each cycle of the instruction
        for _ in 0..inst_cycles {
            cpu.memory.c64.tick_peripherals();
        }

        // Jiffy clock on VIC frame boundary
        if cpu.memory.c64.vic.new_frame {
            cpu.memory.c64.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }

        // Deliver IRQ (level-triggered)
        if cpu.memory.irq_pending() {
            let irq_cycles = deliver_irq_emu(cpu);
            if irq_cycles > 0 {
                cycles_done += irq_cycles;
                for _ in 0..irq_cycles {
                    cpu.memory.c64.tick_peripherals();
                }
            }
        }

        // Deliver NMI (edge-triggered)
        let cur_nmi = cpu.memory.nmi_pending();
        if cur_nmi && !prev_nmi {
            let nmi_cycles = deliver_nmi_emu(cpu);
            cycles_done += nmi_cycles;
            for _ in 0..nmi_cycles {
                cpu.memory.c64.tick_peripherals();
            }
        }
        prev_nmi = cur_nmi;

        // Periodically check if interrupt-driven playback is ready
        check_cycles += inst_cycles;
        if check_cycles >= 50_000 {
            check_cycles = 0;

            let ram = &cpu.memory.c64.ram.ram;
            let irq_vec = ram[0x0314] as u16 | ((ram[0x0315] as u16) << 8);
            let nmi_vec = ram[0x0318] as u16 | ((ram[0x0319] as u16) << 8);

            let vic_raster_irq = cpu.memory.c64.vic.irq_mask_has_raster();
            let cia1_ta_started = cpu.memory.c64.cia1.timer_a.started();
            let cia1_ta_mask = cpu.memory.c64.cia1.interrupt.icr_mask() & 0x01 != 0;

            let irq_ready =
                irq_vec != 0xEA31 && (vic_raster_irq || (cia1_ta_mask && cia1_ta_started));

            // Check NMI vector
            let kernal_rom = cpu.memory.c64.kernal_rom.rom_ref();
            let nmi_hw_vec =
                kernal_rom[0xFFFA - 0xE000] as u16 | ((kernal_rom[0xFFFB - 0xE000] as u16) << 8);
            let nmi_installed = nmi_vec != 0xFE72 || nmi_hw_vec != 0xFE43;
            let cia2_mask = cpu.memory.c64.cia2.interrupt.icr_mask();
            let cia2_ta_started = cpu.memory.c64.cia2.timer_a.started();
            let nmi_ready = nmi_installed && cia2_mask != 0 && cia2_ta_started;

            if irq_ready || nmi_ready {
                eprintln!(
                    "[phosphor] RSID INIT: playback ready at cycle {} \
                     (IRQ=${:04X} NMI=${:04X})",
                    cycles_done, irq_vec, nmi_vec,
                );
                return (false, prev_nmi);
            }
        }
    }

    (false, prev_nmi)
}

/// Run RSID emulation for `cycles` cycles with per-cycle peripheral ticking.
fn run_rsid_sub_emu(cpu: &mut CPU<RsidBus, Nmos6502>, cycles: u32, prev_nmi: &mut bool) {
    let mut cycles_done: u32 = 0;

    while cycles_done < cycles {
        let inst_cycles = cpu.memory.opcode_cycles(cpu.registers.program_counter);
        cpu.single_step();
        cycles_done += inst_cycles;
        cpu.memory.frame_cycle += inst_cycles;

        // Tick all peripherals for each cycle
        for _ in 0..inst_cycles {
            cpu.memory.c64.tick_peripherals();
        }

        // Jiffy clock on VIC frame boundary
        if cpu.memory.c64.vic.new_frame {
            cpu.memory.c64.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }

        // IRQ (level-triggered)
        if cpu.memory.irq_pending() {
            let irq_cycles = deliver_irq_emu(cpu);
            if irq_cycles > 0 {
                cycles_done += irq_cycles;
                cpu.memory.frame_cycle += irq_cycles;
                for _ in 0..irq_cycles {
                    cpu.memory.c64.tick_peripherals();
                }
            }
        }

        // NMI (edge-triggered)
        let cur_nmi = cpu.memory.nmi_pending();
        if cur_nmi && !*prev_nmi {
            let nmi_cycles = deliver_nmi_emu(cpu);
            cycles_done += nmi_cycles;
            cpu.memory.frame_cycle += nmi_cycles;
            for _ in 0..nmi_cycles {
                cpu.memory.c64.tick_peripherals();
            }
        }
        *prev_nmi = cur_nmi;
    }
}

/// Deliver an IRQ to the CPU (emu variant).
fn deliver_irq_emu(cpu: &mut CPU<RsidBus, Nmos6502>) -> u32 {
    if cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS) {
        return 0;
    }

    let pc = cpu.registers.program_counter;
    let mut sp = cpu.registers.stack_pointer.0;

    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = (pc >> 8) as u8;
    sp = sp.wrapping_sub(1);
    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = (pc & 0xFF) as u8;
    sp = sp.wrapping_sub(1);
    let status_byte = (cpu.registers.status.bits() | 0x20) & !0x10;
    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = status_byte;
    sp = sp.wrapping_sub(1);

    cpu.registers.stack_pointer = StackPointer(sp);
    cpu.registers.status.insert(Status::PS_DISABLE_INTERRUPTS);

    // Read IRQ vector through banking (KERNAL ROM when HIRAM=1)
    let lo = cpu.memory.get_byte(0xFFFE) as u16;
    let hi = cpu.memory.get_byte(0xFFFF) as u16;
    cpu.registers.program_counter = (hi << 8) | lo;

    7
}

/// Deliver an NMI to the CPU (emu variant).
fn deliver_nmi_emu(cpu: &mut CPU<RsidBus, Nmos6502>) -> u32 {
    let pc = cpu.registers.program_counter;
    let mut sp = cpu.registers.stack_pointer.0;

    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = (pc >> 8) as u8;
    sp = sp.wrapping_sub(1);
    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = (pc & 0xFF) as u8;
    sp = sp.wrapping_sub(1);
    let status_byte = (cpu.registers.status.bits() | 0x20) & !0x10;
    cpu.memory.c64.ram.ram[0x0100 | sp as usize] = status_byte;
    sp = sp.wrapping_sub(1);

    cpu.registers.stack_pointer = StackPointer(sp);
    cpu.registers.status.insert(Status::PS_DISABLE_INTERRUPTS);

    let lo = cpu.memory.get_byte(0xFFFA) as u16;
    let hi = cpu.memory.get_byte(0xFFFB) as u16;
    cpu.registers.program_counter = (hi << 8) | lo;

    7
}

// Background player engine. Runs in its own thread, communicates
// with the GUI via crossbeam channels. USB I/O goes through the
// setuid usbsid-bridge helper (fixed-size protocol, async ring buffer).
pub mod hacks;
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
use hacks::{apply_hacks, HackFlags};
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
        /// Some(port) → start U64 audio stream on this port after native playback begins.
        audio_port: Option<u16>,
    },
    Stop,
    TogglePause,
    SetSubtune(u16),
    SetEngine(String, String, String), // (engine_name, u64_address, u64_password)
    UpdateU64Config(String, String),   // (u64_address, u64_password) — no device teardown
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
    /// Raw SID register shadow — 128 bytes (4 SIDs × 32 bytes each).
    /// Indices 0x00–0x1F = SID1, 0x20–0x3F = SID2, etc.
    pub sid_regs: Vec<u8>,
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
fn send_sid_writes(
    bridge: &mut dyn SidDevice,
    writes: &[(u32, u8, u8)],
    mirror_mono: bool,
    cycles_per_frame: u32,
) {
    if writes.is_empty() {
        return;
    }

    // Clamp cycle timestamps to [0, cycles_per_frame].
    // When the player fires right at the frame boundary it executes a few hundred
    // overflow cycles (see PLAYER_OVERFLOW in run_rsid_sub_emu).  Those SID writes
    // have frame_cycle > cycles_per_frame.  Clamping ensures:
    //   • The emulated device never generates more audio samples than one frame worth.
    //   • The hardware device delta stream sums to at most cycles_per_frame so
    //     set_flush() pads correctly from the right position.
    // Musically: overflow writes land at the very end of the audio frame — correct,
    // since they happened "past" the nominal frame boundary anyway.
    let clamp = |c: u32| c.min(cycles_per_frame);

    if mirror_mono {
        // Mono: duplicate each write for SID2 at delta=0 (same cycle position).
        let mut cycled: Vec<(u16, u8, u8)> = Vec::with_capacity(writes.len() * 2);
        let mut prev_cycle: u32 = 0;

        for &(cycle, reg, val) in writes {
            let cycle = clamp(cycle);
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
            let cycle = clamp(cycle);
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
        .stack_size(4 * 1024 * 1024) // 4MB — shadow CPU needs space for C64 memory
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
                                        ctx.cycles_per_frame,
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
                                        ctx.cycles_per_frame,
                                    );
                                }
                            }
                            PlayEngine::Native { shadow } => {
                                // Run shadow CPU for visualization only.
                                match shadow {
                                    NativeShadow::Psid {
                                        cpu,
                                        trampoline,
                                        halt_pc,
                                    } => {
                                        cpu.memory.clear_writes();
                                        cpu.registers.program_counter = *trampoline;
                                        cpu.registers.stack_pointer = StackPointer(0xFD);
                                        run_until(cpu, *halt_pc, 200_000);
                                    }
                                    NativeShadow::Rsid { cpu, prev_nmi } => {
                                        cpu.memory.clear_writes();
                                        run_rsid_sub_emu(cpu, ctx.cycles_per_frame, prev_nmi);
                                    }
                                }
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
                        ctx.next_frame += frame_dur;

                        let now = Instant::now();
                        if ctx.next_frame < now {
                            // Frame overrun — log it periodically
                            let overrun = now - ctx.next_frame;
                            if ctx.frame_count % 250 == 0 {
                                eprintln!(
                                    "[phosphor] frame {} overrun by {}µs (sids={})",
                                    ctx.frame_count,
                                    overrun.as_micros(),
                                    ctx.track_info.num_sids,
                                );
                            }
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
    let (info, elapsed, levels, writes, regs) = match ctx {
        Some(c) => (
            Some(c.track_info.clone()),
            c.elapsed,
            c.voice_levels(),
            c.sid_writes().len(),
            c.sid_regs(),
        ),
        None => (None, Duration::ZERO, vec![], 0, vec![0u8; 128]),
    };

    let _ = tx.try_send(PlayerStatus {
        state: state.clone(),
        track_info: info,
        elapsed,
        voice_levels: levels,
        writes_per_frame: writes,
        error: error.clone(),
        sid_regs: regs,
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
            audio_port,
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
                        // Start audio streaming back to host if configured.
                        if let Some(port) = audio_port {
                            if let Err(e) = br.start_audio(port) {
                                eprintln!("[phosphor] U64 audio stream failed to start: {e}");
                            }
                        }
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
                // Build context with shadow CPU for visualization.
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
                    path: path.clone(),
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

                // Build shadow emulation for visualization
                let mut sid_bases: Vec<u16> = vec![0xD400];
                if header.extra_sid_addrs[0] != 0 {
                    sid_bases.push(header.extra_sid_addrs[0]);
                }
                if header.extra_sid_addrs[1] != 0 {
                    sid_bases.push(header.extra_sid_addrs[1]);
                }
                let mapper = SidMapper::new(&sid_bases);
                let mono_mode = num_sids <= 1;
                let trampoline: u16 = 0x0300;
                let halt_pc = trampoline + 3;

                let shadow = if is_rsid {
                    let engine = setup_rsid_engine(
                        &sid_file,
                        song,
                        &mapper,
                        mono_mode,
                        cycles_per_frame,
                        trampoline,
                        halt_pc,
                    );
                    match engine {
                        PlayEngine::Rsid { mut cpu, prev_nmi } => {
                            cpu.memory.clear_writes();
                            NativeShadow::Rsid { cpu, prev_nmi }
                        }
                        _ => unreachable!(),
                    }
                } else {
                    let engine =
                        setup_psid_engine(&sid_file, song, &mapper, mono_mode, trampoline, halt_pc);
                    match engine {
                        PlayEngine::Psid(mut cpu) => {
                            cpu.memory.clear_writes();
                            if header.play_address != 0 {
                                cpu.memory
                                    .install_trampoline(trampoline, header.play_address);
                            }
                            NativeShadow::Psid {
                                cpu,
                                trampoline,
                                halt_pc,
                            }
                        }
                        _ => unreachable!(),
                    }
                };

                *play_ctx = Some(PlayContext {
                    engine: PlayEngine::Native { shadow },
                    trampoline,
                    halt_pc,
                    frame_us,
                    cycles_per_frame,
                    elapsed: Duration::ZERO,
                    mirror_mono: false,
                    track_info,
                    frame_count: 0,
                    next_frame: Instant::now(),
                    audio_port,
                });
            } else {
                let mut ctx = setup_playback(
                    sid_file,
                    path,
                    song,
                    force_stereo,
                    sid4_addr,
                    is_rsid,
                    bridge,
                );
                ctx.audio_port = audio_port;
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
            // For native engines (U64) we use the hardware pause/resume API so
            // the C64 clock freezes mid-frame and resumes from exactly the same
            // point — no song restart, no clock drift.
            let is_native = play_ctx.as_ref().map(|c| c.is_native()).unwrap_or(false);

            match state {
                PlayState::Playing if is_native => {
                    if let Some(ref mut br) = bridge {
                        match br.pause_machine() {
                            Ok(()) => {
                                *state = PlayState::Paused;
                                eprintln!("[phosphor] U64 machine paused");
                            }
                            Err(e) => {
                                *last_error = Some(e.clone());
                                eprintln!("[phosphor] U64 pause failed: {e}");
                            }
                        }
                    }
                }
                PlayState::Paused if is_native => {
                    if let Some(ref mut br) = bridge {
                        match br.resume_machine() {
                            Ok(()) => {
                                *state = PlayState::Playing;
                                eprintln!("[phosphor] U64 machine resumed");
                            }
                            Err(e) => {
                                *last_error = Some(e.clone());
                                eprintln!("[phosphor] U64 resume failed: {e}");
                            }
                        }
                    }
                }
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
                // Preserve the audio port so we can restart streaming after the
                // subtune change — stop_playback kills the audio stream.
                let saved_audio_port = ctx.audio_port;
                let sid4 = 0;
                // Keep audio stream alive — it's a continuous UDP flow from the
                // U64 that doesn't need to be restarted on a subtune change.
                stop_playback_keep_audio(play_ctx, bridge);

                if was_native {
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
                                            path: path.clone(),
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

                                        // Build shadow CPU for visualization
                                        let mut sid_bases: Vec<u16> = vec![0xD400];
                                        if header.extra_sid_addrs[0] != 0 {
                                            sid_bases.push(header.extra_sid_addrs[0]);
                                        }
                                        if header.extra_sid_addrs[1] != 0 {
                                            sid_bases.push(header.extra_sid_addrs[1]);
                                        }
                                        let mapper = SidMapper::new(&sid_bases);
                                        let mono_mode = num_sids <= 1;
                                        let trampoline: u16 = 0x0300;
                                        let halt_pc = trampoline + 3;

                                        let shadow = if is_rsid {
                                            let engine = setup_rsid_engine(
                                                &sid_file,
                                                song,
                                                &mapper,
                                                mono_mode,
                                                cycles_per_frame,
                                                trampoline,
                                                halt_pc,
                                            );
                                            match engine {
                                                PlayEngine::Rsid { mut cpu, prev_nmi } => {
                                                    cpu.memory.clear_writes();
                                                    NativeShadow::Rsid { cpu, prev_nmi }
                                                }
                                                _ => unreachable!(),
                                            }
                                        } else {
                                            let engine = setup_psid_engine(
                                                &sid_file, song, &mapper, mono_mode, trampoline,
                                                halt_pc,
                                            );
                                            match engine {
                                                PlayEngine::Psid(mut cpu) => {
                                                    cpu.memory.clear_writes();
                                                    if header.play_address != 0 {
                                                        cpu.memory.install_trampoline(
                                                            trampoline,
                                                            header.play_address,
                                                        );
                                                    }
                                                    NativeShadow::Psid {
                                                        cpu,
                                                        trampoline,
                                                        halt_pc,
                                                    }
                                                }
                                                _ => unreachable!(),
                                            }
                                        };

                                        *play_ctx = Some(PlayContext {
                                            engine: PlayEngine::Native { shadow },
                                            trampoline,
                                            halt_pc,
                                            frame_us,
                                            cycles_per_frame,
                                            elapsed: Duration::ZERO,
                                            mirror_mono: false,
                                            track_info,
                                            frame_count: 0,
                                            next_frame: Instant::now(),
                                            audio_port: saved_audio_port,
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

        PlayerCmd::UpdateU64Config(addr, pass) => {
            eprintln!("[phosphor] U64 config updated (addr={addr})");
            *u64_address = addr;
            *u64_password = pass;
            // Drop existing U64 connection so next Play reconnects with new config.
            if engine_name == "u64" {
                if let Some(ref mut br) = bridge {
                    br.close();
                }
                *bridge = None;
            }
        }

        PlayerCmd::Quit => {}
    }
}

fn stop_playback(ctx: &mut Option<PlayContext>, bridge: &mut Option<Box<dyn SidDevice>>) {
    stop_playback_inner(ctx, bridge, true);
}

fn stop_playback_keep_audio(
    ctx: &mut Option<PlayContext>,
    bridge: &mut Option<Box<dyn SidDevice>>,
) {
    stop_playback_inner(ctx, bridge, false);
}

fn stop_playback_inner(
    ctx: &mut Option<PlayContext>,
    bridge: &mut Option<Box<dyn SidDevice>>,
    stop_audio: bool,
) {
    if ctx.is_some() {
        if let Some(ref mut br) = bridge {
            if stop_audio {
                br.stop_audio();
            }
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
    /// UDP port for U64 audio streaming, if active. Preserved across subtune changes.
    audio_port: Option<u16>,
}

enum PlayEngine {
    Psid(CPU<C64Memory, Nmos6502>),
    Rsid {
        cpu: CPU<RsidBus, Nmos6502>,
        prev_nmi: bool,
    },
    /// U64 native playback — real C64 handles SID output.
    /// Shadow CPU runs locally for visualization only.
    Native {
        shadow: NativeShadow,
    },
}

/// Shadow CPU for native playback visualization.
enum NativeShadow {
    Psid {
        cpu: CPU<C64Memory, Nmos6502>,
        trampoline: u16,
        halt_pc: u16,
    },
    Rsid {
        cpu: CPU<RsidBus, Nmos6502>,
        prev_nmi: bool,
    },
}
#[allow(dead_code)]
impl PlayContext {
    fn is_rsid(&self) -> bool {
        matches!(self.engine, PlayEngine::Rsid { .. })
    }

    fn is_native(&self) -> bool {
        matches!(self.engine, PlayEngine::Native { .. })
    }

    fn sid_writes(&self) -> &[SidWrite] {
        match &self.engine {
            PlayEngine::Psid(cpu) => &cpu.memory.sid_writes,
            PlayEngine::Rsid { cpu, .. } => &cpu.memory.sid_writes,
            PlayEngine::Native { shadow } => match shadow {
                NativeShadow::Psid { cpu, .. } => &cpu.memory.sid_writes,
                NativeShadow::Rsid { cpu, .. } => &cpu.memory.sid_writes,
            },
        }
    }

    fn voice_levels(&self) -> Vec<f32> {
        match &self.engine {
            PlayEngine::Psid(cpu) => cpu.memory.voice_levels(),
            PlayEngine::Rsid { cpu, .. } => cpu.memory.voice_levels(),
            PlayEngine::Native { shadow } => match shadow {
                NativeShadow::Psid { cpu, .. } => cpu.memory.voice_levels(),
                NativeShadow::Rsid { cpu, .. } => cpu.memory.voice_levels(),
            },
        }
    }

    /// Return a copy of the raw SID register shadow for the UI panel.
    fn sid_regs(&self) -> Vec<u8> {
        match &self.engine {
            PlayEngine::Psid(cpu) => cpu.memory.sid_shadow.to_vec(),
            PlayEngine::Rsid { cpu, .. } => cpu.memory.sid_shadow.to_vec(),
            PlayEngine::Native { shadow } => match shadow {
                NativeShadow::Psid { cpu, .. } => cpu.memory.sid_shadow.to_vec(),
                NativeShadow::Rsid { cpu, .. } => cpu.memory.sid_shadow.to_vec(),
            },
        }
    }

    fn clear_writes(&mut self) {
        match &mut self.engine {
            PlayEngine::Psid(cpu) => cpu.memory.clear_writes(),
            PlayEngine::Rsid { cpu, .. } => cpu.memory.clear_writes(),
            PlayEngine::Native { shadow } => match shadow {
                NativeShadow::Psid { cpu, .. } => cpu.memory.clear_writes(),
                NativeShadow::Rsid { cpu, .. } => cpu.memory.clear_writes(),
            },
        }
    }
}

fn setup_playback(
    sid_file: SidFile,
    path: PathBuf,
    song: u16,
    force_stereo: bool,
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
    let use_stereo = true;
    // force_stereo: treat 2SID tunes as mono (mirror SID1 to both channels).
    // Only applies to exactly 2-SID tunes.
    let force_mono_2sid = force_stereo && num_sids == 2;
    let mono_mode = !is_multi || force_mono_2sid;
    let mirror_mono = mono_mode;

    // When forcing stereo on a 2SID tune, use only the base SID for mapping
    let mapper = if force_mono_2sid {
        SidMapper::new(&sid_bases[..1])
    } else {
        SidMapper::new(&sid_bases)
    };
    let mut frame_us = header.frame_us();
    let mut cycles_per_frame = if header.is_pal {
        PAL_CYCLES_PER_FRAME
    } else {
        NTSC_CYCLES_PER_FRAME
    };

    let sid_type = if force_mono_2sid {
        "2SID → Stereo (forced mono)".to_string()
    } else {
        match num_sids {
            1 => "Mono".to_string(),
            2 => "2SID Stereo".to_string(),
            3 => "3SID".to_string(),
            n => format!("{}SID", n),
        }
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
            // Tell the device how many SIDs are active:
            //   mono tunes in stereo mode → set_stereo(1) for mirror
            //   multi-SID tunes → set_stereo(num_sids - 1)
            let active_sids = if mono_mode { 2 } else { num_sids };
            br.set_stereo((active_sids - 1) as i32);
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
        PlayEngine::Native { .. } => &empty_writes,
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
    //
    // For PSID with speed bit set (CIA timing), read the CIA1 Timer A latch
    // that the tune programmed during INIT. This tells us the intended
    // play call rate. Many 2SID tunes use ~100Hz (2× frame rate).
    let engine = match engine {
        PlayEngine::Psid(mut cpu) => {
            // Check if this song uses CIA timing
            let song_idx = song.saturating_sub(1) as u32;
            let speed_bit = if song_idx < 32 {
                (header.speed >> song_idx) & 1
            } else {
                (header.speed >> 31) & 1
            };

            if speed_bit == 1 && !header.is_rsid {
                let cia_latch = cpu.memory.cia1.timer_a.latch as u64;
                if cia_latch > 0 && cia_latch < 0xFFFF {
                    let clock = if header.is_pal { 985248u64 } else { 1022727u64 };
                    let cia_us = (cia_latch * 1_000_000) / clock;
                    let cia_hz = clock as f64 / cia_latch as f64;
                    eprintln!(
                        "[phosphor] CIA timing: latch={} cycles = {}µs ({:.1}Hz)",
                        cia_latch, cia_us, cia_hz,
                    );
                    // Override frame timing to match CIA rate
                    if cia_us > 0 && cia_us < frame_us {
                        frame_us = cia_us;
                        cycles_per_frame = cia_latch as u32;
                        eprintln!(
                            "[phosphor] Frame rate adjusted to {}µs ({:.1}Hz) from CIA timer",
                            frame_us, cia_hz,
                        );
                    }
                }
            }

            cpu.memory.clear_writes();
            if header.play_address != 0 {
                cpu.memory
                    .install_trampoline(trampoline, header.play_address);
            }
            PlayEngine::Psid(cpu)
        }
        PlayEngine::Rsid { mut cpu, prev_nmi } => {
            // ── CIA latch rate detection for RSID ─────────────────────────
            // RSID tunes program CIA1 Timer A during their INIT routine just
            // like PSIDs do.  Read the latch (the reload value the tune wrote,
            // not the live counter) to find the intended play rate.
            //
            // For RSID the speed-bit concept doesn't exist — CIA timing is
            // always the case — so we check unconditionally.  We only override
            // if the latch encodes a rate meaningfully different from the
            // default VIC frame rate (>5% off) to avoid reacting to rounding.
            //
            // Access path differs from PSID: RsidBus exposes the c64_emu CIA
            // directly as cpu.memory.c64.cia1.timer_a.latch (u16), whereas
            // C64Memory uses cpu.memory.cia1.timer_a.latch.
            {
                let cia_latch = cpu.memory.c64.cia1.timer_a.latch as u64;
                let clock: u64 = if header.is_pal { 985_248 } else { 1_022_727 };
                let default_frame_cycles: u64 = if header.is_pal {
                    PAL_CYCLES_PER_FRAME as u64
                } else {
                    NTSC_CYCLES_PER_FRAME as u64
                };

                if cia_latch > 0 && cia_latch < 0xFFFF {
                    let cia_us = (cia_latch * 1_000_000) / clock;
                    let cia_hz = clock as f64 / cia_latch as f64;
                    let ratio = cia_latch as f64 / default_frame_cycles as f64;

                    eprintln!(
                        "[phosphor] RSID CIA1 Timer A latch={} → {}µs ({:.1}Hz)",
                        cia_latch, cia_us, cia_hz,
                    );

                    // Only override when:
                    // 1. Rate is meaningfully non-standard (>5% off VIC frame rate).
                    //    Tunes that don't program CIA1 leave the latch at our startup
                    //    default (= VIC frame cycles), so ratio ≈ 1.0 → no override.
                    // 2. Rate is sane (between 20Hz and 300Hz). Bogus latch values
                    //    from tunes that briefly use CIA1 for other purposes and then
                    //    reprogram it won't have a realistic play rate.
                    if cia_us > 0
                        && (ratio < 0.95 || ratio > 1.05)
                        && cia_hz >= 20.0
                        && cia_hz <= 300.0
                    {
                        frame_us = cia_us;
                        cycles_per_frame = cia_latch as u32;
                        eprintln!(
                            "[phosphor] RSID frame rate adjusted to {}µs ({:.1}Hz) from CIA latch",
                            frame_us, cia_hz,
                        );
                    }
                }
            }

            cpu.memory.clear_writes();
            PlayEngine::Rsid { cpu, prev_nmi }
        }
        n @ PlayEngine::Native { .. } => n,
    };

    // If CIA timing adjusted cycles_per_frame, tell the device
    // so flush() generates the correct amount of audio.
    if let Some(ref mut br) = bridge {
        br.set_cycles_per_frame(cycles_per_frame);
    }

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
        audio_port: None,
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

    bus.load(sid_file.load_address, &sid_file.payload);
    bus.setup_machine_state(header.is_pal);
    bus.install_kernal_stubs();
    bus.install_software_vectors();
    bus.set_hw_vector(0xFFFA, 0xFE43); // NMI → KERNAL NMI entry
    bus.set_hw_vector(0xFFFE, 0xFF48); // IRQ → KERNAL IRQ entry
    bus.setup_rsid_cia_defaults(header.is_pal);

    let load_end = sid_file.load_address as u32 + sid_file.payload.len() as u32;
    if load_end > 0xE000 {
        eprintln!(
            "[phosphor] RSID: tune loads into KERNAL area: ${:04X}-${:04X}",
            sid_file.load_address,
            load_end.min(0xFFFF),
        );
    }

    let hack_flags = apply_hacks(&mut bus, header.init_address);

    // ── BASIC RSID path ──────────────────────────────────────────────────
    let (mut bus, effective_init_addr, is_basic_mode) = if header.is_basic {
        let roms_ok = bus.c64.kernal_rom.rom_ref()[0x39a] != 0x60;
        if roms_ok {
            bus.install_trampoline(trampoline, 0xFCE2);
            let reset_idle: u16 = 0x0303;
            bus.c64.ram.ram[0x0303] = 0x4C;
            bus.c64.ram.ram[0x0304] = 0x03;
            bus.c64.ram.ram[0x0305] = 0x03;

            let mut reset_cpu = CPU::new(bus, Nmos6502);
            reset_cpu.registers.program_counter = trampoline;
            reset_cpu.registers.stack_pointer = StackPointer(0xFD);
            eprintln!("[phosphor] RSID BASIC: running KERNAL reset at $FCE2");
            run_rsid_init_emu(&mut reset_cpu, reset_idle, 5_000_000, &hack_flags);

            let mut bus = reset_cpu.memory;
            bus.c64.ram.ram[0x030C] = song.saturating_sub(1) as u8;
            eprintln!(
                "[phosphor] RSID BASIC: subtune={} stored at $030C, starting at $A7AE",
                song.saturating_sub(1)
            );
            (bus, 0xA7AE_u16, true)
        } else {
            let sys_addr = {
                let ram = &bus.c64.ram.ram;
                let next_line = ram[0x0801] as u16 | ((ram[0x0802] as u16) << 8);
                if next_line > 0x0805
                    && ram[0x0805] == 0x9E
                    && (next_line as usize) < ram.len()
                    && ram[next_line as usize] == 0x00
                {
                    let mut addr: u16 = 0;
                    for i in 0x0806..=(next_line as usize) {
                        let c = ram[i];
                        if c >= b'0' && c <= b'9' {
                            addr = addr * 10 + (c - b'0') as u16;
                        } else if addr > 0 {
                            break;
                        }
                    }
                    if addr > 0 {
                        addr
                    } else {
                        header.init_address
                    }
                } else {
                    header.init_address
                }
            };
            eprintln!("[phosphor] RSID BASIC (no ROMs): using ${:04X}", sys_addr);
            (bus, sys_addr, false)
        }
    } else {
        (bus, header.init_address, false)
    };

    // Install INIT trampoline + CLI idle loop
    bus.install_trampoline(trampoline, effective_init_addr);
    bus.c64.ram.ram[0x0303] = 0x58; // CLI
    bus.c64.ram.ram[0x0304] = 0x4C; // JMP $0304  (idle loop)
    bus.c64.ram.ram[0x0305] = 0x04;
    bus.c64.ram.ram[0x0306] = 0x03;

    let mut cpu = CPU::new(bus, Nmos6502);
    cpu.registers.program_counter = trampoline;
    cpu.registers.stack_pointer = StackPointer(0xFD);
    if !is_basic_mode {
        cpu.registers.accumulator = song.saturating_sub(1) as u8;
    }

    // ── Reference architecture: NO separate INIT phase for RSID ──────────────
    //
    // WebSid's startupTune() for RSID simply does:
    //   1. resetDefaults() / memRsidInit() / sysReset()
    //   2. cpuSetProgramCounter(trampoline, song)
    //   ...then the audio loop (runEmulation) starts immediately.
    //
    // INIT runs as part of the audio loop from the very first cycle.
    // - Returning tunes:     JSR init_addr → INIT returns → JMP idle → CIA1 IRQ → play
    // - Non-returning tunes: JSR init_addr → INIT loops forever → NMIs provide timing
    //
    // There is NO 5M-cycle "burn-in" phase. Doing so was wrong because:
    //   - For non-returning tunes (NMI-driven busy loops) it wasted hardware state
    //     and advanced the tune's state machine before any audio was captured.
    //   - The tune was re-run from the trampoline but with contaminated CIA state.
    //
    // We match the reference exactly: set PC=trampoline and hand off to sub_emu.
    // sub_emu will run the trampoline (JSR init_addr …) and collect SID writes
    // from the very first cycle.

    eprintln!(
        "[phosphor] RSID (c64_emu): load=${:04X} init=${:04X} play=${:04X}",
        sid_file.load_address, header.init_address, header.play_address,
    );
    eprintln!(
        "[phosphor] RSID: IRQ vector $0314=${:04X}, NMI vector $0318=${:04X}",
        cpu.memory.c64.ram.ram[0x0314] as u16 | ((cpu.memory.c64.ram.ram[0x0315] as u16) << 8),
        cpu.memory.c64.ram.ram[0x0318] as u16 | ((cpu.memory.c64.ram.ram[0x0319] as u16) << 8),
    );

    cpu.memory.hack_flags = hack_flags;

    PlayEngine::Rsid {
        cpu,
        prev_nmi: false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  RSID emulation loops (cycle-accurate, using c64_emu)
// ─────────────────────────────────────────────────────────────────────────────

/// Run RSID INIT with full per-cycle hardware emulation.
///
/// Runs until:
/// - PC reaches `idle_pc` (INIT returned normally), or
/// - `max_cycles` budget is exhausted (non-returning INIT).
///
/// No early-exit heuristics — we let INIT run to completion, matching
/// WebSid's behaviour.  Returns `(init_returned, prev_nmi)`.
fn run_rsid_init_emu(
    cpu: &mut CPU<RsidBus, Nmos6502>,
    idle_pc: u16,
    max_cycles: u32,
    hack_flags: &HackFlags,
) -> (bool, bool) {
    let mut cycles_done: u32 = 0;
    let mut prev_nmi = false;

    while cycles_done < max_cycles {
        if cpu.registers.program_counter == idle_pc {
            return (true, prev_nmi);
        }

        // ── Fix #1: BA stun — honour VIC bus-not-available ────────────────
        // When BA is low the CPU cannot access the bus; only peripherals tick.
        // Insert stun cycles until BA goes high again.
        if !hack_flags.disable_badline_stun {
            while cpu.memory.c64.is_cpu_jammed() {
                cpu.memory.c64.tick_peripherals();
                cycles_done += 1;
                if cycles_done >= max_cycles {
                    return (false, prev_nmi);
                }
            }
        }

        // ── Fix #2: Check IRQ BEFORE stepping the CPU ─────────────────────
        // Delivering before single_step() eliminates the "last instruction
        // already ran" window that caused systematic cycle timing errors.
        let irq_cycles = if cpu.memory.irq_pending()
            && !cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS)
        {
            let c = deliver_irq_emu(cpu);
            for _ in 0..c {
                cpu.memory.c64.tick_peripherals();
            }
            c
        } else {
            0
        };

        // ── Fix #2 (NMI): Check NMI BEFORE stepping the CPU ──────────────
        // NMI gate mirrors reference: ciaNMI() && (_no_nmi_hack || _no_flag_i)
        // When nmi_needs_i_flag_clear is set (_no_nmi_hack=0), NMI may only
        // fire if the CPU I-flag is currently CLEAR (_no_flag_i=1).
        let cur_nmi = cpu.memory.nmi_pending();
        let i_flag_clear = !cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS);
        let nmi_allowed = !hack_flags.nmi_needs_i_flag_clear || i_flag_clear;

        let nmi_cycles = if cur_nmi && !prev_nmi && nmi_allowed {
            let c = deliver_nmi_emu(cpu);
            for _ in 0..c {
                cpu.memory.c64.tick_peripherals();
            }
            c
        } else {
            0
        };
        prev_nmi = cur_nmi;

        cycles_done += irq_cycles + nmi_cycles;

        // Execute next instruction (only if no interrupt was delivered)
        if irq_cycles == 0 && nmi_cycles == 0 {
            let inst_cycles = cpu.memory.opcode_cycles(cpu.registers.program_counter);
            cpu.single_step();
            cycles_done += inst_cycles;

            for _ in 0..inst_cycles {
                cpu.memory.c64.tick_peripherals();
            }
        }

        // Jiffy clock on VIC frame boundary
        if cpu.memory.c64.vic.new_frame {
            cpu.memory.c64.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }
    }

    (false, prev_nmi)
}

/// Run RSID emulation for `cycles` cycles with per-cycle peripheral ticking.
///
/// Fixes applied vs. original:
/// 1. BA stun — CPU pauses when VIC holds bus (badlines / sprite DMA).
/// 2. IRQ/NMI delivered BEFORE single_step(), eliminating timing drift.
/// 3. NMI-during-RTI guard honoured when hack flag is set.
fn run_rsid_sub_emu(cpu: &mut CPU<RsidBus, Nmos6502>, cycles: u32, prev_nmi: &mut bool) {
    let mut cycles_done: u32 = 0;
    let hack_flags = cpu.memory.hack_flags.clone();

    // Maximum extra cycles we run past the frame boundary to let an IRQ handler
    // (the tune's player routine) finish after it was triggered right at or near
    // the end of the frame.
    //
    // Why this is needed:
    //   CIA1 Timer A is set to exactly `cycles` (the frame length), so it fires
    //   on the very last cycle of the frame.  We deliver the IRQ (7 cycles),
    //   which pushes cycles_done past `cycles`.  Without overflow budget, the
    //   loop exits immediately and the player never runs.  On the next frame the
    //   CPU's PC is still inside the IRQ vector so the player executes at the
    //   START of that frame — one frame late — causing every other frame to
    //   receive a double player call and the audio to be badly out of sync.
    //
    //   On real hardware the player routine runs, then the CPU sits in a CLI idle
    //   loop until the next CIA fire.  We replicate that: run up to
    //   `cycles + PLAYER_OVERFLOW` cycles so the player can always complete, then
    //   the idle loop will trigger the cycle-count exit naturally on the next frame.
    //
    //   4096 is well above any known SID player routine (typically 100–500 cycles).
    //   SID writes that occur in the overflow are stamped with frame_cycle values
    //   slightly above `cycles`; flush()/send_sid_writes() handles this correctly
    //   via saturating_sub (remaining = 0 if last_write > cycles_per_frame).
    const PLAYER_OVERFLOW: u32 = 4096;
    let hard_limit = cycles + PLAYER_OVERFLOW;

    // true while the CPU is executing an IRQ or NMI handler (I-flag set by us).
    // We track this so the loop does not exit mid-handler when cycles_done >= cycles.
    let mut in_interrupt: bool = false;

    while cycles_done < hard_limit {
        // Exit once we've run the full frame AND we're not inside an interrupt handler.
        // This lets a player that was triggered right at the frame boundary finish.
        if cycles_done >= cycles && !in_interrupt {
            break;
        }

        // ── BA stun ───────────────────────────────────────────────────────
        if !hack_flags.disable_badline_stun {
            while cpu.memory.c64.is_cpu_jammed() {
                cpu.memory.c64.tick_peripherals();
                cpu.memory.frame_cycle += 1;
                cycles_done += 1;
                if cycles_done >= hard_limit {
                    return;
                }
            }
        }

        // ── Deliver IRQ BEFORE executing the next instruction ─────────────
        let irq_cycles = if cpu.memory.irq_pending()
            && !cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS)
        {
            let c = deliver_irq_emu(cpu);
            for _ in 0..c {
                cpu.memory.c64.tick_peripherals();
            }
            in_interrupt = true;
            c
        } else {
            0
        };

        // ── Deliver NMI before instruction, with I-flag gate ──────────────
        let cur_nmi = cpu.memory.nmi_pending();
        let i_flag_clear = !cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS);
        let nmi_allowed = !hack_flags.nmi_needs_i_flag_clear || i_flag_clear;

        let nmi_cycles = if cur_nmi && !*prev_nmi && nmi_allowed {
            let c = deliver_nmi_emu(cpu);
            for _ in 0..c {
                cpu.memory.c64.tick_peripherals();
            }
            in_interrupt = true;
            c
        } else {
            0
        };
        *prev_nmi = cur_nmi;

        let int_cycles = irq_cycles + nmi_cycles;
        cycles_done += int_cycles;
        cpu.memory.frame_cycle += int_cycles;

        if int_cycles > 0 {
            if cpu.memory.c64.vic.new_frame {
                cpu.memory.c64.vic.new_frame = false;
                cpu.memory.tick_jiffy_clock();
            }
            continue;
        }

        // ── Execute one instruction ───────────────────────────────────────
        let inst_cycles = cpu.memory.opcode_cycles(cpu.registers.program_counter);
        cpu.single_step();
        cycles_done += inst_cycles;
        cpu.memory.frame_cycle += inst_cycles;

        for _ in 0..inst_cycles {
            cpu.memory.c64.tick_peripherals();
        }

        // Detect RTI: if I-flag was set (we're in an interrupt) and the CPU
        // just executed RTI ($40), the interrupt handler has returned.
        // The CPU restored the status register from the stack, which will have
        // cleared the I-flag (most SID players run with I clear in the main loop).
        if in_interrupt && !cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS) {
            in_interrupt = false;
        }

        if cpu.memory.c64.vic.new_frame {
            cpu.memory.c64.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }
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

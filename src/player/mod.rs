// Background player engine. Runs in its own thread, communicates
// with the GUI via crossbeam channels. USB I/O goes through the
// setuid usbsid-bridge helper (fixed-size protocol, async ring buffer).

pub mod memory;
pub mod sid_file;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, select, tick, Receiver, Sender};
use mos6502::cpu::CPU;
use mos6502::instruction::Nmos6502;
use mos6502::registers::{StackPointer, Status};

use crate::sid_device::{create_device, SidDevice};
use memory::*;
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

/// Run RSID INIT with full hardware emulation (CIA/VIC ticking + interrupt delivery).
///
/// Like libsidplayfp, this is continuous CPU execution — no artificial phases.
/// Budget is in cycles (not steps). If INIT returns (reaches idle_pc), stops early.
/// Also exits early once the tune has installed interrupt handlers AND enabled
/// the matching interrupt source — meaning playback is ready.
/// If INIT doesn't return, the frame loop takes over seamlessly.
/// Returns (init_returned, final_prev_nmi).
fn run_rsid_init(
    cpu: &mut CPU<C64Memory, Nmos6502>,
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

        let inst_cycles = opcode_cycles_banked(&cpu.memory, cpu.registers.program_counter);
        cpu.single_step();
        cycles_done += inst_cycles;

        // Tick all hardware
        cpu.memory.cia1.tick(inst_cycles);
        cpu.memory.cia2.tick(inst_cycles);
        cpu.memory.vic.tick(inst_cycles);

        // Account for VIC badline stolen cycles
        let stolen = cpu.memory.vic.stolen_cycles;
        if stolen > 0 {
            cycles_done += stolen;
            cpu.memory.cia1.tick(stolen);
            cpu.memory.cia2.tick(stolen);
        }

        // Jiffy clock: increment $00A2 on VIC frame boundary.
        // Many tunes poll this for timing during INIT (decompression etc.)
        if cpu.memory.vic.new_frame {
            cpu.memory.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }

        // Deliver IRQ (level-triggered)
        if cpu.memory.cia1.int_pending() || cpu.memory.vic.irq_line {
            let irq_cycles = deliver_irq(cpu);
            if irq_cycles > 0 {
                cycles_done += irq_cycles;
                cpu.memory.cia1.tick(irq_cycles);
                cpu.memory.cia2.tick(irq_cycles);
                cpu.memory.vic.tick(irq_cycles);
            }
        }

        // Deliver NMI (edge-triggered)
        let cur_nmi = cpu.memory.cia2.int_pending();
        if cur_nmi && !prev_nmi {
            let nmi_cycles = deliver_nmi(cpu);
            cycles_done += nmi_cycles;
            cpu.memory.cia1.tick(nmi_cycles);
            cpu.memory.cia2.tick(nmi_cycles);
            cpu.memory.vic.tick(nmi_cycles);
        }
        prev_nmi = cur_nmi;

        // Periodically check if interrupt-driven playback is ready.
        check_cycles += inst_cycles;
        if check_cycles >= 50_000 {
            check_cycles = 0;

            let irq_vec = cpu.memory.ram[0x0314] as u16 | ((cpu.memory.ram[0x0315] as u16) << 8);
            let nmi_vec = cpu.memory.ram[0x0318] as u16 | ((cpu.memory.ram[0x0319] as u16) << 8);

            // IRQ ready: custom handler installed + interrupt source enabled
            let irq_ready = irq_vec != 0xEA31
                && (cpu.memory.vic.raster_irq_enabled
                    || (cpu.memory.cia1.int_mask & 0x01 != 0 && cpu.memory.cia1.timer_a.running));

            // NMI ready: check if the SOFTWARE vector changed ($0318),
            // OR if the HARDWARE vector ($FFFA) was changed by the tune's
            // code. Read $FFFA through banking (kernal_rom) to avoid false
            // positives from tune data loaded over the KERNAL area.
            let nmi_hw_vec = cpu.memory.banked_read(0xFFFA) as u16
                | ((cpu.memory.banked_read(0xFFFB) as u16) << 8);
            let nmi_installed = nmi_vec != 0xFE72 || nmi_hw_vec != 0xFE43;

            let nmi_ready =
                nmi_installed && cpu.memory.cia2.int_mask != 0 && cpu.memory.cia2.timer_a.running;

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

/// Deliver an IRQ to the CPU (manual, since mos6502 crate has no IRQ support).
///
/// The real 6502 IRQ sequence:
///   1. Push PC high byte
///   2. Push PC low byte
///   3. Push status register (with B flag clear)
///   4. Set I flag (disable further IRQs)
///   5. Load PC from $FFFE/$FFFF
/// Deliver an IRQ to the CPU. Returns the number of cycles consumed (7)
/// or 0 if interrupts are disabled and IRQ was not taken.
fn deliver_irq(cpu: &mut CPU<C64Memory, Nmos6502>) -> u32 {
    // Check if interrupts are disabled
    if cpu.registers.status.contains(Status::PS_DISABLE_INTERRUPTS) {
        return 0;
    }

    let pc = cpu.registers.program_counter;
    let mut sp = cpu.registers.stack_pointer.0;

    // Push PC high byte
    cpu.memory.ram[0x0100 | sp as usize] = (pc >> 8) as u8;
    sp = sp.wrapping_sub(1);

    // Push PC low byte
    cpu.memory.ram[0x0100 | sp as usize] = (pc & 0xFF) as u8;
    sp = sp.wrapping_sub(1);

    // Push status (B flag clear, unused bit set)
    let status_byte = (cpu.registers.status.bits() | 0x20) & !0x10;
    cpu.memory.ram[0x0100 | sp as usize] = status_byte;
    sp = sp.wrapping_sub(1);

    cpu.registers.stack_pointer = StackPointer(sp);

    // Set interrupt disable flag
    cpu.registers.status.insert(Status::PS_DISABLE_INTERRUPTS);

    // Jump to IRQ vector at $FFFE/$FFFF (reads through banking layer —
    // on real C64, vector fetch always sees KERNAL ROM when HIRAM=1)
    let lo = cpu.memory.banked_read(0xFFFE) as u16;
    let hi = cpu.memory.banked_read(0xFFFF) as u16;
    cpu.registers.program_counter = (hi << 8) | lo;

    7 // 6502 IRQ sequence takes 7 cycles
}

/// Deliver an NMI (non-maskable interrupt).
/// Same as IRQ but: cannot be masked, uses vector $FFFA/$FFFB.
/// Deliver an NMI (non-maskable interrupt). Returns 7 (cycles consumed).
fn deliver_nmi(cpu: &mut CPU<C64Memory, Nmos6502>) -> u32 {
    let pc = cpu.registers.program_counter;
    let mut sp = cpu.registers.stack_pointer.0;

    cpu.memory.ram[0x0100 | sp as usize] = (pc >> 8) as u8;
    sp = sp.wrapping_sub(1);
    cpu.memory.ram[0x0100 | sp as usize] = (pc & 0xFF) as u8;
    sp = sp.wrapping_sub(1);
    let status_byte = (cpu.registers.status.bits() | 0x20) & !0x10;
    cpu.memory.ram[0x0100 | sp as usize] = status_byte;
    sp = sp.wrapping_sub(1);

    cpu.registers.stack_pointer = StackPointer(sp);
    cpu.registers.status.insert(Status::PS_DISABLE_INTERRUPTS);

    let lo = cpu.memory.banked_read(0xFFFA) as u16;
    let hi = cpu.memory.banked_read(0xFFFB) as u16;
    cpu.registers.program_counter = (hi << 8) | lo;

    7 // 6502 NMI sequence takes 7 cycles
}

/// Run RSID emulation for `cycles` cycles, ticking CIA/VIC and delivering interrupts.
/// `prev_nmi` is passed by reference to maintain edge-detection state across sub-frames.
fn run_rsid_sub(cpu: &mut CPU<C64Memory, Nmos6502>, cycles: u32, prev_nmi: &mut bool) {
    let mut cycles_done: u32 = 0;

    while cycles_done < cycles {
        let inst_cycles = opcode_cycles_banked(&cpu.memory, cpu.registers.program_counter);
        cpu.single_step();
        cycles_done += inst_cycles;
        cpu.memory.frame_cycle += inst_cycles;

        // Tick all interrupt sources
        cpu.memory.cia1.tick(inst_cycles);
        cpu.memory.cia2.tick(inst_cycles);
        cpu.memory.vic.tick(inst_cycles);

        // Account for VIC badline stolen cycles (CPU stalled ~40 cycles)
        let stolen = cpu.memory.vic.stolen_cycles;
        if stolen > 0 {
            cycles_done += stolen;
            cpu.memory.frame_cycle += stolen;
            cpu.memory.cia1.tick(stolen);
            cpu.memory.cia2.tick(stolen);
        }

        // Jiffy clock: increment $00A2-$00A0 on each VIC frame boundary.
        // Many tunes poll this counter for timing even without interrupts.
        if cpu.memory.vic.new_frame {
            cpu.memory.vic.new_frame = false;
            cpu.memory.tick_jiffy_clock();
        }

        // IRQ (level-triggered)
        if cpu.memory.cia1.int_pending() || cpu.memory.vic.irq_line {
            let irq_cycles = deliver_irq(cpu);
            if irq_cycles > 0 {
                cycles_done += irq_cycles;
                cpu.memory.frame_cycle += irq_cycles;
                cpu.memory.cia1.tick(irq_cycles);
                cpu.memory.cia2.tick(irq_cycles);
                cpu.memory.vic.tick(irq_cycles);
            }
        }

        // NMI (edge-triggered)
        let cur_nmi = cpu.memory.cia2.int_pending();
        if cur_nmi && !*prev_nmi {
            let nmi_cycles = deliver_nmi(cpu);
            cycles_done += nmi_cycles;
            cpu.memory.frame_cycle += nmi_cycles;
            cpu.memory.cia1.tick(nmi_cycles);
            cpu.memory.cia2.tick(nmi_cycles);
            cpu.memory.vic.tick(nmi_cycles);
        }
        *prev_nmi = cur_nmi;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Player thread
// ─────────────────────────────────────────────────────────────────────────────

pub fn spawn_player() -> (Sender<PlayerCmd>, Receiver<PlayerStatus>) {
    let (cmd_tx, cmd_rx) = bounded::<PlayerCmd>(64);
    let (status_tx, status_rx) = bounded::<PlayerStatus>(16);

    thread::Builder::new()
        .name("sid-player".into())
        .spawn(move || {
            player_loop(cmd_rx, status_tx);
        })
        .expect("Failed to spawn player thread");

    (cmd_tx, status_rx)
}

fn player_loop(cmd_rx: Receiver<PlayerCmd>, status_tx: Sender<PlayerStatus>) {
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
                        if ctx.is_rsid {
                            // ── RSID ─────────────────────────────────────────
                            ctx.cpu.memory.clear_writes();
                            run_rsid_sub(&mut ctx.cpu, ctx.cycles_per_frame, &mut ctx.prev_nmi);

                            if let Some(ref mut br) = bridge {
                                send_sid_writes(
                                    br.as_mut(),
                                    &ctx.cpu.memory.sid_writes,
                                    ctx.mirror_mono,
                                );
                            }
                        } else {
                            // ── PSID ─────────────────────────────────────────
                            ctx.cpu.memory.clear_writes();
                            ctx.cpu.registers.program_counter = ctx.trampoline;
                            ctx.cpu.registers.stack_pointer = StackPointer(0xFD);
                            run_until(&mut ctx.cpu, ctx.halt_pc, 200_000);

                            if let Some(ref mut br) = bridge {
                                send_sid_writes(
                                    br.as_mut(),
                                    &ctx.cpu.memory.sid_writes,
                                    ctx.mirror_mono,
                                );
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

fn ensure_hardware(bridge: &mut Option<Box<dyn SidDevice>>) -> Result<(), String> {
    if bridge.is_some() {
        return Ok(());
    }
    let mut br = create_device()?;
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
            c.cpu.memory.voice_levels(),
            c.cpu.memory.sid_writes.len(),
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

            if let Err(e) = ensure_hardware(bridge) {
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
                let is_rsid = ctx.is_rsid;
                let sid4 = 0;
                stop_playback(play_ctx, bridge);

                if let Ok(data) = std::fs::read(&path) {
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

struct PlayContext {
    cpu: CPU<C64Memory, Nmos6502>,
    trampoline: u16,
    halt_pc: u16,
    frame_us: u64,
    cycles_per_frame: u32,
    elapsed: Duration,
    mirror_mono: bool,
    is_rsid: bool,
    prev_nmi: bool, // NMI edge state (persists across sub-frames)
    track_info: TrackInfo,
    frame_count: u32,
    next_frame: Instant, // absolute deadline for next frame
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

    // ── Build C64 memory + CPU ───────────────────────────────────────────
    let mut mem = C64Memory::new(header.is_pal, mapper, mono_mode);
    mem.load(sid_file.load_address, &sid_file.payload);

    // Rebuild kernal_rom overlay: copies tune data at $E000+ into the
    // ROM overlay so it's visible through banking, then re-installs our
    // KERNAL stubs at the critical entry points ($FF48, $EA31, etc.)
    mem.rebuild_kernal_rom();

    let load_end = sid_file.load_address as u32 + sid_file.payload.len() as u32;
    if load_end > 0xE000 {
        eprintln!(
            "[phosphor] Tune loads into KERNAL area: ${:04X}-${:04X} \
             (kernal_rom rebuilt)",
            sid_file.load_address,
            load_end.min(0xFFFF),
        );
    }

    let trampoline: u16 = 0x0300;
    let halt_pc = trampoline + 3;

    // NMI vector → KERNAL NMI entry ($FE43)
    mem.set_hw_vector(0xFFFA, 0xFE43);

    if is_rsid {
        // For RSID: IRQ vector points to the KERNAL IRQ entry ($FF48)
        // which saves A/X/Y, then jumps through the software vector
        // at $0314/$0315. The tune's INIT routine typically changes
        // $0314/$0315 to point to its own IRQ handler.
        mem.set_hw_vector(0xFFFE, 0xFF48);

        // Pre-initialize CIA1 to RSID defaults per SID spec:
        // Timer A at 60Hz (PAL: 0x4025, NTSC: 0x4295), running, IRQ enabled.
        mem.cia1.setup_rsid_defaults(header.is_pal);
    } else {
        // For PSID: IRQ vector → halt (not used, but safe)
        mem.set_hw_vector(0xFFFE, halt_pc);
    }

    // ── Run INIT routine ─────────────────────────────────────────────────
    mem.install_trampoline(trampoline, header.init_address);

    // For RSID, install a CLI idle loop after the JSR:
    //   $0303: CLI          (58)
    //   $0304: JMP $0304    (4C 04 03)
    // This ensures interrupts are enabled after INIT returns.
    let idle_pc: u16 = if is_rsid {
        mem.ram[0x0303] = 0x58; // CLI
        mem.ram[0x0304] = 0x4C; // JMP $0304
        mem.ram[0x0305] = 0x04;
        mem.ram[0x0306] = 0x03;
        0x0304 // detect INIT return when CPU reaches this JMP
    } else {
        halt_pc // PSID: use the default JMP halt from install_trampoline
    };

    let mut cpu = CPU::new(mem, Nmos6502);
    cpu.registers.program_counter = trampoline;
    cpu.registers.stack_pointer = StackPointer(0xFD);
    cpu.registers.accumulator = song.saturating_sub(1) as u8;

    // Run INIT.
    // RSID: full hardware emulation (CIA/VIC ticking + interrupts) because
    // tunes may poll timers or need interrupts during initialization.
    // Budget: 30M cycles (~30 seconds C64 time, <1s real time).
    // Early exit once the tune installs interrupt handlers + enables sources.
    // PSID: simple step execution, no interrupt delivery needed.
    let (init_returned, init_prev_nmi) = if is_rsid {
        run_rsid_init(&mut cpu, idle_pc, 30_000_000)
    } else {
        run_until(&mut cpu, halt_pc, 2_000_000);
        (cpu.registers.program_counter == halt_pc, false)
    };

    // Send INIT's SID writes to hardware
    let init_writes = cpu.memory.sid_writes.len();
    if let Some(ref mut br) = bridge {
        for &(_cycle, reg, val) in &cpu.memory.sid_writes {
            br.write(reg, val);
        }
        eprintln!("[phosphor] INIT done, {init_writes} SID writes sent");
    }
    cpu.memory.clear_writes();

    if is_rsid {
        // Clean up stale CIA interrupt flags. During INIT, our RSID defaults
        // had CIA1 timer running. If the tune stopped the timer but didn't
        // read $DC0D, a pending underflow flag would cause an IRQ flood
        // (e.g., Chimera by Rob Hubbard: stops CIA1, uses VIC raster only,
        // but stale CIA1 int_data causes deliver_irq on every instruction).
        cpu.memory.cia1.clear_stale_ints();
        cpu.memory.cia2.clear_stale_ints();

        eprintln!(
            "[phosphor] RSID: load=${:04X} init=${:04X} play=${:04X}",
            sid_file.load_address, header.init_address, header.play_address,
        );
        if init_returned {
            // INIT returned normally — CPU is in CLI; JMP $0304 idle loop.
            eprintln!("[phosphor] RSID: INIT returned, idle loop active");
        } else {
            // INIT didn't return — non-returning INIT (e.g. MOD player with
            // decompression). The frame loop will continue execution naturally.
            // Decompression takes several real-time seconds at 50 frames/sec,
            // then the tune sets up interrupts and playback begins.
            cpu.registers.status.remove(Status::PS_DISABLE_INTERRUPTS);
            eprintln!(
                "[phosphor] RSID: INIT did not return (non-returning), PC=${:04X}",
                cpu.registers.program_counter,
            );
        }

        eprintln!(
            "[phosphor] RSID: CIA1 timer_a={}, mask={:#04x}, running={}",
            cpu.memory.cia1.timer_a.latch,
            cpu.memory.cia1.int_mask,
            cpu.memory.cia1.timer_a.running,
        );
        eprintln!(
            "[phosphor] RSID: VIC raster_compare={}, raster_irq={}",
            cpu.memory.vic.raster_compare, cpu.memory.vic.raster_irq_enabled,
        );
        eprintln!(
            "[phosphor] RSID: CIA2 timer_a={}, mask={:#04x}, running={}",
            cpu.memory.cia2.timer_a.latch,
            cpu.memory.cia2.int_mask,
            cpu.memory.cia2.timer_a.running,
        );
        eprintln!(
            "[phosphor] RSID: IRQ vector $0314=${:04X}, NMI vector $0318=${:04X}",
            cpu.memory.ram[0x0314] as u16 | ((cpu.memory.ram[0x0315] as u16) << 8),
            cpu.memory.ram[0x0318] as u16 | ((cpu.memory.ram[0x0319] as u16) << 8),
        );
    } else if header.play_address != 0 {
        // PSID: install play trampoline
        cpu.memory
            .install_trampoline(trampoline, header.play_address);
    }

    PlayContext {
        cpu,
        trampoline,
        halt_pc,
        frame_us,
        cycles_per_frame,
        elapsed: Duration::ZERO,
        mirror_mono,
        is_rsid,
        prev_nmi: init_prev_nmi,
        track_info,
        frame_count: 0,
        next_frame: Instant::now(),
    }
}

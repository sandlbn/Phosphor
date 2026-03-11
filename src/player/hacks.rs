// Tune-specific patches for known timing-critical RSID files.
//
// Ported from WebSid/compute! hacks.c + cpu_operations.inc by Jürgen Wothke.
//
// cpuHackNMI(on) semantics (from cpu_operations.inc):
//   _no_nmi_hack = !on
//   CHECK_FOR_NMI: fires when ciaNMI() && (_no_nmi_hack || _no_flag_i)
//
//   cpuHackNMI(0) → _no_nmi_hack=1 → NMI fires freely (normal behaviour)
//   cpuHackNMI(1) → _no_nmi_hack=0 → NMI only fires when I-flag is CLEAR
//
// The reference calls cpuHackNMI(0) at the top of hackIfNeeded() on every
// song load to reset the default, then individual patches call cpuHackNMI(1)
// for songs where the NMI-must-wait-for-I-flag guard is needed.

use super::rsid_bus::RsidBus;

// ─────────────────────────────────────────────────────────────────────────────
//  Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Apply any needed hack to `bus` for the tune with the given `init_addr`.
/// Call this after the tune payload has been loaded into RAM but before INIT.
///
/// Returns `HackFlags` that must be honoured by both emulation loops.
pub fn apply_hacks(bus: &mut RsidBus, init_addr: u16) -> HackFlags {
    // Always reset to defaults first (mirrors cpuHackNMI(0) at top of hackIfNeeded)
    let mut flags = HackFlags::default();

    patch_mr_meaner_if_needed(bus, init_addr, &mut flags);
    patch_feeling_good_if_needed(bus, init_addr, &mut flags);
    patch_4non_blondes_if_needed(bus, init_addr, &mut flags);
    patch_game_player_if_needed(bus, init_addr, &mut flags);
    patch_synthmeld_if_needed(bus, init_addr, &mut flags);
    patch_immigrant_song_if_needed(bus, init_addr, &mut flags);
    patch_utopia6_if_needed(bus, init_addr, &mut flags);
    patch_swallow_if_needed(bus, init_addr, &mut flags);
    patch_we_are_demo_if_needed(bus, init_addr, &mut flags);
    patch_graphixsmania_if_needed(bus, init_addr, &mut flags);

    flags
}

/// Flags set by the hack system that the emulation loops must honour.
///
/// `nmi_needs_i_flag_clear`:
///   Maps to `_no_nmi_hack = 0` (cpuHackNMI(1)) in the reference.
///   When true, NMI delivery is gated: only fire if the CPU I-flag is
///   currently CLEAR.  This prevents spurious NMI re-triggers on songs
///   whose CIA NMI line is still asserted when an RTI restores the status
///   register (which briefly clears I before setting it back).
///
/// `disable_badline_stun`:
///   When true, skip the BA stun loop entirely for this tune.
#[derive(Default, Debug, Clone)]
pub struct HackFlags {
    /// NMI may only fire when the CPU I-flag is clear.
    /// Mirrors: `_no_nmi_hack = 0` (cpuHackNMI(1)).
    pub nmi_needs_i_flag_clear: bool,

    /// Completely disable VIC badline stun (songs whose timing breaks
    /// when the emulator's badline approximation is active).
    pub disable_badline_stun: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helper
// ─────────────────────────────────────────────────────────────────────────────

fn mem_match(ram: &[u8], addr: usize, pattern: &[u8]) -> bool {
    let end = addr + pattern.len();
    if end > ram.len() {
        return false;
    }
    ram[addr..end] == *pattern
}

// ─────────────────────────────────────────────────────────────────────────────
//  Individual patches
// ─────────────────────────────────────────────────────────────────────────────

/// Mr_Meaner.sid — NMI/IRQ handling issue.
/// Disable badline stun; the INT-flag gets stuck otherwise.
fn patch_mr_meaner_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let pattern: &[u8] = &[0xad, 0x0e, 0xdd, 0xad, 0xd9, 0x16, 0x8d, 0xd6, 0x16];
    if init_addr == 0x1000 && mem_match(&bus.c64.ram.ram, 0x157c, pattern) {
        flags.disable_badline_stun = true;
        eprintln!("[hacks] Mr_Meaner: disabling badline stun");
    }
}

/// Feeling_Good.sid — NMI fires during RTI when it shouldn't.
/// cpuHackNMI(1): NMI only fires when I-flag is clear.
fn patch_feeling_good_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let pattern: &[u8] = &[0xee, 0xe5, 0x1c, 0xee, 0x1a, 0x1d];
    if init_addr == 0x1000 && mem_match(&bus.c64.ram.ram, 0x1cf7, pattern) {
        flags.nmi_needs_i_flag_clear = true;
        eprintln!("[hacks] Feeling_Good: NMI gated on I-flag clear");
    }
}

/// 4_Non_Blondes-Whats_Up_Remix.sid — VIC badline timing issue.
/// Disable the display (clear DEN bit) to prevent badlines.
fn patch_4non_blondes_if_needed(bus: &mut RsidBus, init_addr: u16, _flags: &mut HackFlags) {
    let pattern: &[u8] = &[0x84, 0xFD, 0xA0, 0x00, 0xB1, 0xFA];
    if init_addr == 0x082E && mem_match(&bus.c64.ram.ram, 0x0903, pattern) {
        bus.c64.ram.ram[0x0835] = 0x0b;
        eprintln!("[hacks] 4_Non_Blondes: disabling display (D011 patch at $0835)");
    }
}

/// Game_Player.sid — NMI fires during RTI.
/// cpuHackNMI(1): NMI only fires when I-flag is clear.
fn patch_game_player_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let pattern: &[u8] = &[0x8D, 0xD8, 0x0B, 0xAD, 0xAE, 0x21];
    if init_addr == 0x0810 && mem_match(&bus.c64.ram.ram, 0x0B85, pattern) {
        flags.nmi_needs_i_flag_clear = true;
        eprintln!("[hacks] Game_Player: NMI gated on I-flag clear");
    }
}

/// Synthmeld.sid — timer-B chained IRQ fires at wrong moment.
fn patch_synthmeld_if_needed(bus: &mut RsidBus, init_addr: u16, _flags: &mut HackFlags) {
    let pattern: &[u8] = &[
        0xEE, 0x9C, 0xE8, 0xA9, 0x07, 0x10, 0x3A, 0xC9, 0xE0, 0x90, 0x1F,
    ];
    if init_addr == 0x0b00 && mem_match(&bus.c64.ram.ram, 0xB398, pattern) {
        bus.c64.ram.ram[0xB39C] = 0x00;
        bus.c64.ram.ram[0xB398] = 0xea;
        bus.c64.ram.ram[0xB399] = 0xea;
        bus.c64.ram.ram[0xB39a] = 0xea;
        eprintln!("[hacks] Synthmeld: patching timer counter loop");
    }
}

/// Immigrant_Song.sid — absolute unforgiving NMI/badline timing.
/// Disable the display (clear DEN bit) to eliminate badlines.
fn patch_immigrant_song_if_needed(bus: &mut RsidBus, init_addr: u16, _flags: &mut HackFlags) {
    let pattern: &[u8] = &[0xd1, 0x0b, 0x20, 0xcc, 0x0c, 0x20, 0x39];
    if init_addr == 0x080d && mem_match(&bus.c64.ram.ram, 0x0826, pattern) {
        bus.c64.ram.ram[0x0821] = 0x0b;
        eprintln!("[hacks] Immigrant_Song: disabling display (patch at $0821)");
    }
}

/// Utopia_tune_6.sid — sprite-DMA bad-cycles cause timing drift.
/// Disable badline stun; also disable IRQ ACK to let handler re-fire.
fn patch_utopia6_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let pattern: &[u8] = &[0xce, 0x16, 0xd0, 0xee, 0x16, 0xd0];
    if init_addr == 0x9200 && mem_match(&bus.c64.ram.ram, 0x8b05, pattern) {
        flags.disable_badline_stun = true;
        // Disable IRQ ACKN — causes IRQ handler to be immediately called again.
        bus.c64.ram.ram[0x8E49] = 0x00;
        eprintln!("[hacks] Utopia_tune_6: disabling badline stun + IRQ ACKN patch");
    }
}

/// Comaland_tune_3.sid & Fantasmolytic_tune_2.sid ("Swallow" tunes).
fn patch_swallow_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let p1: &[u8] = &[0x8E, 0x16, 0xD0, 0xA5, 0xE0, 0x69, 0x29];
    let p2: &[u8] = &[0x8E, 0x16, 0xD0, 0xA5, 0xC1, 0x69, 0x29];
    if init_addr == 0x2000
        && (mem_match(&bus.c64.ram.ram, 0x28C8, p1) || mem_match(&bus.c64.ram.ram, 0x28C8, p2))
    {
        flags.disable_badline_stun = true;
        eprintln!("[hacks] Swallow (Comaland/Fantasmolytic): disabling badline stun");
    }
}

/// We_Are_Demo_tune_2.sid — sprite-related bad-cycles.
fn patch_we_are_demo_if_needed(bus: &mut RsidBus, init_addr: u16, flags: &mut HackFlags) {
    let pattern: &[u8] = &[0x8E, 0x18, 0xD4, 0x79, 0x00, 0x09, 0x85, 0xE1];
    if init_addr == 0x0c60 && mem_match(&bus.c64.ram.ram, 0x0B10, pattern) {
        flags.disable_badline_stun = true;
        eprintln!("[hacks] We_Are_Demo: disabling badline stun");
    }
}

/// Graphixmania_2_part_6.sid — unnecessary D418 write in IRQ player.
fn patch_graphixsmania_if_needed(bus: &mut RsidBus, init_addr: u16, _flags: &mut HackFlags) {
    let pattern: &[u8] = &[0xB8, 0x29, 0x0F, 0x8D, 0x18, 0xD4];
    if init_addr == 0x7000 && mem_match(&bus.c64.ram.ram, 0x1214, pattern) {
        bus.c64.ram.ram[0x48F9] = 0xad;
        eprintln!("[hacks] Graphixsmania_2_part_6: patching D418 write");
    }
}

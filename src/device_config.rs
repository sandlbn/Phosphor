//! Shared helper that runs a [`crate::player::DeviceConfigCmd`] against an
//! arbitrary `usbsid_pico_config::Transport`. Both `DirectDevice` and
//! `BridgeDevice` invoke this from their `SidDevice::run_device_config`
//! impls so we don't duplicate the operation matching logic.

use crate::player::{DeviceConfigCmd, DeviceConfigEdit};
use crate::ui::DeviceConfigSnapshot;
use usbsid_pico_config::protocol::{cfg as cfg_op, encode_packet};
use usbsid_pico_config::transport::Transport;
use usbsid_pico_config::Device;

/// Send a single fire-and-forget config opcode (no response expected).
/// Used for the diagnostic / hardware action commands the crate doesn't
/// expose as named wrappers (TEST_SID, RESET_USBSID, MIDI state, etc.).
fn send_cfg_opcode<T: Transport>(
    dev: &mut Device<T>,
    cmd: u8,
    args: [u8; 4],
) -> Result<(), String> {
    let packet = encode_packet(cmd, args);
    dev.transport_mut()
        .send(&packet)
        .map_err(|e| format!("send opcode {cmd:#04X}: {e}"))
}

/// Execute `op` against the device wrapped by `transport`. Returns
/// `Some(snapshot)` only for `Refresh` (and as an after-effect of
/// auto-detect, since callers will want the resulting config). For
/// action-only ops the result is `None`.
pub fn run<T: Transport>(
    transport: T,
    op: &DeviceConfigCmd,
) -> Result<Option<DeviceConfigSnapshot>, String> {
    let mut dev = Device::new(transport);
    match op {
        DeviceConfigCmd::Refresh => refresh(&mut dev).map(Some),

        DeviceConfigCmd::ApplyPreset(p) => {
            dev.apply_preset(*p)
                .map_err(|e| format!("apply_preset: {e}"))?;
            // Re-read so the GUI immediately reflects the new socket
            // layout the preset just installed.
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::SetClock(rate) => {
            // Read → mutate → write → apply. The crate doesn't expose a
            // single-field clock-write yet; round-trip via the full config
            // is fine for an occasional user click.
            let mut cfg = dev
                .read_config_lenient()
                .map_err(|e| format!("read_config: {e}"))?;
            cfg.clock_rate = *rate;
            dev.write_config(&cfg)
                .map_err(|e| format!("write_config: {e}"))?;
            dev.apply().map_err(|e| format!("apply: {e}"))?;
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::Edit(edit) => {
            let mut cfg = dev
                .read_config_lenient()
                .map_err(|e| format!("read_config: {e}"))?;
            apply_edit(&mut cfg, *edit);
            dev.write_config(&cfg)
                .map_err(|e| format!("write_config: {e}"))?;
            dev.apply().map_err(|e| format!("apply: {e}"))?;
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::Save => {
            dev.save_no_reset()
                .map_err(|e| format!("save_no_reset: {e}"))?;
            Ok(None)
        }

        DeviceConfigCmd::Reset => {
            dev.reset_to_defaults()
                .map_err(|e| format!("reset_to_defaults: {e}"))?;
            // Give the firmware a moment to settle before the GUI re-reads.
            std::thread::sleep(std::time::Duration::from_millis(500));
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::AutoDetect => {
            dev.auto_detect().map_err(|e| format!("auto_detect: {e}"))?;
            // auto_detect blocks the firmware for ~3 s; wait for it then read.
            std::thread::sleep(std::time::Duration::from_millis(3500));
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::Confirm => {
            dev.confirm_config()
                .map_err(|e| format!("confirm_config: {e}"))?;
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::DetectSids => {
            dev.detect_sids().map_err(|e| format!("detect_sids: {e}"))?;
            std::thread::sleep(std::time::Duration::from_millis(1500));
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::DetectClones => {
            dev.detect_clones()
                .map_err(|e| format!("detect_clones: {e}"))?;
            std::thread::sleep(std::time::Duration::from_millis(2500));
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::TestSid(which) => {
            let cmd = match *which {
                0 => cfg_op::TEST_ALLSIDS,
                1 => cfg_op::TEST_SID1,
                2 => cfg_op::TEST_SID2,
                3 => cfg_op::TEST_SID3,
                4 => cfg_op::TEST_SID4,
                other => return Err(format!("TestSid: invalid SID index {other}")),
            };
            send_cfg_opcode(&mut dev, cmd, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::StopTests => {
            send_cfg_opcode(&mut dev, cfg_op::STOP_TESTS, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::ResetUsbsid => {
            send_cfg_opcode(&mut dev, cfg_op::RESET_USBSID, [0, 0, 0, 0])?;
            // Device re-enumerates — drop the handle; the GUI will reconnect.
            Ok(None)
        }

        DeviceConfigCmd::RestartBus => {
            send_cfg_opcode(&mut dev, cfg_op::RESTART_BUS, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::RestartBusClk => {
            send_cfg_opcode(&mut dev, cfg_op::RESTART_BUS_CLK, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::SyncPios => {
            send_cfg_opcode(&mut dev, cfg_op::SYNC_PIOS, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::SocketDetect => {
            send_cfg_opcode(&mut dev, cfg_op::SOCKET_DETECT, [0, 0, 0, 0])?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            refresh(&mut dev).map(Some)
        }

        DeviceConfigCmd::MidiLoadState => {
            send_cfg_opcode(&mut dev, cfg_op::LOAD_MIDI_STATE, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::MidiSaveState => {
            send_cfg_opcode(&mut dev, cfg_op::SAVE_MIDI_STATE, [0, 0, 0, 0])?;
            Ok(None)
        }

        DeviceConfigCmd::MidiResetState => {
            send_cfg_opcode(&mut dev, cfg_op::RESET_MIDI_STATE, [0, 0, 0, 0])?;
            Ok(None)
        }
    }
}

fn apply_edit(cfg: &mut usbsid_pico_config::DeviceConfig, edit: DeviceConfigEdit) {
    use DeviceConfigEdit::*;
    fn socket(
        cfg: &mut usbsid_pico_config::DeviceConfig,
        n: u8,
    ) -> &mut usbsid_pico_config::SocketConfig {
        if n == 1 {
            &mut cfg.socket1
        } else {
            &mut cfg.socket2
        }
    }
    match edit {
        SocketEnabled(n, v) => socket(cfg, n).enabled = v,
        SocketDualSid(n, v) => socket(cfg, n).dualsid = v,
        SocketChipType(n, t) => socket(cfg, n).chip_type = t,
        SocketSidType(n, slot, t) => {
            let s = socket(cfg, n);
            if slot == 1 {
                s.sid1.kind = t
            } else {
                s.sid2.kind = t
            }
        }
        StereoEnabled(v) => cfg.stereo_enabled = v,
        LockAudioSwitch(v) => cfg.lock_audio_switch = v,
        Mirrored(v) => cfg.mirrored = v,
        Flipped(v) => cfg.flipped = v,
        Mixed(v) => cfg.mixed = v,
        FmoplEnabled(v) => cfg.protocols.fmopl_enabled = v,
        FmoplSidno(v) => cfg.protocols.fmopl_sidno = v,
        LockClockrate(v) => cfg.lock_clockrate = v,
        ExternalClock(v) => cfg.external_clock = v,
        LedEnabled(v) => cfg.led.enabled = v,
        LedIdleBreathe(v) => cfg.led.idle_breathe = v,
        RgbLedEnabled(v) => cfg.rgb_led.enabled = v,
        RgbLedIdleBreathe(v) => cfg.rgb_led.idle_breathe = v,
        RgbLedBrightness(v) => cfg.rgb_led.brightness = v,
        RgbLedSidToUse(v) => cfg.rgb_led.sid_to_use = v,
        NeedConfirmation(v) => cfg.need_confirmation = v,
        DisableChangeDetect(v) => cfg.disable_changedetect = v,
    }
}

fn refresh<T: Transport>(dev: &mut Device<T>) -> Result<DeviceConfigSnapshot, String> {
    let firmware_version = dev
        .read_firmware_version()
        .map_err(|e| format!("read_firmware_version: {e}"))?;
    let pcb_version = dev
        .read_pcb_version()
        .map_err(|e| format!("read_pcb_version: {e}"))?;
    let config = dev
        .read_config_lenient()
        .map_err(|e| format!("read_config: {e}"))?;
    Ok(DeviceConfigSnapshot {
        firmware_version,
        pcb_version,
        config,
    })
}

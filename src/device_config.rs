//! Shared helper that runs a [`crate::player::DeviceConfigCmd`] against an
//! arbitrary `usbsid_pico_config::Transport`. Both `DirectDevice` and
//! `BridgeDevice` invoke this from their `SidDevice::run_device_config`
//! impls so we don't duplicate the operation matching logic.

use crate::player::{DeviceConfigCmd, DeviceConfigEdit};
use crate::ui::DeviceConfigSnapshot;
use usbsid_pico_config::transport::Transport;
use usbsid_pico_config::Device;

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
        LockClockrate(v) => cfg.lock_clockrate = v,
        ExternalClock(v) => cfg.external_clock = v,
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

//! Memory bank abstractions.
//!
//! In the C64, the PLA routes CPU accesses to different chips depending on
//! the processor-port bits and the address.  Each "bank" is a device that
//! can be read (`peek`) and written (`poke`).

pub mod bank;
pub mod color_ram;
pub mod disconnected_bus;
pub mod extra_sid;
pub mod io_bank;
pub mod sid_bank;
pub mod system_ram;
pub mod system_rom;
pub mod zero_ram;

pub use bank::Bank;
pub use color_ram::ColorRamBank;
pub use disconnected_bus::DisconnectedBusBank;
pub use extra_sid::ExtraSidBank;
pub use io_bank::IoBank;
pub use sid_bank::SidBank;
pub use system_ram::SystemRamBank;
pub use system_rom::{BasicRomBank, CharacterRomBank, KernalRomBank};
pub use zero_ram::ZeroRamBank;

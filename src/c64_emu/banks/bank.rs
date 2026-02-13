//! The core `Bank` trait — read/write interface for every memory-mapped device.

/// Every memory-mapped device implements this trait.
pub trait Bank {
    /// Write `value` to `address`.
    fn poke(&mut self, address: u16, value: u8);

    /// Read the byte at `address`.
    fn peek(&self, address: u16) -> u8;

    /// Mutable peek (some banks need `&mut self` for side-effects on read,
    /// e.g. CIA interrupt-acknowledge).  Default delegates to `peek`.
    fn peek_mut(&mut self, address: u16) -> u8 {
        self.peek(address)
    }
}

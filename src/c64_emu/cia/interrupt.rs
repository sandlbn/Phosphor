//! CIA interrupt control logic.
//!
//! Old CIA (MOS 6526): interrupts are delayed by 1 cycle.
//! New CIA (MOS 8521): interrupts fire immediately.

use super::INT_REQUEST;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiaModel {
    Mos6526,
    Mos8521,
    Mos6526W4485,
}

pub struct InterruptSource {
    model: CiaModel,
    /// Interrupt Control Register (mask).
    icr: u8,
    /// Interrupt Data Register (pending flags).
    idr: u8,
    /// Is the IRQ pin currently asserted?
    pub asserted: bool,
    /// Delayed trigger (for old CIA model).
    pending_trigger: Option<u8>,
}

impl InterruptSource {
    pub fn new(model: CiaModel) -> Self {
        Self {
            model,
            icr: 0,
            idr: 0,
            asserted: false,
            pending_trigger: None,
        }
    }

    pub fn reset(&mut self) {
        self.icr = 0;
        self.idr = 0;
        self.asserted = false;
        self.pending_trigger = None;
    }

    /// Set or clear bits in the interrupt mask register.
    /// Bit 7 high → set bits; bit 7 low → clear bits.
    /// Returns `Some(true)` if this causes a new IRQ assertion.
    pub fn set_mask(&mut self, data: u8) -> Option<bool> {
        if data & INT_REQUEST != 0 {
            self.icr |= data & !INT_REQUEST;
        } else {
            self.icr &= !data;
        }
        // Check if any already-pending flag now matches the new mask.
        if self.idr & self.icr & 0x1F != 0 && !self.is_triggered() {
            self.idr |= INT_REQUEST;
            self.asserted = true;
            return Some(true);
        }
        None
    }

    /// Trigger an interrupt flag.  Returns `true` if the IRQ line
    /// should be asserted (i.e. flag is enabled in the mask).
    pub fn trigger(&mut self, flag: u8) -> bool {
        self.idr |= flag;

        if (self.idr & self.icr & 0x1F) != 0 {
            match self.model {
                CiaModel::Mos8521 => {
                    // immediate
                    if !self.is_triggered() {
                        self.idr |= INT_REQUEST;
                        self.asserted = true;
                        return true;
                    }
                }
                CiaModel::Mos6526 | CiaModel::Mos6526W4485 => {
                    // delayed by 1 cycle — we store the pending state
                    // and the caller must call `tick_delayed()` next cycle.
                    self.pending_trigger = Some(flag);
                    if !self.is_triggered() {
                        self.idr |= INT_REQUEST;
                        self.asserted = true;
                        return true;
                    }
                }
            }
        }
        false
    }

    /// For old CIA model: process any 1-cycle delayed interrupt.
    /// Returns `true` if IRQ should be asserted.
    pub fn tick_delayed(&mut self) -> bool {
        if let Some(_flag) = self.pending_trigger.take() {
            if (self.idr & self.icr & 0x1F) != 0 && !self.is_triggered() {
                self.idr |= INT_REQUEST;
                self.asserted = true;
                return true;
            }
        }
        false
    }

    /// Read and clear the IDR (acknowledge).
    pub fn clear(&mut self) -> u8 {
        let old = self.idr;
        self.idr = 0;
        self.asserted = false;
        old
    }

    /// Read the interrupt mask (ICR register).
    pub fn icr_mask(&self) -> u8 {
        self.icr
    }

    fn is_triggered(&self) -> bool {
        (self.idr & INT_REQUEST) != 0
    }
}

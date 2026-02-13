//! Minimal cycle-accurate event scheduler.
//!
//! The original libsidplayfp `EventScheduler` drives every chip via
//! callback events.  We keep the same concept but use a simpler
//! priority-queue approach (sorted `Vec`); performance is fine for
//! the small number of concurrent events in a C64 (~20 max).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

// ── Clock types ────────────────────────────────────────────────

/// Master-clock tick counter (signed so deltas can be negative).
pub type EventClock = i64;

/// Two-phase clock.  PHI1 is the first half-cycle, PHI2 the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Phi1 = 0,
    Phi2 = 1,
}

// ── Event identifier ───────────────────────────────────────────

/// Every scheduled callback is wrapped in an `Event`.
/// We use a trait-object approach so any chip can register closures.
pub type EventId = u64;

/// Boxed callable — the thing that actually runs when the event fires.
pub type EventAction = Box<dyn FnMut(&mut EventContext)>;

// ── Scheduler entry ────────────────────────────────────────────

struct ScheduledEvent {
    fire_at: EventClock,
    id: EventId,
    /// `None` once cancelled / consumed.
    action: Option<EventAction>,
}

impl Eq for ScheduledEvent {}
impl PartialEq for ScheduledEvent {
    fn eq(&self, other: &Self) -> bool {
        self.fire_at == other.fire_at && self.id == other.id
    }
}
impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; we want earliest first → reverse.
        other
            .fire_at
            .cmp(&self.fire_at)
            .then_with(|| other.id.cmp(&self.id))
    }
}

// ── EventContext (the scheduler itself) ────────────────────────

pub struct EventContext {
    /// Monotonic master clock (increments by 1 each half-cycle).
    clock: EventClock,
    phase: Phase,
    next_id: EventId,
    queue: BinaryHeap<ScheduledEvent>,
}

impl EventContext {
    pub fn new() -> Self {
        Self {
            clock: 0,
            phase: Phase::Phi1,
            next_id: 0,
            queue: BinaryHeap::new(),
        }
    }

    // ── Time queries ───────────────────────────────────────────

    /// Current half-cycle count on the given phase edge.
    pub fn get_time(&self, phase: Phase) -> EventClock {
        self.clock + (self.phase as EventClock) - (phase as EventClock)
    }

    /// Shorthand: PHI2 time (most CIA / VIC accesses happen here).
    pub fn phi2_time(&self) -> EventClock {
        self.get_time(Phase::Phi2)
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    // ── Scheduling ─────────────────────────────────────────────

    /// Schedule `action` to fire after `delay` half-cycles from now,
    /// aligned to `phase`.  Returns the event ID (for cancellation).
    pub fn schedule(&mut self, delay: EventClock, phase: Phase, action: EventAction) -> EventId {
        let id = self.next_id;
        self.next_id += 1;
        let fire_at = self.clock + delay + (phase as EventClock) - (self.phase as EventClock);
        self.queue.push(ScheduledEvent {
            fire_at,
            id,
            action: Some(action),
        });
        id
    }

    /// Cancel a previously scheduled event (best-effort; O(n)).
    pub fn cancel(&mut self, target_id: EventId) {
        // We can't efficiently remove from a BinaryHeap, so mark it dead.
        // It will be skipped when popped.
        // For a small queue this is fine.
        let mut temp: Vec<_> = self.queue.drain().collect();
        for e in &mut temp {
            if e.id == target_id {
                e.action = None;
            }
        }
        self.queue.extend(temp);
    }

    /// Is the event still pending?
    pub fn is_pending(&self, target_id: EventId) -> bool {
        self.queue
            .iter()
            .any(|e| e.id == target_id && e.action.is_some())
    }

    // ── Advance ────────────────────────────────────────────────

    /// Advance one half-cycle.  Fires all events whose time has come.
    ///
    /// Returns `true` if at least one event fired.
    pub fn clock_tick(&mut self) -> bool {
        self.clock += 1;
        self.phase = match self.phase {
            Phase::Phi1 => Phase::Phi2,
            Phase::Phi2 => Phase::Phi1,
        };

        let mut fired = false;
        loop {
            let should_fire = self
                .queue
                .peek()
                .map_or(false, |e| e.fire_at <= self.clock && e.action.is_some());
            if !should_fire {
                // Also drain dead (cancelled) entries at the top.
                let is_dead = self
                    .queue
                    .peek()
                    .map_or(false, |e| e.fire_at <= self.clock && e.action.is_none());
                if is_dead {
                    self.queue.pop();
                    continue;
                }
                break;
            }
            let mut entry = self.queue.pop().unwrap();
            if let Some(ref mut action) = entry.action {
                action(self);
                fired = true;
            }
        }
        fired
    }

    /// Reset the scheduler (new session).
    pub fn reset(&mut self) {
        self.clock = 0;
        self.phase = Phase::Phi1;
        self.queue.clear();
    }
}

impl Default for EventContext {
    fn default() -> Self {
        Self::new()
    }
}

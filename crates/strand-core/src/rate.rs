//! Feedback-loop guard: more than [`MAX_WRITES_PER_SEC`] writes per second
//! to one cell from one handler warns once and throttles.
//!
//! Writes are counted per tick, not per call: a handler that writes a cell
//! many times inside one tick coalesces to one write, so loops inside one
//! handler run never trip the guard. While throttled, the latest value is
//! held and applied when the one-second window has room again, so the final
//! value is never lost. Writes from outside a handler (services, CLI) are not
//! counted; they coalesce per tick anyway.

use std::collections::VecDeque;
use std::time::Duration;

use crate::runtime::{Diagnostic, NodeId, Runtime};

/// The threshold from the design: more than this many writes per second to
/// one cell from one handler is a feedback loop.
pub const MAX_WRITES_PER_SEC: usize = 30;

const WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct RateWindow {
    /// `(tick, time)` of each counted write in the last second.
    writes: VecDeque<(u64, Duration)>,
    /// The tick in which the last write was throttled.
    throttled_tick: Option<u64>,
    warned: bool,
}

pub(crate) struct Deferred {
    pub(crate) cell: NodeId,
    pub(crate) writer: NodeId,
    pub(crate) due: Duration,
    pub(crate) apply: Box<dyn FnOnce(&Runtime)>,
}

impl Runtime {
    /// Decide whether a write from the current handler to `cell` goes
    /// through now (`true`) or is deferred (`false`).
    pub(crate) fn rate_gate(&self, cell: NodeId) -> bool {
        let Some(writer) = self.inner.writer.get() else {
            return true;
        };
        let now = self.now();
        let tick = self.tick_seq();
        let mut map = self.inner.rate.borrow_mut();
        let w = map.entry((cell, writer)).or_default();
        while w.writes.front().is_some_and(|&(_, t)| t + WINDOW <= now) {
            w.writes.pop_front();
        }
        if w.writes.is_empty() {
            w.warned = false;
        }
        if w.throttled_tick == Some(tick) {
            return false;
        }
        if w.writes.back().is_some_and(|&(t, _)| t == tick) {
            return true;
        }
        if w.writes.len() >= MAX_WRITES_PER_SEC {
            w.throttled_tick = Some(tick);
            let warn = !w.warned;
            w.warned = true;
            drop(map);
            if warn {
                let names = self.path(vec![writer, cell]);
                self.diagnose(Diagnostic::WriteRate {
                    cell,
                    writer,
                    names,
                });
            }
            return false;
        }
        w.writes.push_back((tick, now));
        true
    }

    /// Hold a throttled write; a newer one from the same handler replaces it.
    pub(crate) fn defer_write(&self, cell: NodeId, apply: Box<dyn FnOnce(&Runtime)>) {
        let Some(writer) = self.inner.writer.get() else {
            apply(self);
            return;
        };
        let due = self
            .inner
            .rate
            .borrow()
            .get(&(cell, writer))
            .and_then(|w| w.writes.front().map(|&(_, t)| t + WINDOW))
            .unwrap_or(self.now());
        let mut throttled = self.inner.throttled.borrow_mut();
        throttled.retain(|d| !(d.cell == cell && d.writer == writer));
        throttled.push(Deferred {
            cell,
            writer,
            due,
            apply,
        });
    }

    /// Apply throttled writes whose window has room again.
    pub(crate) fn apply_throttled(&self) {
        let now = self.now();
        let due: Vec<Deferred> = {
            let mut throttled = self.inner.throttled.borrow_mut();
            let (due, keep): (Vec<_>, Vec<_>) = throttled.drain(..).partition(|d| d.due <= now);
            *throttled = keep;
            due
        };
        for d in due {
            if !self.exists(d.cell) {
                continue;
            }
            {
                let mut map = self.inner.rate.borrow_mut();
                let w = map.entry((d.cell, d.writer)).or_default();
                while w.writes.front().is_some_and(|&(_, t)| t + WINDOW <= now) {
                    w.writes.pop_front();
                }
                w.writes.push_back((self.tick_seq(), now));
                w.throttled_tick = None;
            }
            (d.apply)(self);
        }
        self.inner
            .rate
            .borrow_mut()
            .retain(|&(cell, writer), _| self.exists(cell) && self.exists(writer));
    }
}

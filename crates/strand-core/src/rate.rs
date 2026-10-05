//! Feedback-loop guard: more than [`MAX_WRITES_PER_SEC`] writes per second
//! to one cell from one handler warns once and throttles.
//!
//! The guard is for loops *through the graph*: handlers triggered by state
//! (effects, `on change`, timers, listeners of service events, and the
//! tasks they spawn). Handlers run for external input (`on click`,
//! `on scroll`, `<->` writes from widgets; see [`Runtime::input_events`]
//! and [`Runtime::spawn_input`]) are like CLI and service writes and are not
//! counted: smooth scrolling at 60 Hz is the user, not a loop. For a task
//! the exemption covers its synchronous response, up to its first `await`
//! that suspends; after that it is counted like any handler, so
//! `on click { loop { x += 1; await sleep(10ms) } }` is still throttled.
//!
//! Writes are counted per logic step, not per call: a handler that writes a
//! cell many times inside one flush (or one batch of timer bodies in
//! `advance_to`) coalesces to one write. Writing the current
//! value is not counted. Once a handler has attempted more than 30 writes
//! to a cell within one second it is throttled: a write goes through at
//! most every 1/30 s and only the latest value is held in between (a leaky
//! bucket, so the cell keeps moving smoothly at 30 Hz instead of bursting
//! and stalling). `update` reads the held value, so `x += 1` loses no step.
//! A held write is dropped if its handler is disposed (cancelled) before it
//! lands, unless the handler was a task that finished normally.
//!
//! "One handler" is a stable identity: the effect, listener or timer node,
//! inherited by tasks they spawn, or the handler site given to
//! [`Runtime::spawn_for`]. A fresh task per event is still one writer.

use std::any::Any;
use std::collections::VecDeque;
use std::time::Duration;

use crate::runtime::{Diagnostic, NodeId, Runtime};

/// The threshold from the design: more than this many writes per second
/// to one cell from one handler is a feedback loop.
pub const MAX_WRITES_PER_SEC: usize = 30;

const WINDOW: Duration = Duration::from_secs(1);

/// While throttled, at most one write per this interval goes through.
pub const THROTTLED_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

#[derive(Default)]
pub(crate) struct RateWindow {
    /// Times of the ticks in the last second in which this handler tried
    /// to change the cell (written or held).
    attempts: VecDeque<Duration>,
    /// The logic step (`advance_to` or `flush`) of the latest attempt and
    /// whether it went through.
    last_tick: Option<(u64, bool)>,
    /// When the latest write went through.
    last_through: Option<Duration>,
    warned: bool,
}

impl RateWindow {
    fn prune(&mut self, now: Duration) {
        while self.attempts.front().is_some_and(|&t| t + WINDOW <= now) {
            self.attempts.pop_front();
        }
        if self.attempts.is_empty() {
            self.warned = false;
        }
    }
    fn throttling(&self) -> bool {
        self.attempts.len() > MAX_WRITES_PER_SEC
    }
    /// When a held write may go through.
    fn due(&self, now: Duration) -> Duration {
        self.last_through
            .and_then(|t| t.checked_add(THROTTLED_INTERVAL))
            .unwrap_or(now)
    }
}

pub(crate) struct Deferred {
    pub(crate) cell: NodeId,
    pub(crate) writer: NodeId,
    pub(crate) due: Duration,
    /// The writer was a task that finished normally: its last held write
    /// still lands.
    pub(crate) detached: bool,
    /// The value being held, for read-your-writes in `update`.
    pub(crate) value: Option<Box<dyn Any>>,
    /// Lands the write; gets the held value back.
    pub(crate) apply: DeferredApply,
}

pub(crate) type DeferredApply = Box<dyn FnOnce(&Runtime, Option<Box<dyn Any>>)>;

impl Runtime {
    /// Decide whether a write from the current handler to `cell` goes
    /// through now (`true`) or is held (`false`).
    pub(crate) fn rate_gate(&self, cell: NodeId) -> bool {
        self.rate_check(cell, true)
    }

    /// What [`Runtime::rate_gate`] would answer now, without counting an
    /// attempt (a keyed write checks first, so it can change the list in
    /// place and count only when something changed).
    pub(crate) fn rate_would_pass(&self, cell: NodeId) -> bool {
        self.rate_check(cell, false)
    }

    fn rate_check(&self, cell: NodeId, commit: bool) -> bool {
        if self.inner.input.get() {
            return true;
        }
        let Some(writer) = self.inner.writer.get() else {
            return true;
        };
        let now = self.now();
        let tick = self.inner.epoch.get();
        let mut map = self.inner.rate.borrow_mut();
        if !commit {
            let Some(w) = map.get(&(cell, writer)) else {
                return true;
            };
            if let Some((t, through)) = w.last_tick
                && t == tick
            {
                return through;
            }
            let recent = w.attempts.iter().filter(|&&t| t + WINDOW > now).count();
            return recent < MAX_WRITES_PER_SEC || now >= w.due(now);
        }
        let w = map.entry((cell, writer)).or_default();
        w.prune(now);
        if let Some((t, through)) = w.last_tick
            && t == tick
        {
            // Coalesces with this tick's earlier attempt.
            return through;
        }
        w.attempts.push_back(now);
        let through = !w.throttling() || now >= w.due(now);
        let warn = w.throttling() && !w.warned;
        if warn {
            w.warned = true;
        }
        w.last_tick = Some((tick, through));
        if through {
            w.last_through = Some(now);
        }
        drop(map);
        if warn {
            let names = self.path(vec![writer, cell]);
            self.diagnose(Diagnostic::WriteRate {
                cell,
                writer,
                names,
            });
        }
        through
    }

    /// A write that went through supersedes held writes for the same cell
    /// (latest value wins).
    pub(crate) fn drop_deferred(&self, cell: NodeId) {
        let mut throttled = self.inner.throttled.borrow_mut();
        if !throttled.is_empty() {
            throttled.retain(|d| d.cell != cell);
        }
    }

    /// True while a held write to `cell` is waiting.
    pub(crate) fn has_deferred(&self, cell: NodeId) -> bool {
        self.inner.throttled.borrow().iter().any(|d| d.cell == cell)
    }

    /// The value the running handler's held write to `cell` holds.
    pub(crate) fn deferred_value<T: Clone + 'static>(&self, cell: NodeId) -> Option<T> {
        let writer = self.inner.writer.get()?;
        self.inner
            .throttled
            .borrow()
            .iter()
            .find(|d| d.cell == cell && d.writer == writer)
            .and_then(|d| d.value.as_ref()?.downcast_ref::<T>().cloned())
    }

    /// Take the running handler's held write to `cell` out of the queue
    /// (a keyed write applies the next operation to it in place, then holds
    /// it again or lets it through).
    pub(crate) fn take_deferred<T: 'static>(&self, cell: NodeId) -> Option<T> {
        let writer = self.inner.writer.get()?;
        let mut throttled = self.inner.throttled.borrow_mut();
        let at = throttled.iter().position(|d| {
            d.cell == cell && d.writer == writer && d.value.as_ref().is_some_and(|v| v.is::<T>())
        })?;
        let d = throttled.remove(at);
        d.value?.downcast::<T>().ok().map(|b| *b)
    }

    /// Hold a throttled write; a newer one from the same handler replaces it.
    pub(crate) fn defer_write(
        &self,
        cell: NodeId,
        value: Option<Box<dyn Any>>,
        apply: DeferredApply,
    ) {
        let Some(writer) = self.inner.writer.get() else {
            apply(self, value);
            return;
        };
        let now = self.now();
        let due = self
            .inner
            .rate
            .borrow()
            .get(&(cell, writer))
            .map_or(now, |w| w.due(now));
        let mut throttled = self.inner.throttled.borrow_mut();
        throttled.retain(|d| !(d.cell == cell && d.writer == writer));
        throttled.push(Deferred {
            cell,
            writer,
            due,
            detached: false,
            value,
            apply,
        });
    }

    /// A task finished normally: its held writes still land.
    pub(crate) fn detach_deferred(&self, writer: NodeId) {
        for d in self.inner.throttled.borrow_mut().iter_mut() {
            if d.writer == writer {
                d.detached = true;
            }
        }
    }

    /// Apply held writes whose interval has passed.
    pub(crate) fn apply_throttled(&self) {
        let now = self.now();
        let due: Vec<Deferred> = {
            let mut throttled = self.inner.throttled.borrow_mut();
            let (due, keep): (Vec<_>, Vec<_>) = throttled.drain(..).partition(|d| d.due <= now);
            *throttled = keep;
            due
        };
        for d in due {
            if !self.exists(d.cell) || !(d.detached || self.exists(d.writer)) {
                continue;
            }
            if let Some(w) = self.inner.rate.borrow_mut().get_mut(&(d.cell, d.writer)) {
                w.last_through = Some(now);
                if let Some((t, through)) = &mut w.last_tick
                    && *t == self.inner.epoch.get()
                {
                    *through = true;
                }
            }
            (d.apply)(self, d.value);
        }
        // Forget handlers that have been quiet for a whole window.
        self.inner.rate.borrow_mut().retain(|_, w| {
            w.prune(now);
            !w.attempts.is_empty()
        });
    }

    /// After a disposal: drop rate state and held writes of nodes that are
    /// gone (a cancelled handler's write never lands late).
    pub(crate) fn forget_rate_state(&self) {
        let alive = |id: NodeId| self.exists(id);
        {
            let mut rate = self.inner.rate.borrow_mut();
            if !rate.is_empty() {
                rate.retain(|&(cell, writer), _| alive(cell) && alive(writer));
            }
        }
        let mut throttled = self.inner.throttled.borrow_mut();
        if !throttled.is_empty() {
            throttled.retain(|d| alive(d.cell) && (d.detached || alive(d.writer)));
        }
    }
}

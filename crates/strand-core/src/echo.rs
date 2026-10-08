//! Echo suppression for service-backed cells.
//!
//! A local write to an `rw` service field is applied optimistically and sent
//! to the service tagged with a [`Generation`]. Services report values back;
//! when that report is the echo of a write we made (by tag, or by value for
//! protocols such as D-Bus `PropertiesChanged` that carry no tag) it is
//! ignored, so a slider being dragged never snaps back to an older value.
//! A value that matches no pending write is an outside change and wins.
//! A write an outside change overtook (the outside value reached the cell
//! before the service answered the write) is not forgotten: the service
//! handled the write after that change, so its tagged answer is the
//! newest truth and settles the cell.
//!
//! The write-rate guard covers the service path too: while a handler's
//! writes to the cell are throttled, nothing is sent; the latest value is
//! held and sent (with a fresh tag) when the window has room, so a feedback
//! loop through a service is cut at 30 writes per second like a local one.
//! At most [`MAX_PENDING_ECHOES`] unacknowledged writes are remembered.

use std::collections::VecDeque;

use crate::error::Error;
use crate::runtime::Runtime;
use crate::signal::Signal;

/// The tag a local write to a service carries.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// What [`Signal::receive`] did with a service report.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Received {
    /// The value was applied (an outside change, or the service settled on a
    /// different value than we asked for).
    Applied,
    /// The report was the echo of a pending local write and was ignored.
    Echo,
}

/// Unacknowledged writes remembered per cell; older ones are forgotten (an
/// outside value clears them anyway).
pub const MAX_PENDING_ECHOES: usize = 64;

/// The echo bookkeeping of one cell (or of one item of a keyed list:
/// [`crate::KeyedSignal::write_item_tagged`]).
pub(crate) struct EchoState<T> {
    pub(crate) next: u64,
    pending: VecDeque<(Generation, T)>,
    /// Pending writes an outside value cleared, oldest first: their tagged
    /// answers settle (bounded like `pending`).
    overtaken: VecDeque<Generation>,
}

impl<T> Default for EchoState<T> {
    fn default() -> Self {
        EchoState {
            next: 1,
            pending: VecDeque::new(),
            overtaken: VecDeque::new(),
        }
    }
}

/// What a report means for a cell.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The echo of a pending write: ignore it.
    Echo,
    /// An outside change: apply it.
    Apply,
    /// The service answered our last write (possibly with another value
    /// than we asked for, or after an outside change): take it.
    Settle,
}

impl<T: PartialEq> EchoState<T> {
    /// An outside value: pending writes are overtaken.
    fn overtake(&mut self) {
        self.overtaken
            .extend(self.pending.drain(..).map(|(g, _)| g));
        while self.overtaken.len() > MAX_PENDING_ECHOES {
            self.overtaken.pop_front();
        }
    }

    /// Remember a write of `value` tagged `g` (from a shared counter, so
    /// `g` may skip numbers).
    pub(crate) fn record(&mut self, g: Generation, value: T) {
        self.pending.push_back((g, value));
        while self.pending.len() > MAX_PENDING_ECHOES {
            self.pending.pop_front();
        }
    }

    /// A tag this state is waiting for (pending or overtaken).
    pub(crate) fn knows(&self, g: Generation) -> bool {
        self.pending.iter().any(|(pg, _)| *pg == g) || self.overtaken.contains(&g)
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.overtaken.is_empty()
    }

    /// Forget every write: a service run that ended never answers them,
    /// and the next run's reports are not their echoes.
    pub(crate) fn forget(&mut self) {
        self.pending.clear();
        self.overtaken.clear();
    }

    /// What a report of `value` (tagged `echo_of`) means; `next` is the
    /// next tag the counter would give.
    pub(crate) fn verdict(&mut self, value: &T, echo_of: Option<Generation>, next: u64) -> Verdict {
        match echo_of {
            Some(g) => {
                let known = self.pending.iter().any(|(pg, _)| *pg == g);
                let older = self.pending.front().is_some_and(|(pg, _)| g < *pg);
                let overtaken = self.overtaken.contains(&g);
                while self.overtaken.front().is_some_and(|og| *og <= g) {
                    self.overtaken.pop_front();
                }
                if overtaken {
                    // The service handled this write after an outside
                    // value we already applied: its answer is newer than
                    // that value, unless a newer write of ours is pending
                    // (whose answer comes next).
                    if self.pending.is_empty() {
                        Verdict::Settle
                    } else {
                        Verdict::Echo
                    }
                } else if known {
                    while self.pending.front().is_some_and(|(pg, _)| *pg <= g) {
                        self.pending.pop_front();
                    }
                    if self.pending.is_empty() {
                        Verdict::Settle
                    } else {
                        Verdict::Echo
                    }
                } else if older || self.pending.is_empty() && g.0 < next {
                    // An echo of a write already acknowledged.
                    Verdict::Echo
                } else {
                    self.pending.clear();
                    self.overtaken.clear();
                    Verdict::Apply
                }
            }
            None => match self.pending.iter().position(|(_, v)| v == value) {
                Some(i) => {
                    self.pending.drain(..=i);
                    Verdict::Echo
                }
                None => {
                    self.overtake();
                    Verdict::Apply
                }
            },
        }
    }
}

/// The echo state of node `id` in `rt` (made on first use), as `S`.
pub(crate) fn with_state<S: Default + 'static, R>(
    rt: &Runtime,
    id: crate::NodeId,
    f: impl FnOnce(&mut S) -> R,
) -> R {
    let mut map = rt.inner.echo.borrow_mut();
    if map.get(id).and_then(|b| b.downcast_ref::<S>()).is_none() {
        map.insert(id, Box::new(S::default()));
    }
    // Inserted above; the downcast cannot fail for this `S`.
    match map.get_mut(id).and_then(|b| b.downcast_mut::<S>()) {
        Some(state) => f(state),
        None => f(&mut S::default()),
    }
}

/// The echo state of node `id`, if it has one (none is made).
pub(crate) fn peek_state<S: 'static, R>(
    rt: &Runtime,
    id: crate::NodeId,
    f: impl FnOnce(&mut S) -> R,
) -> Option<R> {
    let mut map = rt.inner.echo.borrow_mut();
    map.get_mut(id).and_then(|b| b.downcast_mut::<S>()).map(f)
}

impl<T: Clone + PartialEq + 'static> Signal<T> {
    fn with_echo<R>(self, rt: &Runtime, f: impl FnOnce(&mut EchoState<T>) -> R) -> R {
        with_state(rt, self.id, f)
    }

    /// A local write destined for a service. When the write-rate guard lets
    /// it through, the value is applied, remembered as pending, and `send`
    /// is called with it and its tag; the tag is returned. When this
    /// handler is throttled, `Ok(None)` is returned and nothing is sent yet:
    /// the latest held write is applied and sent when the window has room
    /// (a newer write replaces it, and so does its `send`).
    pub fn write_tagged(
        self,
        rt: &Runtime,
        value: T,
        send: impl FnOnce(&Runtime, &T, Generation) + 'static,
    ) -> Result<Option<Generation>, Error> {
        rt.check_write_allowed(self.id)?;
        rt.note_write(self.id);
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        let commit = move |rt: &Runtime, value: T| -> Result<Generation, Error> {
            self.set_raw(rt, value.clone())?;
            let g = self.with_echo(rt, |s| {
                let g = Generation(s.next);
                s.next += 1;
                s.record(g, value.clone());
                g
            });
            send(rt, &value, g);
            Ok(g)
        };
        if rt.rate_gate(self.id) {
            // Supersedes any held write, even when the value is unchanged.
            rt.drop_deferred(self.id);
            commit(rt, value).map(Some)
        } else {
            let held = Box::new(value.clone());
            rt.defer_write(
                self.id,
                Some(held),
                Box::new(move |rt: &Runtime, _held| {
                    let _ = commit(rt, value);
                }),
                None,
            );
            Ok(None)
        }
    }

    /// Number of local writes the service has not echoed yet.
    pub fn pending_writes(self, rt: &Runtime) -> usize {
        if !rt.exists(self.id) {
            return 0;
        }
        self.with_echo(rt, |s| s.pending_len())
    }

    /// Forget the pending writes: the service run they went to ended
    /// without answering them, so a later report equal to one of them is
    /// an outside change, not its echo. Tags keep counting up.
    pub fn forget_echoes(self, rt: &Runtime) {
        if rt.exists(self.id) {
            self.with_echo(rt, EchoState::forget);
        }
    }

    /// A value reported by the service. `echo_of` is the generation the
    /// service says this value reflects, when its protocol carries one.
    pub fn receive(
        self,
        rt: &Runtime,
        value: T,
        echo_of: Option<Generation>,
    ) -> Result<Received, Error> {
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        let verdict = self.with_echo(rt, |s| {
            let next = s.next;
            s.verdict(&value, echo_of, next)
        });
        match verdict {
            Verdict::Echo => Ok(Received::Echo),
            Verdict::Apply => {
                self.set_raw(rt, value)?;
                Ok(Received::Applied)
            }
            Verdict::Settle => {
                if self.set_raw(rt, value)? {
                    Ok(Received::Applied)
                } else {
                    Ok(Received::Echo)
                }
            }
        }
    }
}

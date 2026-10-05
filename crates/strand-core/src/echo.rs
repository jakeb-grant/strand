//! Echo suppression for service-backed cells.
//!
//! A local write to an `rw` service field is applied optimistically and sent
//! to the service tagged with a [`Generation`]. Services report values back;
//! when that report is the echo of a write we made (by tag, or by value for
//! protocols such as D-Bus `PropertiesChanged` that carry no tag) it is
//! ignored, so a slider being dragged never snaps back to an older value.
//! A value that matches no pending write is an outside change and wins.
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

struct EchoState<T> {
    next: u64,
    pending: VecDeque<(Generation, T)>,
}

impl<T: Clone + PartialEq + 'static> Signal<T> {
    fn with_echo<R>(self, rt: &Runtime, f: impl FnOnce(&mut EchoState<T>) -> R) -> R {
        let mut map = rt.inner.echo.borrow_mut();
        if map
            .get(self.id)
            .and_then(|b| b.downcast_ref::<EchoState<T>>())
            .is_none()
        {
            map.insert(
                self.id,
                Box::new(EchoState::<T> {
                    next: 1,
                    pending: VecDeque::new(),
                }),
            );
        }
        // Inserted above; the downcast cannot fail for this `T`.
        match map
            .get_mut(self.id)
            .and_then(|b| b.downcast_mut::<EchoState<T>>())
        {
            Some(state) => f(state),
            None => f(&mut EchoState {
                next: 0,
                pending: VecDeque::new(),
            }),
        }
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
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        let commit = move |rt: &Runtime, value: T| -> Result<Generation, Error> {
            self.set_raw(rt, value.clone())?;
            let g = self.with_echo(rt, |s| {
                let g = Generation(s.next);
                s.next += 1;
                s.pending.push_back((g, value.clone()));
                while s.pending.len() > MAX_PENDING_ECHOES {
                    s.pending.pop_front();
                }
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
                Box::new(move |rt: &Runtime| {
                    let _ = commit(rt, value);
                }),
            );
            Ok(None)
        }
    }

    /// Number of local writes the service has not echoed yet.
    pub fn pending_writes(self, rt: &Runtime) -> usize {
        if !rt.exists(self.id) {
            return 0;
        }
        self.with_echo(rt, |s| s.pending.len())
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
        enum Verdict {
            Echo,
            Apply,
            /// The service acknowledged our last write but settled on another
            /// value (clamped); take it.
            Settle,
        }
        let verdict = self.with_echo(rt, |s| match echo_of {
            Some(g) => {
                let known = s.pending.iter().any(|(pg, _)| *pg == g);
                let older = s.pending.front().is_some_and(|(pg, _)| g < *pg);
                if known {
                    while s.pending.front().is_some_and(|(pg, _)| *pg <= g) {
                        s.pending.pop_front();
                    }
                    if s.pending.is_empty() {
                        Verdict::Settle
                    } else {
                        Verdict::Echo
                    }
                } else if older || s.pending.is_empty() && g.0 < s.next {
                    // An echo of a write already acknowledged.
                    Verdict::Echo
                } else {
                    s.pending.clear();
                    Verdict::Apply
                }
            }
            None => match s.pending.iter().position(|(_, v)| *v == value) {
                Some(i) => {
                    s.pending.drain(..=i);
                    Verdict::Echo
                }
                None => {
                    s.pending.clear();
                    Verdict::Apply
                }
            },
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

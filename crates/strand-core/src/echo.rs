//! Echo suppression for service-backed cells.
//!
//! A local write to an `rw` service field is applied optimistically and sent
//! to the service tagged with a [`Generation`]. Services report values back;
//! when that report is the echo of a write we made (by tag, or by value for
//! protocols such as D-Bus `PropertiesChanged` that carry no tag) it is
//! ignored, so a slider being dragged never snaps back to an older value.
//! A value that matches no pending write is an outside change and wins.

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

    /// A local write destined for a service: applied now, remembered as
    /// pending, and returned tag to send along with it.
    pub fn write_tagged(self, rt: &Runtime, value: T) -> Result<Generation, Error> {
        self.set(rt, value.clone())?;
        Ok(self.with_echo(rt, |s| {
            let g = Generation(s.next);
            s.next += 1;
            s.pending.push_back((g, value));
            g
        }))
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

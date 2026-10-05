//! `Async<T>`: a value that loads, without ceremony.
//!
//! It keeps its previous value while a new one loads and exposes `.pending`
//! and `.error`. `x ?? fallback` ([`Async::or`]) gives the value when there
//! is one, so a search box keeps showing the last results while typing.
//! Responses to superseded requests are ignored, so a slow early request
//! never overwrites a newer result.
//!
//! A load that is cancelled (unmount, a handler restart on reload) clears
//! `pending` if it was still the latest request, so a kept cell never shows
//! a spinner forever. Bookkeeping writes (`begin`, `resolve`, cancel) bypass
//! the write-rate guard: they must see each other, and a search box typing
//! faster than 30 keys a second is not a feedback loop.
//! [`Runtime::async_memo`] is the derived form (`let hits =
//! apps.search(query)`): it re-requests when its tracked inputs change and
//! drops superseded requests quietly.

use std::fmt;
use std::future::Future;

use crate::error::Error;
use crate::runtime::{NodeId, Runtime, WeakRuntime};
use crate::signal::{Effect, Signal};
use crate::task::Task;

/// Identifies one load; only the latest one may resolve.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// A value that loads asynchronously.
#[derive(Clone, Debug, PartialEq)]
pub struct Async<T> {
    value: Option<T>,
    pending: bool,
    error: Option<Error>,
    latest: u64,
}

impl<T> Default for Async<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> Async<T> {
    /// No value yet, nothing loading.
    pub fn empty() -> Self {
        Self {
            value: None,
            pending: false,
            error: None,
            latest: 0,
        }
    }

    /// A settled value.
    pub fn ready(value: T) -> Self {
        Self {
            value: Some(value),
            ..Self::empty()
        }
    }

    /// The current (or previous, while loading) value.
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// `.pending`: a load is in flight.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// `.error`: the last load failed. The previous value is kept.
    pub fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// `x ?? fallback`: the value if there is one, else `fallback` (still
    /// loading the first time, or failed with nothing to keep).
    pub fn or(&self, fallback: T) -> T
    where
        T: Clone,
    {
        self.value.clone().unwrap_or(fallback)
    }

    /// Start a load: `pending` becomes true, the value and any error stay.
    pub fn begin(&mut self) -> RequestId {
        self.latest += 1;
        self.pending = true;
        RequestId(self.latest)
    }

    /// Load `id` was cancelled: if it is still the latest, nothing is in
    /// flight any more (`pending` clears; value and error stay). Returns
    /// whether anything changed.
    pub fn cancel(&mut self, id: RequestId) -> bool {
        if id.0 != self.latest || !self.pending {
            return false;
        }
        self.pending = false;
        true
    }

    /// Finish load `id`. Returns `false` (and changes nothing) if a newer
    /// load has started since. Success replaces the value and clears the
    /// error; failure keeps the value and sets the error.
    pub fn resolve(&mut self, id: RequestId, result: Result<T, Error>) -> bool {
        if id.0 != self.latest {
            return false;
        }
        self.pending = false;
        match result {
            Ok(v) => {
                self.value = Some(v);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        true
    }
}

impl<T: Clone + PartialEq + 'static> Signal<Async<T>> {
    /// Read-modify-write without the rate gate.
    fn bookkeep<R>(self, rt: &Runtime, f: impl FnOnce(&mut Async<T>) -> R) -> Result<R, Error> {
        rt.check_write_allowed(self.id)?;
        // Not rate-gated, but still a write edge for the flush order.
        rt.note_write(self.id);
        let mut a = self.get_untracked(rt)?;
        let r = f(&mut a);
        self.set_raw(rt, a)?;
        Ok(r)
    }

    /// Begin a load on an `Async` cell.
    pub fn begin(self, rt: &Runtime) -> Result<RequestId, Error> {
        self.bookkeep(rt, Async::begin)
    }

    /// Resolve load `id`; stale ids are ignored (returns `false`).
    pub fn resolve(
        self,
        rt: &Runtime,
        id: RequestId,
        result: Result<T, Error>,
    ) -> Result<bool, Error> {
        self.bookkeep(rt, |a| a.resolve(id, result))
    }

    /// Load `id` was cancelled; see [`Async::cancel`].
    pub fn cancel(self, rt: &Runtime, id: RequestId) -> Result<bool, Error> {
        self.bookkeep(rt, |a| a.cancel(id))
    }

    /// Begin a load and run `fut` as a handler that resolves it. The handler
    /// is owned by the current owner, so unmounting cancels the load; a
    /// cancelled load clears `pending` if it was the latest.
    pub fn load<F>(self, rt: &Runtime, fut: F) -> Result<Task, Error>
    where
        F: Future<Output = Result<T, Error>> + 'static,
    {
        let id = self.begin(rt)?;
        let mut guard = CancelGuard {
            cell: self,
            id,
            rt: rt.downgrade(),
            armed: true,
        };
        Ok(rt.spawn(async move {
            let result = fut.await;
            guard.armed = false;
            match guard.rt.upgrade() {
                Some(rt) => self.resolve(&rt, id, result).map(|_| ()),
                None => Ok(()),
            }
        }))
    }
}

/// Lives inside a load's future: dropped before the load resolved means
/// cancelled.
struct CancelGuard<T: Clone + PartialEq + 'static> {
    cell: Signal<Async<T>>,
    id: RequestId,
    rt: WeakRuntime,
    armed: bool,
}

impl<T: Clone + PartialEq + 'static> Drop for CancelGuard<T> {
    fn drop(&mut self) {
        if self.armed
            && let Some(rt) = self.rt.upgrade()
        {
            // The cell may be gone too (unmounted together): ignore.
            let _ = self.cell.cancel(&rt, self.id);
        }
    }
}

/// A derived `Async` value (`let hits = apps.search(query)`): read-only,
/// like every `let`. Copyable handle.
pub struct AsyncMemo<T> {
    cell: Signal<Async<T>>,
    effect: Effect,
}

impl<T> Clone for AsyncMemo<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for AsyncMemo<T> {}
impl<T> PartialEq for AsyncMemo<T> {
    fn eq(&self, other: &Self) -> bool {
        self.cell == other.cell
    }
}
impl<T> Eq for AsyncMemo<T> {}
impl<T> fmt::Debug for AsyncMemo<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AsyncMemo({:?})", self.cell.id())
    }
}

impl<T: Clone + PartialEq + 'static> AsyncMemo<T> {
    /// Read and track.
    pub fn get(self, rt: &Runtime) -> Result<Async<T>, Error> {
        self.cell.get(rt)
    }
    /// Read without tracking.
    pub fn get_untracked(self, rt: &Runtime) -> Result<Async<T>, Error> {
        self.cell.get_untracked(rt)
    }
    /// The node holding the value (for [`Runtime::watch`], and what readers
    /// declare in [`Runtime::reads_from`]).
    pub fn id(self) -> NodeId {
        self.cell.id()
    }
    /// The internal effect that tracks the input and starts loads: the VM
    /// declares the input's syntactic read set on it
    /// ([`Runtime::reads_from`]). Its write edge to the value is declared
    /// by [`Runtime::async_memo`].
    pub fn effect_id(self) -> NodeId {
        self.effect.id()
    }
    /// Stop requesting and dispose the value; a running load is cancelled.
    pub fn dispose(self, rt: &Runtime) {
        self.effect.dispose(rt);
        self.cell.dispose(rt);
    }
}

impl Runtime {
    /// A derived `Async` value (`let hits = apps.search(query)`): `input`
    /// is tracked; whenever it changes, `fetch(input)` is started as a new
    /// load and the previous one, if still running, is dropped without a
    /// [`crate::Diagnostic::Cancelled`] (superseding a keystroke is not a
    /// cancellation worth reporting). The value is kept while loading. An
    /// `Err` from `input` becomes the cell's `.error`. Owned by the current
    /// owner. Read-only, like every `let`.
    ///
    /// The effect's write edge to the value is declared here, so a reader
    /// that declares its reads runs after the load started (and, for a
    /// load that is ready at once, resolved) in the same flush, once. The
    /// VM declares the input's reads on [`AsyncMemo::effect_id`].
    pub fn async_memo<A, T, I, F, Fut>(&self, input: I, fetch: F) -> AsyncMemo<T>
    where
        A: 'static,
        T: Clone + PartialEq + 'static,
        I: Fn(&Runtime) -> Result<A, Error> + 'static,
        F: Fn(A) -> Fut + 'static,
        Fut: Future<Output = Result<T, Error>> + 'static,
    {
        let cell = self.signal(Async::empty());
        let mut running: Option<Task> = None;
        let effect = self.effect(move |rt| {
            let input = input(rt);
            if let Some(task) = running.take() {
                rt.cancel_quietly(task);
            }
            match input {
                Ok(a) => {
                    let fut = rt.untrack(|_| fetch(a));
                    let task = cell.load(rt, fut)?;
                    rt.set_quiet(task);
                    running = Some(task);
                }
                Err(e) => {
                    let id = cell.begin(rt)?;
                    cell.resolve(rt, id, Err(e))?;
                }
            }
            Ok(())
        });
        // Both just created, so this cannot fail (and never closes a loop:
        // nothing reads the cell yet).
        let _ = self.writes_to(effect.id(), cell.id());
        AsyncMemo { cell, effect }
    }
}

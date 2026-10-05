//! `Async<T>`: a value that loads, without ceremony.
//!
//! It keeps its previous value while a new one loads and exposes `.pending`
//! and `.error`. `x ?? fallback` ([`Async::or`]) gives the value when there
//! is one, so a search box keeps showing the last results while typing.
//! Responses to superseded requests are ignored, so a slow early request
//! never overwrites a newer result.

use std::future::Future;

use crate::error::Error;
use crate::runtime::Runtime;
use crate::signal::Signal;
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
    /// Begin a load on an `Async` cell.
    pub fn begin(self, rt: &Runtime) -> Result<RequestId, Error> {
        let mut a = self.get_untracked(rt)?;
        let id = a.begin();
        self.set(rt, a)?;
        Ok(id)
    }

    /// Resolve load `id`; stale ids are ignored (returns `false`).
    pub fn resolve(
        self,
        rt: &Runtime,
        id: RequestId,
        result: Result<T, Error>,
    ) -> Result<bool, Error> {
        let mut a = self.get_untracked(rt)?;
        if !a.resolve(id, result) {
            return Ok(false);
        }
        self.set(rt, a)?;
        Ok(true)
    }

    /// Begin a load and run `fut` as a handler that resolves it. The handler
    /// is owned by the current owner, so unmounting cancels the load.
    pub fn load<F>(self, rt: &Runtime, fut: F) -> Result<Task, Error>
    where
        F: Future<Output = Result<T, Error>> + 'static,
    {
        let id = self.begin(rt)?;
        let weak = rt.downgrade();
        Ok(rt.spawn(async move {
            let result = fut.await;
            match weak.upgrade() {
                Some(rt) => self.resolve(&rt, id, result).map(|_| ()),
                None => Ok(()),
            }
        }))
    }
}

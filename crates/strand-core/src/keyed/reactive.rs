//! Keyed collections in the graph.
//!
//! A [`KeyedSignal`] is a collection cell; handlers mutate it and services
//! apply their [`VecDiff`]s to it. Reads return a [`Snapshot`]: the items
//! (shared, not copied) plus a version and access to the diff log, so a
//! consumer that saw version `v` can ask for exactly the diffs since `v`
//! ([`Snapshot::diffs_since`]); the scene emitter turns those into
//! `Create`/`Remove`/`Move` ops by key.
//!
//! [`KeyedOps`] derives collections incrementally: each derived node keeps
//! its operator state and the source version it has consumed, and on
//! recompute feeds only the new source diffs through the operator. If the
//! operator's parameters change (`!dnd` in a filter), or the consumer fell
//! behind the bounded log, it rebuilds and publishes a keyed diff against
//! its previous output, so keys and identity survive either way.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::rc::Rc;

use super::ops::{Filter, IncrementalOp, Map, SortBy, Take};
use super::{KeyedVec, VecDiff, keyed_diff};
use crate::error::Error;
use crate::runtime::{Color, NodeData, NodeId, NodeKind, RunOutcome, Runtime};

/// Diff log entries kept per collection before lagging readers get a
/// `Reset`.
pub const LOG_CAPACITY: usize = 256;

struct DiffLog<K, T> {
    /// Version before the first entry.
    base: u64,
    entries: VecDeque<VecDiff<K, T>>,
}

impl<K: Clone, T: Clone> DiffLog<K, T> {
    fn new() -> Self {
        Self {
            base: 0,
            entries: VecDeque::new(),
        }
    }
    fn version(&self) -> u64 {
        self.base + self.entries.len() as u64
    }
    fn push(&mut self, d: VecDiff<K, T>) {
        self.entries.push_back(d);
        while self.entries.len() > LOG_CAPACITY {
            self.entries.pop_front();
            self.base += 1;
        }
    }
    fn since(&self, v: u64, upto: u64) -> Option<Vec<VecDiff<K, T>>> {
        if v < self.base || upto > self.version() || v > upto {
            return None;
        }
        let a = (v - self.base) as usize;
        let b = (upto - self.base) as usize;
        Some(self.entries.range(a..b).cloned().collect())
    }
}

/// An immutable view of a keyed collection at one version.
pub struct Snapshot<K, T> {
    items: Rc<Vec<(K, T)>>,
    version: u64,
    log: Rc<RefCell<DiffLog<K, T>>>,
}

impl<K, T> Clone for Snapshot<K, T> {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            version: self.version,
            log: self.log.clone(),
        }
    }
}

impl<K, T> PartialEq for Snapshot<K, T> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.log, &other.log) && self.version == other.version
    }
}

impl<K: fmt::Debug, T: fmt::Debug> fmt::Debug for Snapshot<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.version)
            .field("items", &self.items)
            .finish()
    }
}

impl<K: Clone, T: Clone> Snapshot<K, T> {
    /// Items with keys, in order.
    pub fn items(&self) -> &[(K, T)] {
        &self.items
    }
    /// Values in order.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.items.iter().map(|(_, v)| v)
    }
    /// Keys in order.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.items.iter().map(|(k, _)| k)
    }
    /// `xs.len`.
    pub fn len(&self) -> usize {
        self.items.len()
    }
    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// This snapshot's version (one per diff).
    pub fn version(&self) -> u64 {
        self.version
    }
    /// True when both snapshots come from the same collection node, so
    /// [`Snapshot::diffs_since`] applies.
    pub fn same_source(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.log, &other.log)
    }
    /// Diffs that turn the list at `version` into this snapshot. `None`
    /// when they are no longer in the log; then use a `Reset` of
    /// [`Snapshot::items`].
    pub fn diffs_since(&self, version: u64) -> Option<Vec<VecDiff<K, T>>> {
        self.log.borrow().since(version, self.version)
    }
    /// [`Snapshot::diffs_since`], falling back to a `Reset`.
    pub fn diffs_or_reset(&self, version: Option<u64>) -> Vec<VecDiff<K, T>> {
        version
            .and_then(|v| self.diffs_since(v))
            .unwrap_or_else(|| {
                vec![VecDiff::Reset {
                    items: self.items.to_vec(),
                }]
            })
    }
}

/// Anything that yields snapshots: a collection cell or a derived
/// collection.
pub trait KeyedSource<K, T>: Copy + 'static {
    /// Read and track.
    fn snapshot(self, rt: &Runtime) -> Result<Snapshot<K, T>, Error>;
    /// The node id.
    fn node(self) -> NodeId;
}

// ----- cell ---------------------------------------------------------------

struct CellData<K, T> {
    vec: RefCell<KeyedVec<K, T>>,
    log: Rc<RefCell<DiffLog<K, T>>>,
}

impl<K: Clone + 'static, T: Clone + 'static> NodeData for CellData<K, T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn clone_value(&self) -> Option<Box<dyn Any>> {
        Some(Box::new(self.log.try_borrow().ok()?.version()))
    }
    fn value_eq(&self, other: &dyn Any) -> bool {
        match (other.downcast_ref::<u64>(), self.log.try_borrow()) {
            (Some(o), Ok(l)) => l.version() == *o,
            _ => false,
        }
    }
}

/// A keyed collection cell (`state xs: [T] key f = []`). Copyable handle.
pub struct KeyedSignal<K, T> {
    id: NodeId,
    _t: PhantomData<fn() -> (K, T)>,
}

impl<K, T> Clone for KeyedSignal<K, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K, T> Copy for KeyedSignal<K, T> {}
impl<K, T> fmt::Debug for KeyedSignal<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyedSignal({:?})", self.id)
    }
}

impl Runtime {
    /// Create a keyed collection cell.
    pub fn keyed<K, T>(&self, initial: KeyedVec<K, T>) -> KeyedSignal<K, T>
    where
        K: Clone + Eq + Hash + 'static,
        T: Clone + PartialEq + 'static,
    {
        let id = self.create_node(
            NodeKind::Collection,
            Color::Clean,
            Some(Rc::new(CellData {
                vec: RefCell::new(initial),
                log: Rc::new(RefCell::new(DiffLog::new())),
            })),
        );
        KeyedSignal {
            id,
            _t: PhantomData,
        }
    }
}

impl<K, T> KeyedSignal<K, T>
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
{
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }

    /// Dispose the cell.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }

    /// Run a mutation and publish its diffs.
    fn mutate<R>(
        self,
        rt: &Runtime,
        f: impl FnOnce(&mut KeyedVec<K, T>) -> Result<(Vec<VecDiff<K, T>>, R), Error>,
    ) -> Result<R, Error> {
        rt.check_write_allowed(self.id)?;
        let (diffs, r) = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let Ok(mut vec) = d.vec.try_borrow_mut() else {
                return Err(Error::Reentrant);
            };
            let (diffs, r) = f(&mut vec)?;
            let mut log = d.log.borrow_mut();
            for diff in &diffs {
                log.push(diff.clone());
            }
            Ok((diffs, r))
        })??;
        if !diffs.is_empty() {
            rt.cell_changed(self.id);
        }
        Ok(r)
    }

    /// `xs.push(v)`.
    pub fn push(self, rt: &Runtime, value: T) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((vec![v.push(value)?], ())))
    }

    /// `xs.insert(i, v)`.
    pub fn insert(self, rt: &Runtime, index: usize, value: T) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((vec![v.insert(index, value)?], ())))
    }

    /// `xs.remove_key(k)`.
    pub fn remove_key(self, rt: &Runtime, key: &K) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((vec![v.remove_key(key)?], ())))
    }

    /// `xs.move(k, to)`.
    pub fn move_key(self, rt: &Runtime, key: &K, to: usize) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((v.move_key(key, to)?.into_iter().collect(), ())))
    }

    /// `xs.update(k, f)`.
    pub fn update(self, rt: &Runtime, key: &K, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((v.update(key, f)?.into_iter().collect(), ())))
    }

    /// Replace the contents, keeping identity by key.
    pub fn replace_all(
        self,
        rt: &Runtime,
        values: impl IntoIterator<Item = T>,
    ) -> Result<(), Error> {
        self.mutate(rt, |v| Ok((v.replace_all(values)?, ())))
    }

    /// Apply diffs published by a service.
    pub fn apply(self, rt: &Runtime, diffs: &[VecDiff<K, T>]) -> Result<(), Error> {
        self.mutate(rt, |v| {
            let mut applied = Vec::with_capacity(diffs.len());
            for d in diffs {
                v.apply(d)?;
                applied.push(d.clone());
            }
            Ok((applied, ()))
        })
    }

    /// The current list as a [`KeyedVec`] (cheap clone), untracked.
    pub fn get_untracked(self, rt: &Runtime) -> Result<KeyedVec<K, T>, Error> {
        rt.with_data::<CellData<K, T>, _>(self.id, |d| d.vec.borrow().clone())
    }
}

impl<K, T> KeyedSource<K, T> for KeyedSignal<K, T>
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
{
    fn snapshot(self, rt: &Runtime) -> Result<Snapshot<K, T>, Error> {
        rt.track(self.id);
        rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let vec = d.vec.try_borrow().map_err(|_| Error::Reentrant)?;
            Ok(Snapshot {
                items: vec.shared(),
                version: d.log.borrow().version(),
                log: d.log.clone(),
            })
        })?
    }
    fn node(self) -> NodeId {
        self.id
    }
}

// ----- derived ------------------------------------------------------------

/// What a derived step produced.
enum Step<K, U> {
    Diffs(Vec<VecDiff<K, U>>),
    Rebuild(Vec<(K, U)>),
}

type StepFn<K, U> = Box<dyn FnMut(&Runtime) -> Result<Step<K, U>, Error>>;

struct DerivedData<K, U> {
    step: RefCell<StepFn<K, U>>,
    items: RefCell<Rc<Vec<(K, U)>>>,
    log: Rc<RefCell<DiffLog<K, U>>>,
    error: RefCell<Option<Error>>,
    started: std::cell::Cell<bool>,
}

impl<K, U> NodeData for DerivedData<K, U>
where
    K: Clone + Eq + Hash + 'static,
    U: Clone + PartialEq + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn clone_value(&self) -> Option<Box<dyn Any>> {
        let version = self.log.try_borrow().ok()?.version();
        let error = self.error.try_borrow().ok()?.clone();
        Some(Box::new((version, error)))
    }

    fn value_eq(&self, other: &dyn Any) -> bool {
        match (
            other.downcast_ref::<(u64, Option<Error>)>(),
            self.log.try_borrow(),
            self.error.try_borrow(),
        ) {
            (Some((v, e)), Ok(l), Ok(err)) => l.version() == *v && *err == *e,
            _ => false,
        }
    }

    fn run(&self, rt: &Runtime, _id: NodeId) -> RunOutcome {
        let Ok(mut step) = self.step.try_borrow_mut() else {
            return RunOutcome::Unchanged;
        };
        let result = step(rt);
        drop(step);
        let step = match result {
            Ok(s) => s,
            Err(e) => {
                let mut err = self.error.borrow_mut();
                let changed = err.as_ref() != Some(&e);
                *err = Some(e);
                return if changed {
                    RunOutcome::Changed
                } else {
                    RunOutcome::Unchanged
                };
            }
        };
        let had_error = self.error.borrow_mut().take().is_some();
        let mut items = self.items.borrow_mut();
        let mut log = self.log.borrow_mut();
        let first = !self.started.replace(true);
        let diffs = match step {
            Step::Diffs(diffs) => {
                let list = Rc::make_mut(&mut items);
                for d in &diffs {
                    // Operators emit valid diffs; a failure means a bug, so
                    // fall back to a reset rather than diverge.
                    if d.apply(list).is_err() {
                        break;
                    }
                }
                diffs
            }
            Step::Rebuild(new) => {
                let diffs = if first {
                    vec![VecDiff::Reset { items: new.clone() }]
                } else {
                    keyed_diff(&items, &new)
                };
                *items = Rc::new(new);
                diffs
            }
        };
        let changed = !diffs.is_empty() || had_error || first;
        for d in diffs {
            log.push(d);
        }
        if changed {
            RunOutcome::Changed
        } else {
            RunOutcome::Unchanged
        }
    }
}

/// A derived keyed collection (`xs.filter(…)`, `.map`, `.take`,
/// `.sort_by`). Copyable handle.
pub struct KeyedMemo<K, U> {
    id: NodeId,
    _t: PhantomData<fn() -> (K, U)>,
}

impl<K, U> Clone for KeyedMemo<K, U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K, U> Copy for KeyedMemo<K, U> {}
impl<K, U> fmt::Debug for KeyedMemo<K, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyedMemo({:?})", self.id)
    }
}

impl<K, U> KeyedMemo<K, U> {
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }
    /// Dispose.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
}

impl<K, U> KeyedSource<K, U> for KeyedMemo<K, U>
where
    K: Clone + Eq + Hash + 'static,
    U: Clone + PartialEq + 'static,
{
    fn snapshot(self, rt: &Runtime) -> Result<Snapshot<K, U>, Error> {
        rt.track(self.id);
        rt.update_if_necessary(self.id)?;
        rt.with_data::<DerivedData<K, U>, _>(self.id, |d| {
            if let Some(e) = d.error.borrow().clone() {
                return Err(e);
            }
            Ok(Snapshot {
                items: d.items.borrow().clone(),
                version: d.log.borrow().version(),
                log: d.log.clone(),
            })
        })?
    }
    fn node(self) -> NodeId {
        self.id
    }
}

/// Build a derived collection from an operator factory and tracked params.
fn derive<K, T, U, P, O, S>(
    rt: &Runtime,
    src: S,
    params: impl Fn(&Runtime) -> Result<P, Error> + 'static,
    make: impl Fn(&P) -> O + 'static,
) -> KeyedMemo<K, U>
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
    U: Clone + PartialEq + 'static,
    P: PartialEq + 'static,
    O: IncrementalOp<K, T, Out = U> + 'static,
    S: KeyedSource<K, T>,
{
    struct St<P, O, K, T> {
        params: P,
        op: O,
        /// Source log and version consumed (not the items, so the source
        /// can mutate in place).
        log: Rc<RefCell<DiffLog<K, T>>>,
        version: u64,
    }
    let mut state: Option<St<P, O, K, T>> = None;
    let step = move |rt: &Runtime| -> Result<Step<K, U>, Error> {
        let snap = src.snapshot(rt)?;
        let p = params(rt)?;
        if let Some(st) = state.as_mut()
            && st.params == p
            && Rc::ptr_eq(&st.log, &snap.log)
            && let Some(diffs) = snap.diffs_since(st.version)
        {
            let mut out = Vec::new();
            let mut ok = true;
            for d in &diffs {
                if st.op.apply(d, &mut out).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                st.version = snap.version;
                return Ok(Step::Diffs(out));
            }
        }
        // Rebuild: first run, params changed, log gap or a bad diff.
        let mut op = make(&p);
        let mut out = Vec::new();
        op.apply(
            &VecDiff::Reset {
                items: snap.items().to_vec(),
            },
            &mut out,
        )?;
        let items = match out.pop() {
            Some(VecDiff::Reset { items }) => items,
            _ => Vec::new(),
        };
        state = Some(St {
            params: p,
            op,
            log: snap.log.clone(),
            version: snap.version,
        });
        Ok(Step::Rebuild(items))
    };
    let id = rt.create_node(
        NodeKind::Collection,
        Color::Dirty,
        Some(Rc::new(DerivedData::<K, U> {
            step: RefCell::new(Box::new(step)),
            items: RefCell::new(Rc::new(Vec::new())),
            log: Rc::new(RefCell::new(DiffLog::new())),
            error: RefCell::new(None),
            started: std::cell::Cell::new(false),
        })),
    );
    KeyedMemo {
        id,
        _t: PhantomData,
    }
}

/// Incremental `filter`, `map`, `take` and `sort_by` on any keyed source.
///
/// The `_with` forms take a tracked `params` closure: the operator closure
/// itself stays pure and receives the params; when they change the result is
/// rebuilt and published as a keyed diff.
pub trait KeyedOps<K, T>: KeyedSource<K, T>
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
{
    /// `.filter(pred)` with reactive parameters.
    fn filter_with<P, F>(
        self,
        rt: &Runtime,
        params: impl Fn(&Runtime) -> Result<P, Error> + 'static,
        pred: F,
    ) -> KeyedMemo<K, T>
    where
        P: PartialEq + Clone + 'static,
        F: Fn(&P, &T) -> bool + 'static,
    {
        let pred = Rc::new(pred);
        derive(rt, self, params, move |p: &P| {
            let pred = pred.clone();
            let p = p.clone();
            Filter::new(move |_: &K, v: &T| pred(&p, v))
        })
    }

    /// `.filter(pred)`.
    fn filter<F>(self, rt: &Runtime, pred: F) -> KeyedMemo<K, T>
    where
        F: Fn(&T) -> bool + 'static,
    {
        self.filter_with(rt, |_| Ok(()), move |_: &(), v| pred(v))
    }

    /// `.map(f)` with reactive parameters; keys are kept.
    fn map_with<P, U, F>(
        self,
        rt: &Runtime,
        params: impl Fn(&Runtime) -> Result<P, Error> + 'static,
        f: F,
    ) -> KeyedMemo<K, U>
    where
        P: PartialEq + Clone + 'static,
        U: Clone + PartialEq + 'static,
        F: Fn(&P, &T) -> U + 'static,
    {
        let f = Rc::new(f);
        derive(rt, self, params, move |p: &P| {
            let f = f.clone();
            let p = p.clone();
            Map::new(move |_: &K, v: &T| f(&p, v))
        })
    }

    /// `.map(f)`.
    fn map<U, F>(self, rt: &Runtime, f: F) -> KeyedMemo<K, U>
    where
        U: Clone + PartialEq + 'static,
        F: Fn(&T) -> U + 'static,
    {
        self.map_with(rt, |_| Ok(()), move |_: &(), v| f(v))
    }

    /// `.take(n)` with a reactive `n`.
    fn take_with(
        self,
        rt: &Runtime,
        n: impl Fn(&Runtime) -> Result<usize, Error> + 'static,
    ) -> KeyedMemo<K, T> {
        derive(rt, self, n, |&n: &usize| Take::new(n))
    }

    /// `.take(n)`.
    fn take(self, rt: &Runtime, n: usize) -> KeyedMemo<K, T> {
        self.take_with(rt, move |_| Ok(n))
    }

    /// `.sort_by(cmp)` with reactive parameters; stable.
    fn sort_by_with<P, F>(
        self,
        rt: &Runtime,
        params: impl Fn(&Runtime) -> Result<P, Error> + 'static,
        cmp: F,
    ) -> KeyedMemo<K, T>
    where
        P: PartialEq + Clone + 'static,
        F: Fn(&P, &T, &T) -> std::cmp::Ordering + 'static,
    {
        let cmp = Rc::new(cmp);
        derive(rt, self, params, move |p: &P| {
            let cmp = cmp.clone();
            let p = p.clone();
            SortBy::new(move |a: &T, b: &T| cmp(&p, a, b))
        })
    }

    /// `.sort_by(cmp)`; stable.
    fn sort_by<F>(self, rt: &Runtime, cmp: F) -> KeyedMemo<K, T>
    where
        F: Fn(&T, &T) -> std::cmp::Ordering + 'static,
    {
        self.sort_by_with(rt, |_| Ok(()), move |_: &(), a, b| cmp(a, b))
    }
}

impl<K, T, S> KeyedOps<K, T> for S
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
    S: KeyedSource<K, T>,
{
}

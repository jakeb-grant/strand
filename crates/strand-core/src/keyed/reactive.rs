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
use std::cell::{Cell, RefCell};
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

    /// Run a mutation and publish the diffs it applied, even when it then
    /// failed part-way (so the log always matches the items).
    fn mutate(
        self,
        rt: &Runtime,
        f: impl FnOnce(&mut KeyedVec<K, T>) -> (Vec<VecDiff<K, T>>, Result<(), Error>),
    ) -> Result<(), Error> {
        rt.check_write_allowed(self.id)?;
        let (changed, r) = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let Ok(mut vec) = d.vec.try_borrow_mut() else {
                return (false, Err(Error::Reentrant));
            };
            let (diffs, r) = f(&mut vec);
            let changed = !diffs.is_empty();
            let mut log = d.log.borrow_mut();
            for diff in diffs {
                log.push(diff);
            }
            (changed, r)
        })?;
        if changed {
            rt.cell_changed(self.id);
        }
        r
    }

    /// `xs.push(v)`.
    pub fn push(self, rt: &Runtime, value: T) -> Result<(), Error> {
        self.mutate(rt, |v| one(v.push(value).map(Some)))
    }

    /// `xs.insert(i, v)`.
    pub fn insert(self, rt: &Runtime, index: usize, value: T) -> Result<(), Error> {
        self.mutate(rt, |v| one(v.insert(index, value).map(Some)))
    }

    /// `xs.remove_key(k)`.
    pub fn remove_key(self, rt: &Runtime, key: &K) -> Result<(), Error> {
        self.mutate(rt, |v| one(v.remove_key(key).map(Some)))
    }

    /// `xs.move(k, to)`.
    pub fn move_key(self, rt: &Runtime, key: &K, to: usize) -> Result<(), Error> {
        self.mutate(rt, |v| one(v.move_key(key, to)))
    }

    /// `xs.update(k, f)`.
    pub fn update(self, rt: &Runtime, key: &K, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        self.mutate(rt, |v| one(v.update(key, f)))
    }

    /// Replace the contents, keeping identity by key.
    pub fn replace_all(
        self,
        rt: &Runtime,
        values: impl IntoIterator<Item = T>,
    ) -> Result<(), Error> {
        self.mutate(rt, |v| match v.replace_all(values) {
            Ok(diffs) => (diffs, Ok(())),
            Err(e) => (Vec::new(), Err(e.into())),
        })
    }

    /// Apply diffs published by a service, in order. If one is malformed,
    /// the ones before it stay applied and are published (consumers and
    /// derived collections stay consistent with the items) and the error is
    /// returned; the rest of the batch is dropped.
    pub fn apply(self, rt: &Runtime, diffs: &[VecDiff<K, T>]) -> Result<(), Error> {
        self.mutate(rt, |v| {
            let mut applied = Vec::with_capacity(diffs.len());
            for d in diffs {
                if let Err(e) = v.apply(d) {
                    return (applied, Err(e.into()));
                }
                applied.push(d.clone());
            }
            (applied, Ok(()))
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

/// One optional diff from a `KeyedVec` mutation, as `mutate` wants it.
fn one<K, T>(
    r: Result<Option<VecDiff<K, T>>, super::KeyedError>,
) -> (Vec<VecDiff<K, T>>, Result<(), Error>) {
    match r {
        Ok(d) => (d.into_iter().collect(), Ok(())),
        Err(e) => (Vec::new(), Err(e.into())),
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
    started: Cell<bool>,
    /// Tells the step to rebuild from the source on its next call.
    force_rebuild: Rc<Cell<bool>>,
}

impl<K, U> NodeData for DerivedData<K, U>
where
    K: Clone + Eq + Hash + 'static,
    U: Clone + PartialEq + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
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
        let first = !self.started.replace(true);
        let diffs = match step {
            Step::Diffs(mut diffs) => {
                let mut items = self.items.borrow_mut();
                let list = Rc::make_mut(&mut items);
                let applied = diffs.iter().take_while(|d| d.apply(list).is_ok()).count();
                if applied == diffs.len() {
                    diffs
                } else {
                    // Operators emit valid diffs, so this is a bug; recover
                    // without diverging: keep what applied, then rebuild
                    // from the source and publish the keyed difference.
                    diffs.truncate(applied);
                    drop(items);
                    self.force_rebuild.set(true);
                    let rebuilt = match self.step.try_borrow_mut() {
                        Ok(mut step) => step(rt),
                        Err(_) => Err(Error::Reentrant),
                    };
                    let mut items = self.items.borrow_mut();
                    match rebuilt {
                        Ok(Step::Rebuild(new)) => {
                            diffs.extend(keyed_diff(&items, &new));
                            *items = Rc::new(new);
                        }
                        Ok(Step::Diffs(_)) => {}
                        Err(e) => *self.error.borrow_mut() = Some(e),
                    }
                    diffs
                }
            }
            Step::Rebuild(new) => {
                let mut items = self.items.borrow_mut();
                let diffs = if first {
                    vec![VecDiff::Reset { items: new.clone() }]
                } else {
                    keyed_diff(&items, &new)
                };
                *items = Rc::new(new);
                diffs
            }
        };
        let mut log = self.log.borrow_mut();
        let had_error = had_error || self.error.borrow().is_some();
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

// ----- from a plain list ---------------------------------------------------

impl Runtime {
    /// A keyed collection derived from a plain list expression:
    /// `for d in calendar.days(month) key d.date`, `for a in n.actions`.
    /// `f` is tracked like a memo; each new list is diffed by key against
    /// the previous one ([`keyed_diff`]) and published, so items keep their
    /// identity and the emitter gets `Insert`/`Remove`/`Move`/`Update`, not a
    /// `Reset`. It is a derived value, not a write: a list changing at
    /// 60 Hz (a spectrum, `cpu` history) is never rate-throttled. A list
    /// with two items under one key is an [`Error`] value
    /// ([`KeyedError::DuplicateKey`](super::KeyedError::DuplicateKey)), and
    /// the previous items are kept for the next good list to diff against.
    pub fn keyed_memo<K, T, KF, F>(&self, key_of: KF, f: F) -> KeyedMemo<K, T>
    where
        K: Clone + Eq + Hash + 'static,
        T: Clone + PartialEq + 'static,
        KF: Fn(&T) -> K + 'static,
        F: Fn(&Runtime) -> Result<Vec<T>, Error> + 'static,
    {
        let step = move |rt: &Runtime| -> Result<Step<K, T>, Error> {
            let values = f(rt)?;
            let mut seen = std::collections::HashSet::with_capacity(values.len());
            let mut items = Vec::with_capacity(values.len());
            for v in values {
                let k = key_of(&v);
                if !seen.insert(k.clone()) {
                    return Err(super::KeyedError::DuplicateKey.into());
                }
                items.push((k, v));
            }
            Ok(Step::Rebuild(items))
        };
        let id = self.create_node(
            NodeKind::Collection,
            Color::Dirty,
            Some(Rc::new(DerivedData::<K, T> {
                step: RefCell::new(Box::new(step)),
                items: RefCell::new(Rc::new(Vec::new())),
                log: Rc::new(RefCell::new(DiffLog::new())),
                error: RefCell::new(None),
                started: Cell::new(false),
                force_rebuild: Rc::new(Cell::new(false)),
            })),
        );
        KeyedMemo {
            id,
            _t: PhantomData,
        }
    }
}

impl<T: Clone + PartialEq + 'static> crate::Memo<Vec<T>> {
    /// This list as a keyed collection (`for x in xs key k`); see
    /// [`Runtime::keyed_memo`].
    pub fn keyed<K>(self, rt: &Runtime, key_of: impl Fn(&T) -> K + 'static) -> KeyedMemo<K, T>
    where
        K: Clone + Eq + Hash + 'static,
    {
        rt.keyed_memo(key_of, move |rt| self.get(rt))
    }
}

impl<T: Clone + PartialEq + 'static> crate::AsyncMemo<Vec<T>> {
    /// The loaded list as a keyed collection (`for h in hits key h.id`): the
    /// value kept while a newer request is pending or after an error, empty
    /// before the first result. See [`Runtime::keyed_memo`].
    pub fn keyed<K>(self, rt: &Runtime, key_of: impl Fn(&T) -> K + 'static) -> KeyedMemo<K, T>
    where
        K: Clone + Eq + Hash + 'static,
    {
        rt.keyed_memo(key_of, move |rt| {
            Ok(self.get(rt)?.value().cloned().unwrap_or_default())
        })
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
    let force_rebuild = Rc::new(Cell::new(false));
    let force = force_rebuild.clone();
    let step = move |rt: &Runtime| -> Result<Step<K, U>, Error> {
        if force.replace(false) {
            state = None;
        }
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
        rt.bump(|s| s.rebuilds += 1);
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
            started: Cell::new(false),
            force_rebuild,
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

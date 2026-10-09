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
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::rc::Rc;

use foldhash::HashSetExt;

use super::ops::{Filter, IncrementalOp, Map, SortBy, Take};
use super::{KeyedVec, VecDiff, keyed_diff};
use crate::echo::{EchoState, Generation, Verdict, peek_state, with_state};
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

/// A throttled handler's held copy of a collection: the list it started
/// from and the list with its changes.
struct Held<K, T> {
    base: KeyedVec<K, T>,
    work: KeyedVec<K, T>,
}

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
    ///
    /// Writes from graph-triggered handlers go through the 30 writes/s
    /// guard like any cell write (see [`crate::rate`]). A throttled handler
    /// keeps a held copy of the list together with the list it started
    /// from: its later operations apply to that copy (read-your-writes, so
    /// errors such as a duplicate key are reported at once), and when its
    /// window has room the copy's changes land as one keyed diff. A held
    /// copy is a set of changes, not a latest value: a write that goes
    /// through meanwhile (an input handler, a service batch, another
    /// handler's landing) does not supersede it; the held changes are
    /// re-applied by key onto the new list ([`super::rebase`]), so no
    /// `push` is lost. Changes that no longer apply (inserting a key the
    /// other write also inserted, updating or moving an item it removed)
    /// are skipped and reported as [`crate::Diagnostic::KeyedConflict`].
    fn mutate(
        self,
        rt: &Runtime,
        f: impl FnOnce(&mut KeyedVec<K, T>) -> (Vec<VecDiff<K, T>>, Result<(), Error>),
    ) -> Result<(), Error> {
        rt.check_write_allowed(self.id)?;
        rt.note_write(self.id);
        let held = rt.take_deferred::<Held<K, T>>(self.id);
        if held.is_none() && rt.rate_would_pass(self.id) {
            // In place; counted only if something changed.
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
                rt.rate_gate(self.id);
                rt.cell_changed(self.id);
            }
            return r;
        }
        let had_held = held.is_some();
        let mut held = match held {
            Some(h) => h,
            None => {
                let live = self.get_untracked(rt)?;
                Held {
                    base: live.clone(),
                    work: live,
                }
            }
        };
        let (diffs, r) = f(&mut held.work);
        if diffs.is_empty() && !had_held {
            return r;
        }
        // An operation that changed nothing is not an attempt: a held copy
        // just waits for its turn again.
        if !diffs.is_empty() && rt.rate_gate(self.id) {
            let writer = rt.current_writer().unwrap_or_default();
            self.land(rt, writer, held)?;
        } else {
            self.hold(rt, held);
        }
        r
    }

    /// Queue a held copy until the writer's window has room.
    fn hold(self, rt: &Runtime, held: Held<K, T>) {
        let writer = rt.current_writer().unwrap_or_default();
        let rebase: crate::rate::Rebase = Rc::new(move |rt: &Runtime, writer, v: Box<dyn Any>| {
            match v.downcast::<Held<K, T>>() {
                Ok(held) => match self.rebased(rt, writer, *held) {
                    Ok(h) => Box::new(h),
                    Err(h) => Box::new(h),
                },
                Err(v) => v,
            }
        });
        rt.defer_write(
            self.id,
            Some(Box::new(held)),
            Box::new(move |rt: &Runtime, held| {
                if let Some(Ok(held)) = held.map(|h| h.downcast::<Held<K, T>>()) {
                    let _ = self.land(rt, writer, *held);
                }
            }),
            Some(rebase),
        );
    }

    /// `held` re-based onto the live list (another write went through).
    /// `Err` gives it back unchanged when the cell is gone.
    fn rebased(
        self,
        rt: &Runtime,
        writer: NodeId,
        held: Held<K, T>,
    ) -> Result<Held<K, T>, Held<K, T>> {
        let Ok(live) = self.get_untracked(rt) else {
            return Err(held);
        };
        if live.same_items(&held.base) {
            return Ok(held);
        }
        let (items, lost) = super::rebase(live.items(), held.base.items(), held.work.items());
        if lost > 0 {
            rt.diagnose(crate::Diagnostic::KeyedConflict {
                cell: self.id,
                writer,
                skipped: lost,
            });
        }
        match live.with_items(items) {
            Some(work) => Ok(Held { base: live, work }),
            None => Err(held),
        }
    }

    /// Land a held copy: re-apply its changes onto the live list (a plain
    /// replacement when nothing else wrote since it started) and publish
    /// the keyed diff.
    fn land(self, rt: &Runtime, writer: NodeId, held: Held<K, T>) -> Result<(), Error> {
        let (changed, lost) = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let Ok(mut vec) = d.vec.try_borrow_mut() else {
                return Err(Error::Reentrant);
            };
            let (new, lost) = if vec.same_items(&held.base) {
                (held.work, 0)
            } else {
                let (items, lost) =
                    super::rebase(vec.items(), held.base.items(), held.work.items());
                match vec.with_items(items) {
                    Some(v) => (v, lost),
                    None => return Err(super::KeyedError::DuplicateKey.into()),
                }
            };
            let diffs = keyed_diff(vec.items(), new.items());
            *vec = new;
            let changed = !diffs.is_empty();
            let mut log = d.log.borrow_mut();
            for diff in diffs {
                log.push(diff);
            }
            Ok((changed, lost))
        })??;
        if lost > 0 {
            rt.diagnose(crate::Diagnostic::KeyedConflict {
                cell: self.id,
                writer,
                skipped: lost,
            });
        }
        if changed {
            rt.cell_changed(self.id);
        }
        Ok(())
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

    /// A write made by live reload (a declared default adopted, a
    /// persisted list handed over, `@reset` applied at reload): replace
    /// the contents by key, like [`KeyedSignal::replace_all`], but as
    /// [`Signal::set_reloaded`](crate::Signal::set_reloaded) does for a
    /// plain cell: not rate-gated (it is not a handler's write; a held
    /// throttled copy is re-based onto it), and every `on change` handler
    /// downstream takes the new contents as its baseline instead of
    /// firing. Consumers get a keyed diff. Returns whether anything
    /// changed.
    pub fn replace_all_reloaded(
        self,
        rt: &Runtime,
        values: impl IntoIterator<Item = T>,
    ) -> Result<bool, Error> {
        rt.check_write_allowed(self.id)?;
        let changed = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let mut vec = d.vec.try_borrow_mut().map_err(|_| Error::Reentrant)?;
            let diffs = vec.replace_all(values)?;
            let changed = !diffs.is_empty();
            let mut log = d.log.borrow_mut();
            for diff in diffs {
                log.push(diff);
            }
            Ok::<bool, Error>(changed)
        })??;
        if changed {
            rt.cell_changed(self.id);
            rt.rebaseline_from(self.id);
        }
        Ok(changed)
    }

    /// The starting contents of a cell nothing has read yet (a persisted
    /// collection's restored list): no diff, no change.
    pub(crate) fn init_value(self, rt: &Runtime, value: KeyedVec<K, T>) {
        let _ = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            if let Ok(mut vec) = d.vec.try_borrow_mut() {
                *vec = value;
                *d.log.borrow_mut() = DiffLog::new();
            }
        });
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
    ///
    /// Holding the clone across a write to this cell makes that write copy
    /// the items and the key index (O(n)); to read without holding a copy,
    /// use [`KeyedSignal::with`], [`KeyedSignal::with_untracked`] or
    /// [`KeyedSignal::get_key`].
    pub fn get_untracked(self, rt: &Runtime) -> Result<KeyedVec<K, T>, Error> {
        self.with_untracked(rt, KeyedVec::clone)
    }

    /// Borrow the list (tracked) without cloning it: the VM's read path
    /// (`xs.len`, `xs[k]`, a loop over `xs` inside a handler).
    pub fn with<R>(self, rt: &Runtime, f: impl FnOnce(&KeyedVec<K, T>) -> R) -> Result<R, Error> {
        rt.track(self.id);
        self.with_untracked(rt, f)
    }

    /// [`KeyedSignal::with`] without tracking (handler bodies).
    pub fn with_untracked<R>(
        self,
        rt: &Runtime,
        f: impl FnOnce(&KeyedVec<K, T>) -> R,
    ) -> Result<R, Error> {
        rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let vec = d.vec.try_borrow().map_err(|_| Error::Reentrant)?;
            Ok(f(&vec))
        })?
    }

    /// The item with `key` (cloned), untracked: no copy of the list.
    pub fn get_key(self, rt: &Runtime, key: &K) -> Result<Option<T>, Error> {
        self.with_untracked(rt, |v| v.get(key).cloned())
    }
}

/// The echo bookkeeping of a keyed list's items written through
/// [`KeyedSignal::write_item_tagged`]: one tag counter for the list, a
/// state per item with writes in flight.
struct ItemEchoes<K, T> {
    next: u64,
    items: HashMap<K, EchoState<T>>,
    /// The order item writes were made in (held or not): of two writes
    /// of one item, the later one wins whichever lands first.
    made: u64,
    /// An item write is landing: the list's change is its own, and the
    /// held writes it supersedes are dropped by their order after it.
    landing: bool,
}

impl<K, T> Default for ItemEchoes<K, T> {
    fn default() -> Self {
        ItemEchoes {
            next: 1,
            items: HashMap::new(),
            made: 0,
            landing: false,
        }
    }
}

/// Passes an item write on: the item's index, its new value, its tag.
type ItemSend<T> = Box<dyn FnOnce(&Runtime, usize, &T, Generation)>;

/// One held item write.
struct HeldItem<K, T> {
    key: K,
    value: T,
    /// The item when the write was made: another change of it since
    /// supersedes the write.
    base: T,
    /// When it was made ([`ItemEchoes::made`]).
    made: u64,
    send: ItemSend<T>,
}

/// The item writes a throttled handler holds: the latest per item.
struct HeldItems<K, T>(Vec<HeldItem<K, T>>);

/// Service-backed keyed lists: optimistic item writes and their echoes
/// (the keyed counterpart of [`crate::Signal::write_tagged`] and
/// [`crate::Signal::receive`]).
impl<K, T> KeyedSignal<K, T>
where
    K: Clone + Eq + Hash + 'static,
    T: Clone + PartialEq + 'static,
{
    /// A local write of the item with `key` (its whole new value),
    /// destined for a service: `s.volume = 0.5` for `s` in `audio.sinks`.
    /// Like [`crate::Signal::write_tagged`]: when the write-rate guard
    /// lets it through, the item is updated at once, the write is
    /// remembered as pending for that item, and `send` is called with the
    /// item's index, its value and the write's tag (returned). A throttled
    /// handler's item writes are held, the latest per item, and land
    /// (each with its `send`) when its window has room: `Ok(None)`.
    /// Writing an item the list does not hold is an error.
    ///
    /// Latest write wins, per item: an item write that lands (even one
    /// that changes nothing) drops the writes of that item other handlers
    /// hold that were made before it, and any other change of the item
    /// (another write of the list, a service's report) drops every held
    /// write of it. Held writes of other items are kept.
    pub fn write_item_tagged(
        self,
        rt: &Runtime,
        key: K,
        value: T,
        send: impl FnOnce(&Runtime, usize, &T, Generation) + 'static,
    ) -> Result<Option<Generation>, Error> {
        rt.check_write_allowed(self.id)?;
        rt.note_write(self.id);
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        if !self.with_untracked(rt, |v| v.contains_key(&key))? {
            return Err(super::KeyedError::MissingKey.into());
        }
        let base = self
            .with_untracked(rt, |v| v.get(&key).cloned())?
            .ok_or(super::KeyedError::MissingKey)?;
        let made = with_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
            s.made += 1;
            s.made
        });
        let mut held = rt
            .take_deferred::<HeldItems<K, T>>(self.id)
            .map_or_else(Vec::new, |h| h.0);
        held.retain(|h| h.key != key);
        held.push(HeldItem {
            key: key.clone(),
            value,
            base,
            made,
            send: Box::new(send),
        });
        if rt.rate_gate(self.id) {
            let mut mine = Ok(None);
            for h in held {
                let k = h.key.clone();
                let r = self.commit_item(rt, h);
                if k == key {
                    mine = r.map(Some);
                }
            }
            mine
        } else {
            // Another change of a held item supersedes its write; a change
            // made by an item write landing is sorted out by order there.
            let rebase: crate::rate::Rebase = Rc::new(move |rt: &Runtime, _, v: Box<dyn Any>| {
                let landing =
                    peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| s.landing).unwrap_or(false);
                match v.downcast::<HeldItems<K, T>>() {
                    Ok(mut h) if !landing => {
                        let _ = self.with_untracked(rt, |v| {
                            h.0.retain(|h| v.get(&h.key) == Some(&h.base));
                        });
                        h
                    }
                    Ok(h) => h,
                    Err(v) => v,
                }
            });
            rt.defer_write(
                self.id,
                Some(Box::new(HeldItems(held))),
                Box::new(move |rt: &Runtime, h| {
                    if let Some(Ok(h)) = h.map(|h| h.downcast::<HeldItems<K, T>>()) {
                        for h in h.0 {
                            let _ = self.commit_item(rt, h);
                        }
                    }
                }),
                Some(rebase),
            );
            Ok(None)
        }
    }

    /// Apply one item write, remember it, send it; the writes of that
    /// item other handlers hold that were made before it are dropped.
    fn commit_item(self, rt: &Runtime, held: HeldItem<K, T>) -> Result<Generation, Error> {
        let HeldItem {
            key,
            value,
            made,
            send,
            ..
        } = held;
        let (index, changed) = rt.with_data::<CellData<K, T>, _>(self.id, |d| {
            let mut vec = d.vec.try_borrow_mut().map_err(|_| Error::Reentrant)?;
            let index = vec.index_of(&key).ok_or(super::KeyedError::MissingKey)?;
            let diff = vec.update(&key, |t| *t = value.clone())?;
            let changed = diff.is_some();
            if let Some(diff) = diff {
                d.log.borrow_mut().push(diff);
            }
            Ok::<_, Error>((index, changed))
        })??;
        if changed {
            with_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| s.landing = true);
            rt.cell_changed(self.id);
            with_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| s.landing = false);
        }
        rt.edit_deferred::<HeldItems<K, T>>(self.id, |h| {
            h.0.retain(|h| h.key != key || h.made > made);
            !h.0.is_empty()
        });
        let g = with_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
            let g = Generation(s.next);
            s.next += 1;
            s.items.entry(key).or_default().record(g, value.clone());
            g
        });
        send(rt, index, &value, g);
        Ok(g)
    }

    /// Local writes of the item with `key` the service has not answered.
    pub fn pending_item_writes(self, rt: &Runtime, key: &K) -> usize {
        peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
            s.items.get(key).map_or(0, EchoState::pending_len)
        })
        .unwrap_or(0)
    }

    /// Forget every item's pending writes (the service run they went to
    /// ended without answering them). Tags keep counting up.
    pub fn forget_echoes(self, rt: &Runtime) {
        peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| s.items.clear());
    }

    /// Replace the items of `items` (a boot report) that have local
    /// writes pending with the list's own: the report predates those
    /// writes, whose answers come next.
    pub fn keep_pending_items(self, rt: &Runtime, items: &mut [T]) -> Result<(), Error> {
        let Some(keys) = peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
            s.items
                .iter()
                .filter(|(_, st)| st.pending_len() > 0)
                .map(|(k, _)| k.clone())
                .collect::<Vec<K>>()
        }) else {
            return Ok(());
        };
        if keys.is_empty() {
            return Ok(());
        }
        self.with_untracked(rt, |v| {
            let key_of = v.key_fn();
            for item in items.iter_mut() {
                let k = key_of(item);
                if keys.contains(&k)
                    && let Some(local) = v.get(&k)
                {
                    *item = local.clone();
                }
            }
        })
    }

    /// Apply diffs a service reported, `echo_of` the tag of the local
    /// write they answer (if they do). An item's update that is the echo
    /// of a pending write of it is dropped (by tag, or by value for an
    /// untagged report), so a slider dragged over a sink's volume never
    /// snaps back; an update matching no pending write is an outside
    /// change and wins; a `Reset` keeps the local item where its report
    /// is such an echo. Updates that change nothing are dropped. Returns
    /// the diffs applied.
    pub fn receive_items(
        self,
        rt: &Runtime,
        diffs: &[VecDiff<K, T>],
        echo_of: Option<Generation>,
    ) -> Result<Vec<VecDiff<K, T>>, Error> {
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        // The local items of keys with writes in flight (a `Reset` keeps
        // them where its report is an echo).
        let locals: HashMap<K, T> = peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
            s.items.keys().cloned().collect::<Vec<K>>()
        })
        .map(|keys| {
            self.with_untracked(rt, |v| {
                keys.into_iter()
                    .filter_map(|k| v.get(&k).cloned().map(|t| (k, t)))
                    .collect()
            })
        })
        .transpose()?
        .unwrap_or_default();
        let kept: Vec<VecDiff<K, T>> = if locals.is_empty()
            && peek_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| s.items.is_empty())
                .unwrap_or(true)
        {
            diffs.to_vec()
        } else {
            with_state::<ItemEchoes<K, T>, _>(rt, self.id, |s| {
                let next = s.next;
                let mut out = Vec::with_capacity(diffs.len());
                // Whether the report of `key` is the echo of its writes.
                let echo = |items: &mut HashMap<K, EchoState<T>>, key: &K, value: &T| -> bool {
                    let Some(st) = items.get_mut(key) else {
                        return false;
                    };
                    let tag = echo_of.filter(|g| st.knows(*g));
                    let v = st.verdict(value, tag, next);
                    if st.is_idle() {
                        items.remove(key);
                    }
                    v == Verdict::Echo
                };
                for d in diffs {
                    match d {
                        VecDiff::Update { key, value, .. } => {
                            if !echo(&mut s.items, key, value) {
                                out.push(d.clone());
                            }
                        }
                        VecDiff::Reset { items } => {
                            let items = items
                                .iter()
                                .map(|(k, v)| {
                                    let v = match locals.get(k) {
                                        Some(local) if echo(&mut s.items, k, v) => local.clone(),
                                        _ => v.clone(),
                                    };
                                    (k.clone(), v)
                                })
                                .collect();
                            out.push(VecDiff::Reset { items });
                        }
                        VecDiff::Insert { key, .. } | VecDiff::Remove { key, .. } => {
                            s.items.remove(key);
                            out.push(d.clone());
                        }
                        VecDiff::Move { .. } => out.push(d.clone()),
                    }
                }
                out
            })
        };
        // An update to what the item already is changes nothing.
        let kept: Vec<VecDiff<K, T>> = self.with_untracked(rt, |v| {
            kept.into_iter()
                .filter(|d| match d {
                    VecDiff::Update { key, value, .. } => v.get(key) != Some(value),
                    _ => true,
                })
                .collect()
        })?;
        self.apply(rt, &kept)?;
        Ok(kept)
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

impl<K, U> KeyedMemo<K, U>
where
    K: Clone + Eq + Hash + 'static,
    U: Clone + PartialEq + 'static,
{
    /// Borrow the items (tracked, brought up to date) without copying
    /// them: the VM's read path for `for n in shown`, `shown.len`.
    pub fn with<R>(self, rt: &Runtime, f: impl FnOnce(&[(K, U)]) -> R) -> Result<R, Error> {
        let snap = self.snapshot(rt)?;
        Ok(f(snap.items()))
    }

    /// [`KeyedMemo::with`] without tracking (handler bodies).
    pub fn with_untracked<R>(
        self,
        rt: &Runtime,
        f: impl FnOnce(&[(K, U)]) -> R,
    ) -> Result<R, Error> {
        rt.untrack(|rt| self.with(rt, f))
    }

    /// The item with `key` (cloned), untracked. A derived collection keeps
    /// no key index (its output is patched per diff, and an index would
    /// double its upkeep for a read most lists never make), so this is a
    /// scan, O(n): about 1 µs per 1,000 rows. Selecting by key in a
    /// filtered list once per keypress is well within budget; a hot loop
    /// that looks up many keys should read [`KeyedMemo::with`] once.
    pub fn get_key(self, rt: &Runtime, key: &K) -> Result<Option<U>, Error> {
        self.with_untracked(rt, |items| {
            items.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        })
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
            let mut seen = foldhash::HashSet::with_capacity(values.len());
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

/// More source diffs than this in one step, and at least a quarter of the
/// source length, rebuild instead: operators cost up to O(n) per diff
/// (`sort_by` index bookkeeping), a rebuild O(n log n) plus an O(n) keyed
/// diff of the output (a launcher query that reshuffles 2,000 rows).
const BULK_DIFFS: u64 = 128;

fn is_bulk(pending: u64, len: usize) -> bool {
    pending > BULK_DIFFS && pending.saturating_mul(4) > len as u64
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
            && !is_bulk(snap.version.saturating_sub(st.version), snap.len())
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
        // Rebuild: first run, params changed, log gap, a bulk change or a
        // bad diff.
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

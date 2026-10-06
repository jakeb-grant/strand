//! The store contract: what `#[derive(Store)]` generates and what the
//! runtime and the language side rely on.
//!
//! A service's state is a plain struct (`Send`). The derive gives it
//!
//! - a **patch** enum ([`Store::Patch`]): one variant per field holding
//!   the field's new value, one per keyed list holding its `VecDiff`s,
//!   one per event holding its payload. The service thread sends patches
//!   (in [`Envelope`](crate::cx::Envelope)s, one per update) and never
//!   touches logic-thread cells;
//! - the **cells** ([`Store::Cells`], built on the logic thread from
//!   `strand-core`): a `Signal<T>` per field, a `KeyedSignal` per keyed
//!   list, an `EventQueue<T>` per event. [`Cells::apply`] applies a patch:
//!   a field's report goes through `Signal::receive`, so the echo of a
//!   local write (tagged by `write_tagged`) is ignored; a first report is
//!   applied as a boot value (`set_reloaded`: `on change` takes it as its
//!   baseline).
//!
//! The language side (the binary's `ServiceHost` adapter) reads the cells
//! by field index as [`Data`] ([`Cells::read`], tracked) and hears of
//! keyed diffs and events as [`Applied`] values.

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use strand_core::{Error, Generation, KeyedVec, NodeId, Runtime, VecDiff, keyed_diff};

use crate::data::{Data, ToData};

/// What a store's field is, as the derive saw it.
#[derive(Clone, Copy)]
pub struct FieldInfo {
    /// The field's name, as the schema spells it.
    pub name: &'static str,
    /// Its schema type (`float`, `[Workspace]`, `text?`).
    pub ty: fn() -> String,
    /// Marked `#[store(rw)]`: written by the language (`<->`,
    /// assignment).
    pub rw: bool,
    /// A `#[store(keyed)]` list: published as a keyed collection.
    pub keyed: bool,
    /// Its `///` doc.
    pub doc: &'static str,
}

impl fmt::Debug for FieldInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldInfo")
            .field("name", &self.name)
            .field("ty", &(self.ty)())
            .field("rw", &self.rw)
            .field("keyed", &self.keyed)
            .finish()
    }
}

/// An event of a store (`Event<T>` field).
#[derive(Clone, Copy, Debug)]
pub struct EventInfo {
    pub name: &'static str,
    /// How many arguments it carries.
    pub arity: usize,
    pub doc: &'static str,
}

/// What a patch changes: field or event index (into [`Store::FIELDS`] /
/// [`Store::EVENTS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Field(usize),
    Event(usize),
}

/// A patch type (generated).
pub trait Patch: Clone + fmt::Debug + Send + 'static {
    fn target(&self) -> Target;
}

/// How a patch is applied on the logic thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// A boot value (the service's first report after it started):
    /// readers update, `on change` handlers take it as their baseline
    /// ("never at boot").
    Initial,
    /// A report: an outside change, or the answer to a local write
    /// (`Some(generation)`); the echo of a pending write is ignored.
    Report(Option<Generation>),
}

/// What applying a patch did that a dynamic consumer (the language side)
/// mirrors: plain fields need nothing (it reads the cells).
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    /// Keyed field `field` changed by these diffs (keys and items as
    /// [`Data`]).
    Keyed {
        field: usize,
        diffs: Vec<VecDiff<Data, Data>>,
    },
    /// Event `event` fired with these arguments.
    Event { event: usize, args: Vec<Data> },
}

/// A service's state (`#[derive(Store)]`).
pub trait Store: Clone + PartialEq + Default + Send + 'static {
    type Patch: Patch;
    type Cells: Cells<Self>;
    /// Its fields, in declaration order (events excluded).
    const FIELDS: &'static [FieldInfo];
    /// Its events, in declaration order.
    const EVENTS: &'static [EventInfo];
    /// The patches turning `old` into `new`: one per changed field, the
    /// `VecDiff`s of a changed keyed list. Events are never diffed.
    fn diff(old: &Self, new: &Self, out: &mut Vec<Self::Patch>);
    /// Apply `patch` to a plain copy (the service side's own copy).
    fn apply(&mut self, patch: &Self::Patch);
    /// The patch setting field `field` to its value in `self` (a keyed
    /// list as one `Reset`); `None` for no such field.
    fn field_patch(&self, field: usize) -> Option<Self::Patch>;
}

/// Passes a local write on to the service, with its generation.
pub type SendWrite = Box<dyn FnOnce(&Runtime, Generation)>;

/// A store's cells on the logic thread (generated).
pub trait Cells<S: Store>: fmt::Debug + 'static {
    /// The cells, holding `initial`; `service` names them for debugging
    /// (`cpu.usage`).
    fn new(rt: &Runtime, service: &str, initial: &S) -> Self;
    /// Apply one patch; see [`How`].
    fn apply(&self, rt: &Runtime, patch: &S::Patch, how: How) -> Result<Option<Applied>, Error>;
    /// The current state, untracked.
    fn snapshot(&self, rt: &Runtime) -> Result<S, Error>;
    /// Field `field` as [`Data`], tracked: a binding reading it depends on
    /// exactly that field.
    fn read(&self, rt: &Runtime, field: usize) -> Result<Data, Error>;
    /// The core nodes behind field `field`.
    fn ids(&self, field: usize) -> Vec<NodeId>;
    /// A local write of field `field` (its whole new value): applied at
    /// once with `write_tagged`, and `send` (called with the write's
    /// generation when the write-rate guard lets it through) passes it to
    /// the service.
    fn write(&self, rt: &Runtime, field: usize, value: &Data, send: SendWrite)
    -> Result<(), Error>;
    /// A keyed field's items as [`Data`], untracked.
    fn keyed_items(&self, rt: &Runtime, field: usize) -> Result<Vec<Data>, Error>;
    /// Dispose every cell.
    fn dispose(&self, rt: &Runtime);
}

/// An event field's marker type: `received: Event<Notification>`. It
/// holds nothing (events are not state); the service emits with
/// [`Cx::emit`](crate::Cx::emit).
pub struct Event<T>(PhantomData<fn() -> T>);

impl<T> Default for Event<T> {
    fn default() -> Self {
        Event(PhantomData)
    }
}

impl<T> Clone for Event<T> {
    fn clone(&self) -> Self {
        Event(PhantomData)
    }
}

impl<T> PartialEq for Event<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<T> fmt::Debug for Event<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Event")
    }
}

/// A record kept in a keyed list (`#[derive(Data)] #[data(key = id)]`).
pub trait Keyed {
    type Key: Clone + Eq + Hash + fmt::Debug + Send + ToData + 'static;
    fn key(&self) -> Self::Key;
}

/// A [`KeyedVec`] of `items` keyed by [`Keyed::key`] (a repeated key
/// keeps its first item).
pub fn keyed_vec_of<T>(items: Vec<T>) -> KeyedVec<T::Key, T>
where
    T: Keyed + Clone + PartialEq + 'static,
{
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<T> = items.into_iter().filter(|t| seen.insert(t.key())).collect();
    KeyedVec::from_values(|t: &T| t.key(), unique)
        .unwrap_or_else(|_| KeyedVec::new(|t: &T| t.key()))
}

/// The keyed diffs turning `old` into `new`.
pub fn keyed_changes<T>(old: &[T], new: &[T]) -> Vec<VecDiff<T::Key, T>>
where
    T: Keyed + Clone + PartialEq,
{
    let pairs = |v: &[T]| -> Vec<(T::Key, T)> { v.iter().map(|t| (t.key(), t.clone())).collect() };
    keyed_diff(&pairs(old), &pairs(new))
}

/// Apply keyed diffs to a plain list (a diff that does not apply leaves
/// the list as it was before it).
pub fn apply_keyed<T>(items: &mut Vec<T>, diffs: &[VecDiff<T::Key, T>])
where
    T: Keyed + Clone,
{
    let mut pairs: Vec<(T::Key, T)> = items.iter().map(|t| (t.key(), t.clone())).collect();
    for d in diffs {
        if d.apply(&mut pairs).is_err() {
            break;
        }
    }
    *items = pairs.into_iter().map(|(_, t)| t).collect();
}

/// A typed keyed diff as [`Data`].
pub fn diff_data<K: ToData, T: ToData>(d: &VecDiff<K, T>) -> VecDiff<Data, Data> {
    match d {
        VecDiff::Reset { items } => VecDiff::Reset {
            items: items
                .iter()
                .map(|(k, v)| (k.to_data(), v.to_data()))
                .collect(),
        },
        VecDiff::Insert { index, key, value } => VecDiff::Insert {
            index: *index,
            key: key.to_data(),
            value: value.to_data(),
        },
        VecDiff::Update { index, key, value } => VecDiff::Update {
            index: *index,
            key: key.to_data(),
            value: value.to_data(),
        },
        VecDiff::Remove { index, key } => VecDiff::Remove {
            index: *index,
            key: key.to_data(),
        },
        VecDiff::Move { from, to, key } => VecDiff::Move {
            from: *from,
            to: *to,
            key: key.to_data(),
        },
    }
}

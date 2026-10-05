//! Typed handles: `Signal` (state), `Memo` (lazy derived) and `Effect`
//! (edge observer).

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::rc::Rc;

use crate::error::Error;
use crate::runtime::{Color, NodeData, NodeId, NodeKind, RunOutcome, Runtime};

macro_rules! typed_handle {
    ($name:ident, $what:literal) => {
        #[doc = $what]
        pub struct $name<T> {
            pub(crate) id: NodeId,
            _t: PhantomData<fn() -> T>,
        }

        impl<T> $name<T> {
            pub(crate) fn from_id(id: NodeId) -> Self {
                Self {
                    id,
                    _t: PhantomData,
                }
            }
            /// The node id (for [`Runtime::watch`], names and disposal).
            pub fn id(self) -> NodeId {
                self.id
            }
            /// Dispose this node. Later reads return [`Error::Disposed`].
            pub fn dispose(self, rt: &Runtime) {
                rt.dispose(self.id);
            }
        }

        impl<T> Clone for $name<T> {
            fn clone(&self) -> Self {
                *self
            }
        }
        impl<T> Copy for $name<T> {}
        impl<T> PartialEq for $name<T> {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }
        impl<T> Eq for $name<T> {}
        impl<T> Hash for $name<T> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.id.hash(state);
            }
        }
        impl<T> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.id)
            }
        }
    };
}

typed_handle!(Signal, "Writable state (`state x = 0`). Copyable handle.");
typed_handle!(
    Memo,
    "A lazy derived value (`let`, props). Copyable handle."
);

pub(crate) struct SignalData<T> {
    pub(crate) value: RefCell<T>,
}

impl<T: Clone + PartialEq + 'static> NodeData for SignalData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn clone_value(&self) -> Option<Box<dyn Any>> {
        Some(Box::new(self.value.try_borrow().ok()?.clone()))
    }
    fn value_eq(&self, other: &dyn Any) -> bool {
        match (other.downcast_ref::<T>(), self.value.try_borrow()) {
            (Some(o), Ok(v)) => *v == *o,
            _ => false,
        }
    }
}

type ComputeFn<T> = Box<dyn Fn(&Runtime) -> Result<T, Error>>;
type EffectFn = Box<dyn FnMut(&Runtime) -> Result<(), Error>>;

pub(crate) struct MemoData<T> {
    f: ComputeFn<T>,
    value: RefCell<Option<Result<T, Error>>>,
}

impl<T: Clone + PartialEq + 'static> NodeData for MemoData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn clone_value(&self) -> Option<Box<dyn Any>> {
        Some(Box::new(self.value.try_borrow().ok()?.clone()))
    }
    fn value_eq(&self, other: &dyn Any) -> bool {
        match (
            other.downcast_ref::<Option<Result<T, Error>>>(),
            self.value.try_borrow(),
        ) {
            (Some(o), Ok(v)) => *v == *o,
            _ => false,
        }
    }
    fn run(&self, rt: &Runtime, _id: NodeId) -> RunOutcome {
        let new = (self.f)(rt);
        let Ok(mut slot) = self.value.try_borrow_mut() else {
            return RunOutcome::Unchanged;
        };
        if slot.as_ref() == Some(&new) {
            RunOutcome::Unchanged
        } else {
            *slot = Some(new);
            RunOutcome::Changed
        }
    }
}

struct EffectData {
    f: RefCell<EffectFn>,
}

impl NodeData for EffectData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn run(&self, rt: &Runtime, _id: NodeId) -> RunOutcome {
        let Ok(mut f) = self.f.try_borrow_mut() else {
            return RunOutcome::Failed(Error::Reentrant);
        };
        match f(rt) {
            Ok(()) => RunOutcome::Unchanged,
            Err(e) => RunOutcome::Failed(e),
        }
    }
}

/// An observer at the edge of the graph. Runs at the end of the tick in
/// which one of its dependencies changed, at most once per tick unless a
/// later effect writes to it again.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Effect {
    pub(crate) id: NodeId,
}

impl Effect {
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }
    /// Stop the effect and dispose what it owns.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
}

impl Runtime {
    /// Create writable state owned by the current owner.
    pub fn signal<T: Clone + PartialEq + 'static>(&self, value: T) -> Signal<T> {
        let id = self.create_node(
            NodeKind::Signal,
            Color::Clean,
            Some(Rc::new(SignalData {
                value: RefCell::new(value),
            })),
        );
        Signal::from_id(id)
    }

    /// Create a lazy derived value. `f` runs on the first read and then only
    /// when read after one of the values it read last time changed. An `Err`
    /// is stored as the value (errors are values).
    pub fn memo<T, F>(&self, f: F) -> Memo<T>
    where
        T: Clone + PartialEq + 'static,
        F: Fn(&Runtime) -> Result<T, Error> + 'static,
    {
        let id = self.create_node(
            NodeKind::Memo,
            Color::Dirty,
            Some(Rc::new(MemoData {
                f: Box::new(f),
                value: RefCell::new(None),
            })),
        );
        Memo::from_id(id)
    }

    /// Create an effect. It first runs at the next flush, then whenever a
    /// value it read changes (after equality cut-off). Nodes it creates are
    /// owned by it and disposed before each re-run.
    pub fn effect<F>(&self, f: F) -> Effect
    where
        F: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        let id = self.create_node(
            NodeKind::Effect,
            Color::Dirty,
            Some(Rc::new(EffectData {
                f: RefCell::new(Box::new(f)),
            })),
        );
        Effect { id }
    }

    /// `on change x { … }`: run `handler` when the value of `track`
    /// changes, never for its first value (boot or reload). `handler` runs
    /// untracked.
    pub fn on_change<T, F, H>(&self, track: F, mut handler: H) -> Effect
    where
        T: PartialEq + 'static,
        F: Fn(&Runtime) -> Result<T, Error> + 'static,
        H: FnMut(&Runtime, &T) -> Result<(), Error> + 'static,
    {
        let mut prev: Option<T> = None;
        self.effect(move |rt| {
            let value = track(rt)?;
            let first = prev.is_none();
            let changed = prev.as_ref() != Some(&value);
            if first || !changed {
                prev = Some(value);
                return Ok(());
            }
            let r = rt.untrack(|rt| handler(rt, &value));
            prev = Some(value);
            r
        })
    }
}

impl<T: Clone + PartialEq + 'static> Signal<T> {
    /// Read and track.
    pub fn get(self, rt: &Runtime) -> Result<T, Error> {
        self.with(rt, T::clone)
    }

    /// Read without tracking.
    pub fn get_untracked(self, rt: &Runtime) -> Result<T, Error> {
        rt.with_data::<SignalData<T>, _>(self.id, |d| d.value.borrow().clone())
    }

    /// Borrow the value (tracked) without cloning it.
    pub fn with<R>(self, rt: &Runtime, f: impl FnOnce(&T) -> R) -> Result<R, Error> {
        rt.track(self.id);
        let data = rt.data(self.id)?;
        let d = data
            .as_any()
            .downcast_ref::<SignalData<T>>()
            .ok_or(Error::TypeMismatch(self.id))?;
        let v = d.value.try_borrow().map_err(|_| Error::Reentrant)?;
        Ok(f(&v))
    }

    /// Write. Equal values are ignored (equality cut-off); writes within a
    /// tick coalesce to the latest value. A handler writing one cell more
    /// than 30 times a second is throttled (see [`crate::rate`]).
    pub fn set(self, rt: &Runtime, value: T) -> Result<(), Error> {
        rt.check_write_allowed(self.id)?;
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        if rt.rate_gate(self.id) {
            self.set_raw(rt, value).map(|_| ())
        } else {
            rt.defer_write(
                self.id,
                Box::new(move |rt: &Runtime| {
                    let _ = self.set_raw(rt, value);
                }),
            );
            Ok(())
        }
    }

    /// Modify the value in place; notifies only if it changed.
    pub fn update(self, rt: &Runtime, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        let mut v = self.get_untracked(rt)?;
        f(&mut v);
        self.set(rt, v)
    }

    /// Write without rate gating. Returns whether the value changed.
    pub(crate) fn set_raw(self, rt: &Runtime, value: T) -> Result<bool, Error> {
        let changed = rt.with_data::<SignalData<T>, _>(self.id, |d| {
            let Ok(mut v) = d.value.try_borrow_mut() else {
                return Err(Error::Reentrant);
            };
            if *v == value {
                Ok(false)
            } else {
                *v = value;
                Ok(true)
            }
        })??;
        if changed {
            rt.cell_changed(self.id);
        }
        Ok(changed)
    }
}

impl<T: Clone + PartialEq + 'static> Memo<T> {
    /// Bring up to date if needed, read and track.
    pub fn get(self, rt: &Runtime) -> Result<T, Error> {
        rt.track(self.id);
        rt.update_if_necessary(self.id)?;
        rt.with_data::<MemoData<T>, _>(self.id, |d| match d.value.try_borrow() {
            Ok(v) => v.clone().unwrap_or(Err(Error::Reentrant)),
            Err(_) => Err(Error::Reentrant),
        })?
    }

    /// Read without tracking.
    pub fn get_untracked(self, rt: &Runtime) -> Result<T, Error> {
        rt.untrack(|rt| self.get(rt))
    }
}

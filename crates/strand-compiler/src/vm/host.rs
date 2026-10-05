//! How the VM reaches services: the [`ServiceHost`] trait.
//!
//! The VM never knows how a service gets its data. It reads fields,
//! writes `rw` fields, calls `fn` methods, runs actions and listens to
//! events through this trait; the host keeps each field in the reactive
//! graph (usually a `Signal<Value>` per field), so a binding reading
//! `battery.percent` re-runs exactly when that field changes. M1 ships
//! [`super::schema_host::SchemaHost`]: every service of the schema with
//! default values, settable by tests (the deterministic mock) or fed by the
//! real clock and calendar ([`super::clock`]). M3 service crates implement
//! the same trait over D-Bus, PipeWire and Wayland.

use std::time::SystemTime;

use strand_core::{Error, EventQueue, KeyedSignal, NodeId, Runtime};

use super::value::{Value, ValueKey};

/// Who an action is for.
#[derive(Clone, Copy, Debug)]
pub enum ActionTarget<'a> {
    /// A service-level action: `notifications.clear()`.
    Service(&'a str),
    /// An item's action: `ws.focus()`, `n.expire()`; the item record as
    /// the program read it (its key says which one).
    Item(&'a Value),
}

/// The services a program reads, as the VM sees them.
///
/// Reads go through the reactive graph: `read` and `call` must read
/// `Signal`s (or memos) with tracking, so the binding that called them
/// depends on exactly what they read. Writes and actions run in handlers
/// (or for `<->`, outside any) and must not block: a real service sends
/// them to its own thread.
pub trait ServiceHost {
    /// A custom service the program declares (`service ppd from dbus …`),
    /// with its record. Called once at instantiation.
    fn declare(&self, _rt: &Runtime, _name: &str, _record: crate::ty::RecordId) {}

    /// The value of `service.field`, tracked.
    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error>;

    /// The core nodes a read of `service.field` depends on (`None`: of
    /// the service as a whole, as a method call such as `clock.format(p)`
    /// reads it): what the VM declares with `rt.reads_from` before the
    /// first flush, so sinks reading services are ranked from the start
    /// (architecture.md, "Lowering into strand-core"). A conservative
    /// superset is fine. The default (none) leaves the edges to be learned
    /// on first run.
    fn sources(&self, _rt: &Runtime, _service: &str, _field: Option<&str>) -> Vec<NodeId> {
        Vec::new()
    }

    /// A list field published as a keyed collection
    /// (`notifications.popups`, `workspaces.all`): a `for` over it follows
    /// its `VecDiff`s instead of comparing whole lists, so one new
    /// notification is one diff from the service to the scene. `None`
    /// (the default) for fields kept as plain values.
    fn read_keyed(
        &self,
        _rt: &Runtime,
        _service: &str,
        _field: &str,
    ) -> Option<KeyedSignal<ValueKey, Value>> {
        None
    }

    /// Write one `rw` leaf: `audio.sink.volume = 0.8` is `write(rt,
    /// "audio", [Field("sink"), Field("volume")], 0.8)`, so the service
    /// sends only what changed (a concurrent `muted` change is not
    /// overwritten). The first segment is always a field of the service.
    /// Hosts keep the written cell's generation tags with core's
    /// `Signal::write_tagged` and match the service's reports with
    /// `receive`, so an echo of this write is ignored.
    fn write(
        &self,
        rt: &Runtime,
        service: &str,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error>;

    /// Call a `fn` method of a service (`clock.format(p)`,
    /// `calendar.days(m)`, `workspaces.on(s)`), tracked.
    fn call(
        &self,
        rt: &Runtime,
        service: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error>;

    /// Start an `Async` method (`apps.search(q)`) as a load: `let hits =
    /// apps.search(query)` is `rt.async_memo(args, fetch)` per mounted
    /// `let`, so each change of the arguments starts one fetch and drops
    /// the superseded one (dropping the future cancels it), and the value
    /// keeps its last result while pending. The default runs
    /// [`ServiceHost::call`] once and is ready at once.
    fn fetch(&self, rt: &Runtime, service: &str, method: &str, args: Vec<Value>) -> Fetch {
        let r = self.call(rt, service, method, &args);
        Box::pin(async move {
            match r? {
                Value::Async(a) => match (&a.error, &a.value) {
                    (Some(e), _) => Err(Error::failed(e.to_string())),
                    (None, Some(v)) => Ok(v.clone()),
                    (None, None) => Ok(Value::Null),
                },
                v => Ok(v),
            }
        })
    }

    /// Run an action.
    fn action(
        &self,
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
    ) -> Result<(), Error>;

    /// The lossless queue of `service.event` (`notifications.received`);
    /// each event carries its parameters in order.
    fn event(&self, rt: &Runtime, service: &str, event: &str) -> Option<EventQueue<Vec<Value>>>;

    /// A component (or surface, or the config) reading `service` was
    /// mounted, or a surface reading it was shown: the service starts on
    /// its first reader (design.md, "Lifecycle").
    fn acquire(&self, _service: &str) {}

    /// The matching unmount or hide: the service stops 5 s after its last
    /// reader leaves or goes invisible.
    fn release(&self, _service: &str) {}

    /// The next wall-clock time the host loop must wake for (the next
    /// minute boundary while a clock is shown); `None` when nothing is
    /// due.
    fn next_wake(&self, _rt: &Runtime) -> Option<SystemTime> {
        None
    }

    /// The wall clock reached `now`: update time-driven fields.
    fn wake(&self, _rt: &Runtime, _now: SystemTime) {}
}

/// One step of a written path below a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathSeg {
    Field(String),
    Index(usize),
}

impl std::fmt::Display for PathSeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathSeg::Field(n) => write!(f, ".{n}"),
            PathSeg::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// A load started by [`ServiceHost::fetch`].
pub type Fetch = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Error>>>>;

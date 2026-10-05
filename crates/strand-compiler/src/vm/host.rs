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

use strand_core::{Error, EventQueue, Runtime};

use super::value::Value;

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

    /// Write a `rw` field (`audio.sink` with its `volume` changed is
    /// written as the whole new `sink` record).
    fn write(&self, rt: &Runtime, service: &str, field: &str, value: Value) -> Result<(), Error>;

    /// Call a `fn` method of a service (`clock.format(p)`,
    /// `calendar.days(m)`, `apps.search(q)`), tracked.
    fn call(
        &self,
        rt: &Runtime,
        service: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error>;

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
    /// mounted: the service starts on its first reader (design.md,
    /// "Lifecycle").
    fn acquire(&self, _service: &str) {}

    /// The matching unmount: the service stops 5 s after its last reader
    /// leaves.
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

//! System services: typed stores that start on their first reader, are
//! reference-counted, and stop 5 s after their last reader leaves or goes
//! invisible (design.md, "System services and third-party crates").
//!
//! A service is a state struct with `#[service(name = "…", schema = …)]`
//! and `#[derive(Store)]`, plus an `async fn run(cx: Cx<Self>)` that reads
//! the system, changes its state with [`Cx::update`], emits events and
//! answers writes, actions and async calls arriving through
//! [`Cx::recv`]. The derive generates a `Send` patch type (what the
//! service thread sends) and the logic-side cells (`strand-core` signals,
//! keyed collections and event queues) that apply those patches in one
//! tick. Services share one tokio current-thread runtime thread; a
//! service on a `!Send` library (PipeWire, the Wayland toplevel
//! protocols) runs on a thread of its own with the same protocol.
//!
//! The logic thread drives them through [`Services`] (the registry,
//! [`Services::pump`]) and a [`Client`] per service ([`Client::acquire`],
//! [`Client::release`]); the language side (the `strand` binary's
//! `ServiceHost` adapter) uses the [`DynService`] view, by field index
//! with [`Data`] values: this crate never sees the VM's `Value`
//! (architecture.md, "Several service crates, one host").
//!
//! Builtin services implemented here: [`system`] (the portal's
//! appearance settings), [`cpu`] and [`memory`] (procfs, sampled once a
//! second while a reader is visible). [`schemas`] lists their schema
//! texts, which replace the builtin schema's provisional stubs.

extern crate self as strand_services;

pub mod battery;
pub mod bus;
mod client;
pub mod cpu;
mod cx;
mod data;
pub mod dbus;
pub mod memory;
mod procfs;
mod service;
mod store;
pub mod system;
pub mod testing;

pub use bus::{Bus, Buses};
pub use client::{Client, DynService, Observer, STOP_GRACE, Services};
pub use cx::{Cx, Envelope, Msg, Reply, Write};
pub use data::{Data, DataError, FromData, Name, Rgba, SchemaType, Step, ToData, record_field};
pub use service::{CallSig, FromCall, LocalFuture, NoCall, Service, ServiceError, Start};
pub use store::{
    Applied, Cells, Event, EventInfo, FieldInfo, How, Keyed, Patch, SendItemWrite, SendWrite,
    Store, Target, apply_keyed, diff_data, keyed_changes, keyed_vec_of,
};
/// `strand-core`, as the generated code names it.
pub use strand_core as core;
pub use strand_services_macros::{Call, Data, Store, service};

/// The builtin services this crate implements, registered on `services`.
#[derive(Debug, Clone)]
pub struct Builtin {
    pub system: Client<system::System>,
    pub cpu: Client<cpu::Cpu>,
    pub memory: Client<memory::Memory>,
    pub battery: Client<battery::Battery>,
}

impl Builtin {
    /// Register every builtin service.
    pub fn register(services: &Services, rt: &strand_core::Runtime) -> Builtin {
        Builtin {
            system: services.register(rt),
            cpu: services.register(rt),
            memory: services.register(rt),
            battery: services.register(rt),
        }
    }

    /// Each as a [`DynService`].
    pub fn all(&self) -> Vec<std::rc::Rc<dyn DynService>> {
        vec![
            self.system.dynamic(),
            self.cpu.dynamic(),
            self.memory.dynamic(),
            self.battery.dynamic(),
        ]
    }
}

/// The schema texts of the builtin services, in registration order: what
/// the language extends its builtin schema with (`Schema::extend`), so
/// `strand check`, `strand run` and the LSP check against the real
/// services.
/// They live in `strand-services-schema` (the LSP reads them without this
/// runtime); each service's `SCHEMA` is its text there.
pub fn schemas() -> Vec<&'static str> {
    strand_services_schema::schemas()
}

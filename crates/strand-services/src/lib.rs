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
//! second while a reader is visible), and on D-Bus ([`dbus`] holds what
//! they share): [`battery`] (UPower), [`brightness`] (the backlight,
//! written through logind), [`network`] (NetworkManager or iwd, and the kernel's
//! addresses over rtnetlink), [`bluetooth`]
//! (BlueZ), [`notifications`] (the shell's own notification server),
//! [`media`] (MPRIS) and [`tray`] (StatusNotifierItem and DBusMenu);
//! [`apps`] (desktop entries, fuzzy search, frecency, launching); and
//! [`auth`] (the lock screen's password check, through `strand-auth`);
//! and [`session`] (logind: suspend, reboot, power off, lock, log out);
//! and [`outputs`] (the monitors' configuration, wlr-output-management);
//! and [`clipboard`] (the selection, copies and their history, over data
//! control); and [`disk`] (the mounted filesystems' usage, `statvfs`,
//! sampled every 10 s while a reader is visible); and [`keyboard`] (the
//! lock keys and the layout: a focused surface's keyboard, the kernel's
//! lights, the compositor); and (services-round) [`idle`] (whether the
//! user is away, ext-idle-notify-v1).
//! [`schemas`] lists their schema texts, which replace the builtin
//! schema's provisional stubs. A failed run, and what a service needs the
//! user to act on, is a [`ServiceDiagnostic`] ([`Services::take_diagnostics`]).
//! [`ipc_bus`] is no service: it is the running shell's D-Bus front end
//! (`io.github.jakeb_grant.Strand`), a task on the same runtime.

extern crate self as strand_services;

pub mod activation;
pub mod apps;
pub mod art;
#[cfg(feature = "pipewire")]
pub mod audio;
pub mod auth;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod bus;
pub mod child;
mod client;
pub mod clipboard;
pub mod cpu;
pub mod custom;
mod cx;
mod data;
pub mod dbus;
pub mod disk;
pub mod icon_theme;
pub mod idle;
pub mod ipc_bus;
pub mod keyboard;
pub mod media;
pub mod memory;
pub mod network;
pub mod notifications;
pub mod outputs;
pub mod pixmap;
mod procfs;
mod service;
pub mod session;
mod store;
pub mod system;
pub mod testing;
pub mod tray;
#[cfg(feature = "pipewire")]
pub mod video;
pub mod wm;

pub use bus::{Bus, Buses};
pub use client::{
    Change, Client, DynService, Observer, STOP_GRACE, ServiceDiagnostic, ServiceSnapshot, Services,
    Tap, TapEvent, catch_refusals,
};
pub use cx::{Cx, Envelope, Msg, Reply, Ticker, Write};
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
    pub brightness: Client<brightness::Brightness>,
    pub network: Client<network::Network>,
    pub bluetooth: Client<bluetooth::Bluetooth>,
    pub notifications: Client<notifications::Notifications>,
    pub media: Client<media::Media>,
    pub tray: Client<tray::Tray>,
    pub apps: Client<apps::Apps>,
    pub windows: Client<wm::Windows>,
    pub workspaces: Client<wm::Workspaces>,
    pub wm: Client<wm::Wm>,
    #[cfg(feature = "pipewire")]
    pub audio: Client<audio::AudioStore>,
    #[cfg(feature = "pipewire")]
    pub video: Client<video::Video>,
    pub auth: Client<auth::Auth>,
    pub session: Client<session::Session>,
    pub outputs: Client<outputs::Outputs>,
    pub clipboard: Client<clipboard::Clipboard>,
    pub disk: Client<disk::Disk>,
    pub keyboard: Client<keyboard::Keyboard>,
    pub idle: Client<idle::Idle>,
}

impl Builtin {
    /// Register every builtin service.
    pub fn register(services: &Services, rt: &strand_core::Runtime) -> Builtin {
        let b = Builtin {
            system: services.register(rt),
            cpu: services.register(rt),
            memory: services.register(rt),
            battery: services.register(rt),
            brightness: services.register(rt),
            network: services.register(rt),
            bluetooth: services.register(rt),
            notifications: services.register(rt),
            media: services.register(rt),
            tray: services.register(rt),
            apps: services.register(rt),
            windows: services.register(rt),
            workspaces: services.register(rt),
            wm: services.register(rt),
            #[cfg(feature = "pipewire")]
            audio: services.register(rt),
            #[cfg(feature = "pipewire")]
            video: services.register(rt),
            auth: services.register(rt),
            session: services.register(rt),
            outputs: services.register(rt),
            clipboard: services.register(rt),
            disk: services.register(rt),
            keyboard: services.register(rt),
            idle: services.register(rt),
        };
        // (history) The ones sampled on a clock, so a value kept while
        // hidden is known to be a poll.
        b.cpu.polls(Some(cpu::PERIOD));
        b.memory.polls(Some(memory::PERIOD));
        b.disk.polls(Some(disk::PERIOD));
        b.network.polls(Some(network::rates::PERIOD));
        b
    }

    /// Each as a [`DynService`].
    pub fn all(&self) -> Vec<std::rc::Rc<dyn DynService>> {
        vec![
            self.system.dynamic(),
            self.cpu.dynamic(),
            self.memory.dynamic(),
            self.battery.dynamic(),
            self.brightness.dynamic(),
            self.network.dynamic(),
            self.bluetooth.dynamic(),
            self.notifications.dynamic(),
            self.media.dynamic(),
            self.tray.dynamic(),
            self.apps.dynamic(),
            self.windows.dynamic(),
            self.workspaces.dynamic(),
            self.wm.dynamic(),
            #[cfg(feature = "pipewire")]
            self.audio.dynamic(),
            #[cfg(feature = "pipewire")]
            self.video.dynamic(),
            self.auth.dynamic(),
            self.session.dynamic(),
            self.outputs.dynamic(),
            self.clipboard.dynamic(),
            self.disk.dynamic(),
            self.keyboard.dynamic(),
            self.idle.dynamic(),
        ]
    }
}

/// The schema texts of the builtin services, in registration order: what
/// the language extends its builtin schema with (`Schema::extend`), so
/// `strand check`, `strand run` and the LSP check against the real
/// services.
/// They live in `strand-services-schema` (the LSP reads them without this
/// runtime); each service's `SCHEMA` is its text there.
///
/// Built without the `pipewire` feature, `audio` and `video` are not
/// served: their texts are left out (the stubs stay, answered at the
/// schema's defaults).
pub fn schemas() -> Vec<&'static str> {
    let mut all = strand_services_schema::schemas();
    if !cfg!(feature = "pipewire") {
        all.retain(|t| *t != strand_services_schema::AUDIO && *t != strand_services_schema::VIDEO);
    }
    all
}

//! The real services, joined to the language (architecture.md, "Several
//! service crates, one host").
//!
//! - [`schema`]: the builtin schema extended by every service crate this
//!   binary links (`strand_services::schemas()`); `strand check`, `strand
//!   run` (and its compile cache key, the schema's fingerprint) use it.
//! - [`StoreHost`]: one service store as a `ServiceHost`, converting its
//!   typed cells to and from the VM's `Value` ([`convert`]).
//! - [`Composite`]: one host over every store, routing by service name;
//!   the names nobody serves yet (and the clock and calendar) stay with a
//!   `SchemaHost`.
//! - [`Real`]: what `strand run` builds without `STRAND_MOCK`: the
//!   services registry, the builtin stores and the composite host.

mod adapter;
mod composite;
pub mod convert;

use std::rc::Rc;
use std::sync::OnceLock;

use strand_compiler::schema::Schema;
use strand_compiler::ty::TypeTable;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_services::{Builtin, Buses, Services};

pub use adapter::StoreHost;
pub use composite::Composite;

/// The schema `strand check` and `strand run` check against: the builtin
/// one with the linked service crates' declarations in place of their
/// provisional stubs. A service schema that does not apply (a bug their
/// tests catch: `service_schemas_extend_the_builtin_one`) is logged and
/// the builtin schema used.
pub fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| match Schema::builtin_with(&strand_services::schemas()) {
        Ok(s) => s,
        Err((i, errors)) => {
            log::error!("service schema #{i} does not apply: {errors:?}");
            Schema::builtin().clone()
        }
    })
}

/// The real services of one logic thread.
pub struct Real {
    pub services: Services,
    pub builtin: Builtin,
    /// The host `Instance` gets.
    pub host: Rc<Composite>,
    stores: Vec<Rc<StoreHost>>,
}

impl std::fmt::Debug for Real {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Real")
            .field("services", &self.services)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl Real {
    /// Register the builtin services on `buses` (nothing starts until a
    /// reader comes), each behind a [`StoreHost`] reading values of
    /// `types`, joined with `fallback` for every other name. `wake` is
    /// called from any thread when a service has sent something: the host
    /// loop then calls [`Services::pump`]. Call it before mounting (no
    /// owner current).
    pub fn start(
        rt: &Runtime,
        types: &TypeTable,
        buses: Buses,
        fallback: Rc<SchemaHost>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Real {
        let services = Services::new(rt, buses, wake);
        let builtin = Builtin::register(&services, rt);
        let types = Rc::new(types.clone());
        let mut host = Composite::new(fallback, types.clone());
        let mut stores = Vec::new();
        for svc in builtin.all() {
            let items = svc.item_records();
            let store = Rc::new(StoreHost::new(rt, svc, types.clone()));
            host.add(store.clone(), &[store.name()], &items);
            stores.push(store);
        }
        Real {
            services,
            builtin,
            host: Rc::new(host),
            stores,
        }
    }

    /// Stop every service (joining the shared runtime thread) and dispose
    /// the stores' cells.
    pub fn shutdown(&self, rt: &Runtime) {
        self.services.shutdown();
        for s in &self.stores {
            s.dispose(rt);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use strand_compiler::SourceMap;
    use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
    use strand_compiler::lower;
    use strand_compiler::vm::{ServiceHost, Value};
    use strand_services::{Cells as _, STOP_GRACE};

    use super::*;

    /// The schema the binary checks against holds each linked service's
    /// declaration in place of its stub, documented, and each store's
    /// fields and events are exactly its schema record's.
    #[test]
    fn service_schemas_extend_the_builtin_one() {
        let s = schema();
        assert!(s.undocumented().is_empty(), "{:#?}", s.undocumented());
        assert_ne!(s.fingerprint(), Schema::builtin().fingerprint());
        let rt = Runtime::new();
        let services = Services::new(&rt, Buses::none(), || {});
        for svc in Builtin::register(&services, &rt).all() {
            let name = svc.name();
            assert!(!s.provisional.contains(name), "{name} is still a stub");
            let id = s.services[name];
            let rec = s.types.record(id);
            let fields: Vec<(String, String, bool)> = rec
                .fields
                .iter()
                .map(|f| (f.name.clone(), s.types.show(&f.ty).to_string(), f.rw))
                .collect();
            let store: Vec<(String, String, bool)> = svc
                .fields()
                .iter()
                .map(|f| (f.name.to_string(), (f.ty)(), f.rw))
                .collect();
            assert_eq!(fields, store, "{name}'s store and schema disagree");
            let events: Vec<&str> = rec.events.iter().map(|e| e.name.as_str()).collect();
            let store_events: Vec<&str> = svc.events().iter().map(|e| e.name).collect();
            assert_eq!(events, store_events, "{name}'s events");
            // Same fields as the provisional stub it replaced: configs
            // checked before M3 still check.
            let builtin = Schema::builtin();
            let stub = builtin.types.record(builtin.services[name]);
            let stub_fields: Vec<(&str, bool)> = stub
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.rw))
                .collect();
            let real_fields: Vec<(&str, bool)> =
                rec.fields.iter().map(|f| (f.name.as_str(), f.rw)).collect();
            assert_eq!(stub_fields, real_fields, "{name} against its stub");
        }
        services.shutdown();
    }

    /// A config mounted against the real composite host, one monitor.
    struct Shell {
        rt: Runtime,
        real: Real,
        inst: Instance,
        scene: SceneMirror,
        now: Duration,
    }

    impl Shell {
        fn boot(src: &str) -> Shell {
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, schema());
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, schema()));
            let rt = Runtime::new();
            let fallback = Rc::new(SchemaHost::new(&rt, &program.types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(&rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let real = Real::start(&rt, &program.types, Buses::none(), fallback, || {});
            let inst = Instance::new(&rt, program, real.host.clone(), Storage::none());
            let mut shell = Shell {
                rt,
                real,
                inst,
                scene: SceneMirror::new(),
                now: Duration::ZERO,
            };
            shell.tick(Duration::ZERO);
            shell
        }

        /// Pump the services, then advance the logic clock by `by`.
        fn tick(&mut self, by: Duration) {
            self.real.services.pump(&self.rt);
            self.now += by;
            let u = self.inst.tick(self.now);
            assert!(u.errors.is_empty(), "{:?}", u.errors);
            self.scene.apply(&u.diff).unwrap();
        }

        /// Pump and flush until `done`, at most 5 s of real time.
        fn until(&mut self, what: &str, done: impl Fn(&Shell) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(10));
                self.tick(Duration::ZERO);
            }
        }

        fn set(&mut self, name: &str, v: bool) {
            self.inst
                .set(&format!("svc.{name}"), Value::Bool(v))
                .unwrap();
            self.tick(Duration::ZERO);
        }
    }

    impl Drop for Shell {
        fn drop(&mut self) {
            self.inst.shutdown();
            self.real.shutdown(&self.rt);
        }
    }

    /// Only the services a config reads start (the compiler collects the
    /// service paths each scope reads; a scope acquires those): a config
    /// that reads `cpu` starts it and nothing else, and its binding shows
    /// the store's value, converted, through the composite host.
    #[test]
    fn only_the_services_a_config_reads_start() {
        let mut shell = Shell::boot("bar B { text pct(cpu.usage) }\n");
        let b = shell.real.builtin.clone();
        assert_eq!(b.cpu.starts(), 1);
        assert_eq!(b.cpu.readers(), 1);
        assert_eq!(b.memory.starts(), 0, "never read, never started");
        assert_eq!(b.system.starts(), 0, "never read, never started");
        assert!(b.cpu.running());
        shell.until("cpu's first read", |s| s.real.builtin.cpu.reports() > 0);
        let usage = b.cpu.cells().snapshot(&shell.rt).unwrap().usage;
        let shown = format!("{}%", (usage * 100.0).round());
        assert_eq!(shell.scene.texts(), [shown]);
        // The same value through the host as the VM reads it.
        let v = shell
            .rt
            .untrack(|rt| shell.real.host.read(rt, "cpu", "usage"))
            .unwrap();
        assert_eq!(v, Value::float(usage));
        // Names no service crate serves yet answer from the fallback.
        let v = shell
            .rt
            .untrack(|rt| shell.real.host.read(rt, "battery", "percent"))
            .unwrap();
        assert_eq!(v, Value::float(0.0));
        assert!(
            shell
                .real
                .host
                .sources(&shell.rt, "cpu", Some("usage"))
                .len()
                >= 2,
            "the value memo and the store's cell"
        );
    }

    /// A popup reading `memory` starts it when it opens; closing it stops
    /// it 5 s later on the logic clock, and reopening inside those 5 s
    /// cancels the stop without a restart.
    #[test]
    fn a_popup_starts_its_service_and_stops_it_five_seconds_after_closing() {
        let mut shell = Shell::boot(
            "export state p = false\nbar B {\n  text \"x\"\n  popup { open: <-> p; text pct(memory.usage) }\n}\n",
        );
        let memory = shell.real.builtin.memory.clone();
        assert_eq!(memory.starts(), 0, "the popup is closed");
        shell.set("p", true);
        assert_eq!((memory.starts(), memory.readers()), (1, 1));
        assert!(memory.running());
        shell.until("memory's first read", |s| {
            s.scene.texts().iter().any(|t| t != "x" && t != "0%")
        });
        shell.set("p", false);
        assert_eq!(memory.readers(), 0);
        shell.tick(STOP_GRACE - Duration::from_millis(100));
        assert!(memory.running(), "still inside the grace");
        // Reopened inside the grace: the same run carries on.
        shell.set("p", true);
        shell.tick(Duration::from_secs(10));
        assert!(memory.running());
        assert_eq!((memory.starts(), memory.stops()), (1, 0));
        // Closed for good: stopped once the grace is over.
        shell.set("p", false);
        shell.tick(STOP_GRACE - Duration::from_millis(1));
        assert!(memory.running());
        shell.tick(Duration::from_millis(2));
        assert!(!memory.running(), "stopped 5 s after its last reader left");
        assert_eq!((memory.starts(), memory.stops()), (1, 1));
        // Opened again later: a fresh start.
        shell.set("p", true);
        assert_eq!(memory.starts(), 2);
    }

    /// A hidden surface stops a visible-only stream at once (cpu samples
    /// only while a reader is visible), long before the service stops.
    #[test]
    fn a_hidden_surface_stops_a_visible_only_stream() {
        let mut shell =
            Shell::boot("export state o = true\npanel P { open: <-> o; text pct(cpu.usage) }\n");
        let cpu = shell.real.builtin.cpu.clone();
        assert_eq!(cpu.readers(), 1);
        // Visible: it samples once a second.
        let first = cpu.reports();
        shell.until("a second sample", |s| {
            s.real.builtin.cpu.reports() >= first + 2
        });
        shell.set("o", false);
        assert_eq!(cpu.readers(), 0);
        assert!(cpu.running(), "the stop waits for the grace");
        // What was in flight when it was hidden.
        std::thread::sleep(Duration::from_millis(200));
        shell.tick(Duration::ZERO);
        let hidden = cpu.reports();
        // More than two sampling periods.
        std::thread::sleep(strand_services::cpu::PERIOD * 5 / 2);
        shell.tick(Duration::ZERO);
        assert_eq!(cpu.reports(), hidden, "no samples while hidden");
        // Shown again: sampling resumes.
        shell.set("o", true);
        shell.until("sampling again", |s| s.real.builtin.cpu.reports() > hidden);
        assert_eq!((cpu.starts(), cpu.stops()), (1, 0));
    }
}

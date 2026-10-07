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
            // The struct's `///` docs are the schema's (hover shows the
            // schema's): the two copies say the same.
            let member_doc = |m: &str| {
                s.doc(&strand_compiler::schema::DocKey::Member(
                    name.to_string(),
                    m.to_string(),
                ))
                .unwrap_or_default()
                .to_string()
            };
            for f in svc.fields() {
                assert_eq!(f.doc, member_doc(f.name), "{name}.{}'s docs", f.name);
            }
            for e in svc.events() {
                assert_eq!(e.doc, member_doc(e.name), "{name}.{}'s docs", e.name);
            }
            // A keyed list is keyed by what its record's schema `key` says.
            for (f, def) in svc.fields().iter().zip(&rec.fields) {
                if !f.keyed {
                    continue;
                }
                let key = match &def.ty {
                    strand_compiler::ty::Ty::List(item, _) => match &**item {
                        strand_compiler::ty::Ty::Record(r) => s.types.record(*r).key.clone(),
                        _ => None,
                    },
                    _ => None,
                };
                assert_eq!(
                    key,
                    f.key.map(|k| vec![k.to_string()]),
                    "{name}.{}'s key",
                    f.name
                );
            }
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

    /// A store with a keyed list, an `rw` field, an event, item actions
    /// and an async method, through `StoreHost` and `Composite` as the VM
    /// drives them.
    mod tally {
        use std::future::Future;
        use std::task::{Context, Poll, Wake, Waker};

        use strand_compiler::vm::host::{ActionTarget, PathSeg};
        use strand_core::Runtime;
        use strand_services::{Call, Cx, Event, Msg, ServiceError, Store, service};

        use super::*;

        pub const SCHEMA: &str = "
/// An item of the tally.
record TallyItem key id {
  /// Its key.
  id: int
  /// Its name.
  name: text
  /// Takes it off the tally.
  action remove()
}

/// A tally of items.
service tally {
  /// A level, written by the shell.
  level: float rw
  /// The items, keyed by `id`.
  items: [TallyItem]
  /// Adds an item.
  action add(name: text)
  /// Its text, back.
  fn echo(text: text) -> Async<text>
  /// An item was taken off.
  event removed(item: TallyItem)
}
";

        #[derive(strand_services::Data, Clone, Debug, Default, PartialEq)]
        #[data(name = "TallyItem", key = id)]
        pub struct Item {
            pub id: i64,
            pub name: String,
        }

        #[derive(Call, Debug)]
        pub enum TallyAction {
            Add(String),
            Remove { item: Item },
        }

        #[derive(Call, Debug)]
        pub enum TallyCall {
            Echo { text: String },
        }

        /// See the schema.
        #[service(name = "tally", schema = SCHEMA, action = TallyAction, call = TallyCall)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Tally {
            /// A level.
            #[store(rw)]
            pub level: f64,
            /// The items.
            #[store(keyed)]
            pub items: Vec<Item>,
            /// An item was taken off.
            pub removed: Event<Item>,
        }

        impl Tally {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                cx.ready();
                while let Some(m) = cx.recv().await {
                    match m {
                        // Settles on half what was written.
                        Msg::Write(w) => {
                            let v: f64 = w.value().unwrap_or(0.0);
                            cx.report(&w, |s| s.level = v / 2.0);
                        }
                        Msg::Action(TallyAction::Add(name)) => {
                            cx.update(|s| {
                                let id = s.items.len() as i64 + 1;
                                s.items.push(Item { id, name });
                            });
                        }
                        Msg::Action(TallyAction::Remove { item }) => {
                            cx.update(|s| s.items.retain(|i| i.id != item.id));
                            cx.emit(TallyEvent::Removed(item));
                        }
                        Msg::Call(TallyCall::Echo { text }, reply) => {
                            reply.send::<_, String>(Ok(text));
                        }
                        Msg::Visible(_) | Msg::Watch { .. } => {}
                    }
                }
                Ok(())
            }
        }

        struct Unpark(std::thread::Thread);

        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        fn block_on<T>(fut: impl Future<Output = T>) -> T {
            let mut fut = std::pin::pin!(fut);
            let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                    return v;
                }
                assert!(Instant::now() < deadline, "never resolved");
                std::thread::park_timeout(Duration::from_millis(50));
            }
        }

        /// Pump until `done`.
        fn until(rt: &Runtime, services: &Services, what: &str, done: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(5));
                services.pump(rt);
                rt.flush();
            }
        }

        #[test]
        fn keyed_lists_writes_events_item_actions_and_async_calls_cross() {
            let mut schema = Schema::builtin().clone();
            schema.extend(SCHEMA).unwrap();
            assert!(
                schema.undocumented().is_empty(),
                "{:?}",
                schema.undocumented()
            );
            let types = Rc::new(schema.types.clone());
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let client = services.register::<Tally>(&rt);
            let store = Rc::new(StoreHost::new(&rt, client.dynamic(), types.clone()));
            let fallback = Rc::new(SchemaHost::new(&rt, &types, None));
            let mut host = Composite::new(fallback, types.clone());
            let items = client.dynamic().item_records();
            assert_eq!(items, ["TallyItem"]);
            host.add(store.clone(), &["tally"], &items);
            let host: Rc<dyn ServiceHost> = Rc::new(host);
            host.acquire(&rt, "tally");
            let keyed = host
                .read_keyed(&rt, "tally", "items")
                .expect("a keyed field");
            assert!(host.read_keyed(&rt, "tally", "level").is_none());
            let removed = host.event(&rt, "tally", "removed").expect("an event");
            let heard = Rc::new(std::cell::RefCell::new(Vec::<Vec<Value>>::new()));
            let h = heard.clone();
            removed
                .on(&rt, move |_, args| {
                    h.borrow_mut().push(args.clone());
                    Ok(())
                })
                .unwrap();
            let len = |rt: &Runtime| keyed.with_untracked(rt, |v| v.len()).unwrap();
            // Actions on the service: the items arrive as keyed diffs.
            for name in ["a", "b", "c"] {
                host.action(
                    &rt,
                    ActionTarget::Service("tally"),
                    "add",
                    &[Value::text(name)],
                )
                .unwrap();
            }
            until(&rt, &services, "three items", || len(&rt) == 3);
            let list = rt.untrack(|rt| host.read(rt, "tally", "items")).unwrap();
            let names: Vec<String> = list
                .as_list()
                .unwrap()
                .iter()
                .map(|v| {
                    v.field(&types, "name")
                        .unwrap()
                        .as_text()
                        .unwrap()
                        .to_string()
                })
                .collect();
            assert_eq!(names, ["a", "b", "c"]);
            // An item's action goes to the member whose actions take it.
            let b = list.as_list().unwrap()[1].clone();
            host.action(&rt, ActionTarget::Item(&b), "remove", &[])
                .unwrap();
            until(&rt, &services, "b removed", || len(&rt) == 2);
            until(&rt, &services, "the event", || !heard.borrow().is_empty());
            assert_eq!(heard.borrow()[0], vec![b.clone()]);
            // A write shows at once; the service's answer (half) replaces
            // it, tagged so it is not taken for an echo.
            host.write(
                &rt,
                "tally",
                &[PathSeg::Field("level".into())],
                Value::float(0.8),
            )
            .unwrap();
            assert_eq!(
                rt.untrack(|rt| host.read(rt, "tally", "level")).unwrap(),
                Value::float(0.8)
            );
            until(&rt, &services, "the report", || {
                rt.untrack(|rt| host.read(rt, "tally", "level")).unwrap() == Value::float(0.4)
            });
            // Read-only fields refuse writes.
            assert!(
                host.write(&rt, "tally", &[PathSeg::Field("items".into())], Value::Null)
                    .is_err()
            );
            // An async method completes its fetch.
            let got = block_on(host.fetch(&rt, "tally", "echo", vec![Value::text("hi")]));
            assert_eq!(got.unwrap(), Value::text("hi"));
            assert!(
                host.call(&rt, "tally", "echo", &[Value::text("x")])
                    .is_err()
            );
            host.release(&rt, "tally");
            services.shutdown();
        }
    }
}

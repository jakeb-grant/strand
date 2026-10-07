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

/// How long `strand set` on a service waits for a stopped service's
/// first read.
pub const SET_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether `path` names a service field (`brightness.level`): the CLI's
/// `set` writes it through the service host.
pub fn is_service_path(path: &str) -> bool {
    let mut parts = path.split('.');
    let (Some(service), Some(_)) = (parts.next(), parts.next()) else {
        return false;
    };
    schema().services.contains_key(service)
}

/// Write `text` to the `rw` service field `path` (`brightness.level`,
/// `audio.sink.volume`) through `host`, read by the field's type as
/// `Instance::set_text` reads a state's: `0.4`, `40%`, `true`, an enum
/// variant, `null`. A number with a sign is a step from the current
/// value (design.md: `strand set audio.sink.volume +5%`), never below 0.
pub fn set_text(
    host: &dyn strand_compiler::vm::ServiceHost,
    rt: &Runtime,
    types: &TypeTable,
    path: &str,
    text: &str,
) -> Result<(), String> {
    use strand_compiler::ty::{Prim, Ty};
    use strand_compiler::vm::Value;
    use strand_compiler::vm::host::PathSeg;
    let s = schema();
    let mut parts = path.split('.');
    let service = parts.next().unwrap_or_default();
    let fields: Vec<&str> = parts.collect();
    let Some(&record) = s.services.get(service) else {
        return Err(format!("nothing is exported as `{path}`"));
    };
    if fields.is_empty() {
        return Err(format!("`{path}` names a service, not a field"));
    }
    // The leaf's type, and whether it is writable.
    let mut rec = record;
    let mut leaf = None;
    for (i, f) in fields.iter().enumerate() {
        let def = s
            .types
            .record(rec)
            .fields
            .iter()
            .find(|d| d.name == *f)
            .ok_or_else(|| format!("`{path}` has no field `{f}`"))?;
        if i + 1 == fields.len() {
            leaf = Some(def.clone());
        } else {
            match def.ty.non_null() {
                Ty::Record(r) => rec = *r,
                _ => return Err(format!("`{path}` has no field `{}`", fields[i + 1])),
            }
        }
    }
    let leaf = leaf.ok_or_else(|| format!("`{path}` has no field"))?;
    if !leaf.rw {
        return Err(format!("`{path}` is read-only"));
    }
    let text = text.trim();
    let number = |t: &str| -> Option<f64> {
        match t.strip_suffix('%') {
            Some(p) => p.trim().parse::<f64>().ok().map(|n| n / 100.0),
            None => t.parse::<f64>().ok(),
        }
    };
    let relative = text.starts_with('+') || text.starts_with('-');
    let current = || -> Result<f64, String> {
        let mut v = host
            .read(rt, service, fields[0])
            .map_err(|e| e.to_string())?;
        for f in &fields[1..] {
            v = v.field(types, f).cloned().unwrap_or(Value::Null);
        }
        v.as_f64()
            .ok_or_else(|| format!("`{path}` has no value to step from"))
    };
    let value = match leaf.ty.non_null() {
        _ if text == "null" && matches!(leaf.ty, Ty::Optional(_)) => Value::Null,
        Ty::Prim(Prim::Float | Prim::Percent) => {
            let n = number(text).ok_or_else(|| format!("`{text}` is not a number"))?;
            Value::float(if relative {
                (current()? + n).max(0.0)
            } else {
                n
            })
        }
        Ty::Prim(Prim::Int) => {
            let n: i64 = text
                .parse()
                .map_err(|_| format!("`{text}` is not a whole number"))?;
            Value::int(if relative {
                (current()? as i64 + n).max(0)
            } else {
                n
            })
        }
        Ty::Prim(Prim::Bool) => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("`{text}` is not true or false")),
        },
        Ty::Prim(Prim::Text | Prim::Path) => Value::text(text),
        Ty::Enum(e) => {
            let def = s.types.enum_(*e);
            let v = def
                .variant(text)
                .ok_or_else(|| format!("`{text}` is not a {}", def.name))?;
            Value::Enum(*e, v)
        }
        _ => return Err(format!("`{path}` cannot be set from the command line")),
    };
    let segs: Vec<PathSeg> = fields.iter().map(|f| PathSeg::Field((*f).into())).collect();
    host.write(rt, service, &segs, value)
        .map_err(|e| e.to_string())
}

/// The real services of one logic thread.
pub struct Real {
    pub services: Services,
    pub builtin: Builtin,
    /// The host `Instance` gets.
    pub host: Rc<Composite>,
    stores: Vec<Rc<StoreHost>>,
    types: Rc<TypeTable>,
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
            types,
        }
    }

    /// `strand set brightness.level 0.4` (or `+5%`): an `rw` service
    /// field written from the CLI ([`set_text`]). A service nobody reads
    /// is started for it; for a relative step its first read is waited
    /// for (up to [`SET_WAIT`], that service's only), so the step starts
    /// from its real value. It stops 5 s later.
    pub fn set_text(&self, rt: &Runtime, path: &str, text: &str) -> Result<(), String> {
        let name = path.split('.').next().unwrap_or_default();
        let svc = self.builtin.all().into_iter().find(|s| s.name() == name);
        let relative = text.trim().starts_with(['+', '-']);
        if let Some(svc) = &svc {
            svc.acquire(rt);
            if relative {
                self.services.wait_ready_of(rt, Some(name), SET_WAIT);
            }
        }
        let r = set_text(&*self.host, rt, &self.types, path, text);
        if let Some(svc) = &svc {
            svc.release(rt);
        }
        r
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

    /// `strand set` on a service field: absolute and relative values by
    /// the field's type, read-only fields and unknown names refused.
    #[test]
    fn strand_set_writes_service_fields() {
        let rt = Runtime::new();
        let types = schema().types.clone();
        let host = SchemaHost::new(&rt, &types, None);
        host.set(&rt, "brightness.level", Value::float(0.5))
            .unwrap();
        let level =
            |host: &SchemaHost| host.get(&rt, "brightness.level").unwrap().as_f64().unwrap();
        assert!(is_service_path("brightness.level"));
        assert!(!is_service_path("brightness"));
        assert!(!is_service_path("theme.look"));
        set_text(&host, &rt, &types, "brightness.level", "+5%").unwrap();
        assert!((level(&host) - 0.55).abs() < 1e-9, "{}", level(&host));
        set_text(&host, &rt, &types, "brightness.level", "-0.1").unwrap();
        assert!((level(&host) - 0.45).abs() < 1e-9);
        set_text(&host, &rt, &types, "brightness.level", "-100%").unwrap();
        assert_eq!(level(&host), 0.0, "never below 0");
        set_text(&host, &rt, &types, "brightness.level", "40%").unwrap();
        assert!((level(&host) - 0.4).abs() < 1e-9);
        set_text(&host, &rt, &types, "network.wifi", "false").unwrap();
        assert_eq!(host.get(&rt, "network.wifi").unwrap(), Value::Bool(false));
        let err = |p: &str, v: &str| set_text(&host, &rt, &types, p, v).unwrap_err();
        assert!(err("brightness.available", "true").contains("read-only"));
        assert!(err("brightness.level", "loud").contains("not a number"));
        assert!(err("brightness.nope", "1").contains("no field"));
        assert!(err("nothing.level", "1").contains("nothing is exported"));
    }

    /// The schema the binary checks against holds each linked service's
    /// declaration in place of its stub, documented, and each store's
    /// fields and events are exactly its schema record's.
    #[test]
    fn service_schemas_extend_the_builtin_one() {
        // Every text applies (a failing one would leave `schema()` on the
        // stubs, with only a log line).
        if let Err((i, errors)) = Schema::builtin_with(&strand_services::schemas()) {
            panic!(
                "service schema #{i} does not apply: {errors:#?}\n{}",
                strand_services::schemas()[i]
            );
        }
        let s = schema();
        assert!(s.undocumented().is_empty(), "{:#?}", s.undocumented());
        assert_ne!(s.fingerprint(), Schema::builtin().fingerprint());
        let rt = Runtime::new();
        let services = Services::new(&rt, Buses::none(), || {});
        for svc in Builtin::register(&services, &rt).all() {
            let name = svc.name();
            assert!(
                !s.provisional.contains(name),
                "{name} is still a stub: {:?}",
                s.provisional
            );
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
            // Every field of the provisional stub it replaced, with the
            // same `rw` mark and in the same order, and every event: configs
            // checked before M3 still check. A real service may add fields
            // (battery's `devices`; decisions.md, wave4-a2).
            let builtin = Schema::builtin();
            let stub = builtin.types.record(builtin.services[name]);
            let stub_fields: Vec<(&str, bool)> = stub
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.rw))
                .collect();
            let real_fields: Vec<(&str, bool)> = rec
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.rw))
                .filter(|f| stub_fields.iter().any(|s| s.0 == f.0))
                .collect();
            assert_eq!(stub_fields, real_fields, "{name} against its stub");
            let stub_events: Vec<&str> = stub.events.iter().map(|e| e.name.as_str()).collect();
            assert!(
                stub_events.iter().all(|e| events.contains(e)),
                "{name} keeps its stub's events {stub_events:?}"
            );
            assert_calls_match(s, &rt, &*svc);
        }
        // Every service schema text has its store among the builtins, and
        // every builtin has its text: a text without a store would check
        // and hover in the LSP but be served at defaults.
        let mut texts: Vec<String> = strand_services::schemas()
            .iter()
            .map(|t| {
                // The service a text declares: the stub it makes real.
                let builtin = Schema::builtin();
                let mut one = builtin.clone();
                one.extend(t).unwrap();
                let names: Vec<&String> = one
                    .services
                    .keys()
                    .filter(|n| builtin.provisional.contains(*n) && !one.provisional.contains(*n))
                    .collect();
                assert_eq!(names.len(), 1, "one service per text: {names:?}");
                names[0].clone()
            })
            .collect();
        texts.sort();
        let mut stores: Vec<String> = Builtin::register(&services, &rt)
            .all()
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        stores.sort();
        assert_eq!(texts, stores, "schema texts and builtin stores");
        services.shutdown();
    }

    /// A store's actions, async methods and `fn` methods are its schema
    /// record's, by name and arity, and the actions and async methods of
    /// the item records it hands out or takes are those records': the
    /// language routes calls by these names, so a spelling that differs
    /// would type-check and then fail at run time.
    fn assert_calls_match(s: &Schema, rt: &Runtime, svc: &dyn strand_services::DynService) {
        use strand_compiler::ty::Ty;
        let name = svc.name();
        #[derive(Debug, PartialEq)]
        enum Kind {
            Action,
            Async,
            Fn,
        }
        let methods = |r: strand_compiler::ty::RecordId| -> Vec<(Kind, String, usize)> {
            let mut v: Vec<(Kind, String, usize)> = s
                .types
                .record(r)
                .methods
                .iter()
                .filter_map(|m| {
                    let sig = m.sigs.first()?;
                    let kind = if sig.action {
                        Kind::Action
                    } else if matches!(sig.ret, Ty::Async(_)) {
                        Kind::Async
                    } else {
                        Kind::Fn
                    };
                    Some((kind, m.name.clone(), sig.params.len()))
                })
                .collect();
            v.sort_by(|a, b| a.1.cmp(&b.1));
            v
        };
        let of = |sigs: &[strand_services::CallSig], item: Option<&str>| -> Vec<(String, usize)> {
            let mut v: Vec<(String, usize)> = sigs
                .iter()
                .filter(|c| c.item.as_deref() == item)
                .map(|c| (c.name.to_string(), c.arity))
                .collect();
            v.sort();
            v
        };
        let want = |m: &[(Kind, String, usize)], k: Kind| -> Vec<(String, usize)> {
            m.iter()
                .filter(|(kind, _, _)| *kind == k)
                .map(|(_, n, a)| (n.clone(), *a))
                .collect()
        };
        let (actions, calls) = (svc.action_sigs(), svc.method_sigs());
        let own = methods(s.services[name]);
        assert_eq!(
            of(&actions, None),
            want(&own, Kind::Action),
            "{name}'s actions"
        );
        assert_eq!(
            of(&calls, None),
            want(&own, Kind::Async),
            "{name}'s async methods"
        );
        for (f, arity) in want(&own, Kind::Fn) {
            let args = vec![strand_services::Data::Null; arity];
            assert!(
                svc.call(rt, &f, &args).is_some(),
                "{name} does not answer its `fn {f}`"
            );
        }
        for record in svc.item_records() {
            let r = s
                .types
                .find_record(&record)
                .unwrap_or_else(|| panic!("{name} hands out `{record}`, which the schema lacks"));
            let theirs = methods(r);
            assert_eq!(
                of(&actions, Some(&record)),
                want(&theirs, Kind::Action),
                "{name}: the actions of `{record}`"
            );
            assert_eq!(
                of(&calls, Some(&record)),
                want(&theirs, Kind::Async),
                "{name}: the async methods of `{record}`"
            );
        }
        // Every call it takes was compared above.
        let compared = |c: &strand_services::CallSig| match &c.item {
            None => true,
            Some(r) => svc.item_records().contains(r),
        };
        assert!(
            actions.iter().chain(&calls).all(compared),
            "{name}: {actions:?} {calls:?}"
        );
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

    /// A surface's `open` binding is read by the scope around the
    /// surface, not by its content: design.md's toasts panel (`open:
    /// shown.len > 0` over `notifications.popups`) holds `notifications`
    /// while it is closed, or no notification could ever open it.
    #[test]
    fn a_closed_surface_holds_what_its_open_binding_reads() {
        let shell = Shell::boot(
            "let shown = memory.usage\npanel P {\n  open: shown > 2\n  text \"x\"\n}\n",
        );
        let memory = shell.real.builtin.memory.clone();
        assert_eq!(memory.starts(), 1, "the closed panel's `open` reads it");
        assert_eq!(memory.readers(), 1);
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
  /// Its own level, written by the shell.
  level: float rw
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
            pub level: f64,
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

        /// The item writes the tally saw.
        static TALLY_WRITES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        impl Tally {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                cx.ready();
                while let Some(m) = cx.recv().await {
                    match m {
                        // An item's level: applied as written, the write
                        // logged with the item's key.
                        Msg::Write(w) if w.key.is_some() => {
                            let item: Item = w.field_value().map_err(ServiceError::from)?;
                            TALLY_WRITES.lock().unwrap().push(format!(
                                "{} {:?}{} {:?}",
                                w.field,
                                w.key,
                                w.path.iter().map(ToString::to_string).collect::<String>(),
                                w.value
                            ));
                            cx.report(&w, |s| {
                                if let Some(i) = s.items.iter_mut().find(|i| i.id == item.id) {
                                    *i = item;
                                }
                            });
                        }
                        // Settles on half what was written.
                        Msg::Write(w) => {
                            let v: f64 = w.value().unwrap_or(0.0);
                            cx.report(&w, |s| s.level = v / 2.0);
                        }
                        Msg::Action(TallyAction::Add(name)) => {
                            cx.update(|s| {
                                let id = s.items.len() as i64 + 1;
                                s.items.push(Item {
                                    id,
                                    name,
                                    level: 0.0,
                                });
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

        pub const WIFI_SCHEMA: &str = "
/// Wi-Fi.
service wifi {
  /// The network joined.
  ssid: text
  /// The networks in range: scanned only while a visible reader reads
  /// them.
  networks: [text]
}
";

        /// What the wifi service's body saw, in order.
        static WIFI_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        /// The tests reading `WIFI_LOG` run one at a time.
        static WIFI_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// A service with a stream field.
        #[service(name = "wifi", schema = WIFI_SCHEMA)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Wifi {
            /// The network joined.
            pub ssid: String,
            /// The networks in range: scanned only while a visible reader
            /// reads them.
            #[store(stream)]
            pub networks: Vec<String>,
        }

        impl Wifi {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                let log = |s: String| WIFI_LOG.lock().unwrap().push(s);
                log(format!("start scanning={}", cx.watched("networks")));
                cx.update(|s| s.ssid = "home".into());
                cx.ready();
                while let Some(m) = cx.recv().await {
                    if let Msg::Watch { field, on } = m {
                        log(format!("scanning={on} ({field})"));
                        if on {
                            cx.update(|s| s.networks = vec!["home".into(), "cafe".into()]);
                        }
                    }
                }
                Ok(())
            }
        }

        pub const SHELF_SCHEMA: &str = "
/// A shelf: its boot report carries a keyed list and a flag.
service shelf {
  /// Whether it is dark.
  dark: bool
  /// The items, keyed by `id`.
  items: [TallyItem]
  /// Adds an item.
  action add(name: text)
}
";

        #[derive(Call, Debug)]
        pub enum ShelfAction {
            Add(String),
        }

        /// A service whose first report (before `ready()`) holds a keyed
        /// list and a plain field, as a real service's boot read does.
        #[service(name = "shelf", schema = SHELF_SCHEMA, action = ShelfAction)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Shelf {
            /// Whether it is dark.
            pub dark: bool,
            /// The items.
            #[store(keyed)]
            pub items: Vec<Item>,
        }

        /// Lets the shelf's boot read finish (after the shell has run).
        static SHELF_GO: tokio::sync::Notify = tokio::sync::Notify::const_new();

        impl Shelf {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                SHELF_GO.notified().await;
                cx.update(|s| {
                    s.dark = true;
                    s.items = vec![
                        Item {
                            id: 1,
                            name: "a".into(),
                            level: 0.0,
                        },
                        Item {
                            id: 2,
                            name: "b".into(),
                            level: 0.0,
                        },
                    ];
                });
                cx.ready();
                while let Some(m) = cx.recv().await {
                    if let Msg::Action(ShelfAction::Add(name)) = m {
                        cx.update(|s| {
                            let id = s.items.len() as i64 + 1;
                            s.items.push(Item {
                                id,
                                name,
                                level: 0.0,
                            });
                            s.dark = !s.dark;
                        });
                    }
                }
                Ok(())
            }
        }

        /// A service's boot report reaches `on change` as a baseline, not
        /// a change, through the store host: neither a keyed field's list
        /// nor a plain field fires (`on change` never fires at boot, nor
        /// when a service starts late); a later change fires both.
        #[test]
        fn on_change_skips_a_services_boot_report() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let shelf = services.register::<Shelf>(&rt);
            let schema_text = format!("{SCHEMA}{SHELF_SCHEMA}");
            let (mut inst, mut scene) = mount(
                &rt,
                &schema_text,
                "export state items = 0\nexport state darks = 0\non change shelf.items { items = items + 1 }\non change shelf.dark { darks = darks + 1 }\nbar B { text join(\" \", shelf.items.count(i => true), shelf.dark) }\n",
                shelf.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            // The shell runs (its handlers have their baselines) before the
            // service's boot read lands, as with a slow D-Bus service.
            for _ in 0..3 {
                step(&mut inst);
            }
            SHELF_GO.notify_one();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"2 true".to_string()) {
                assert!(Instant::now() < deadline, "the boot report never landed");
                std::thread::sleep(Duration::from_millis(5));
            }
            for _ in 0..5 {
                step(&mut inst);
            }
            assert_eq!(inst.value_of("svc", "items").unwrap(), Value::int(0));
            assert_eq!(inst.value_of("svc", "darks").unwrap(), Value::int(0));
            shelf
                .dynamic()
                .action(&rt, "add", None, &[strand_services::Data::Text("c".into())])
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"3 false".to_string()) {
                assert!(Instant::now() < deadline, "the change never landed");
                std::thread::sleep(Duration::from_millis(5));
            }
            step(&mut inst);
            assert_eq!(inst.value_of("svc", "items").unwrap(), Value::int(1));
            assert_eq!(inst.value_of("svc", "darks").unwrap(), Value::int(1));
            inst.shutdown();
            services.shutdown();
        }

        /// `src` mounted on one monitor against a composite host serving
        /// `svc` (declared by `schema_text`) and the schema's defaults.
        fn mount(
            rt: &Runtime,
            schema_text: &str,
            src: &str,
            svc: Rc<dyn strand_services::DynService>,
        ) -> (Instance, SceneMirror) {
            let schema = Schema::builtin_with(&[schema_text]).unwrap();
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, &schema);
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, &schema));
            let types = Rc::new(program.types.clone());
            let fallback = Rc::new(SchemaHost::new(rt, &types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let name = svc.name();
            let items = svc.item_records();
            let store = Rc::new(StoreHost::new(rt, svc, types.clone()));
            let mut host = Composite::new(fallback, types);
            host.add(store, &[name], &items);
            let inst = Instance::new(rt, program, Rc::new(host), Storage::none());
            (inst, SceneMirror::new())
        }

        /// An async method called anywhere reaches the service: inside a
        /// larger expression of a binding (`tally.echo(x) ?? "none"`, the
        /// scope's load of that call) and awaited in a handler.
        #[test]
        fn async_calls_anywhere_reach_the_service() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "export state q = \"hi\"\nexport state got = \"\"\nbar B {\n  text tally.echo(q) ?? \"none\"\n  text join(\"+\", tally.echo(\"a\") ?? \"\", \"b\")\n  text got\n}\nafter 1s { got = await tally.echo(\"late\") }\n",
                tally.dynamic(),
            );
            let mut now = Duration::ZERO;
            let mut step = |inst: &mut Instance, by: Duration| {
                services.pump(&rt);
                now += by;
                let u = inst.tick(now);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let wait = |what: &str,
                        step: &mut dyn FnMut(&mut Instance, Duration) -> Vec<String>,
                        inst: &mut Instance,
                        want: &[&str]| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let texts = step(inst, Duration::ZERO);
                    if want.iter().all(|w| texts.iter().any(|t| t == w)) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "never: {what} ({texts:?})");
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            wait(
                "the answers in bindings",
                &mut step,
                &mut inst,
                &["hi", "a+b"],
            );
            // The argument changes: one new fetch, its answer shown.
            inst.set("svc.q", Value::text("there")).unwrap();
            wait("the new answer", &mut step, &mut inst, &["there"]);
            // A handler awaits the call.
            step(&mut inst, Duration::from_secs(1));
            wait("the handler's answer", &mut step, &mut inst, &["late"]);
            inst.shutdown();
            services.shutdown();
        }

        /// `for i in tally.items { … i.level = … }`: an `rw` field of an
        /// item of a keyed list is written through the service by the
        /// item's key, from a handler and from a slider's `<->`. The row
        /// shows each write at once; the service's answer to an earlier
        /// write, arriving after a later one, is ignored as its echo, so
        /// the item never goes back to the earlier value.
        #[test]
        fn an_items_rw_field_is_written_by_its_key() {
            TALLY_WRITES.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "bar B {\n  for i in tally.items {\n    row {\n      text join(\" \", i.name, i.level)\n      box { on click { i.level = 0.3; i.level = 0.6 } }\n      slider { value: <-> i.level }\n    }\n  }\n}\n",
                tally.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.clone()
            };
            for name in ["a", "b"] {
                tally
                    .dynamic()
                    .action(
                        &rt,
                        "add",
                        None,
                        &[strand_services::Data::Text(name.into())],
                    )
                    .unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).texts().contains(&"b 0".to_string()) {
                assert!(Instant::now() < deadline, "the items never showed");
                std::thread::sleep(Duration::from_millis(5));
            }
            // Every level the second item takes, as the store applies it.
            let levels: Rc<std::cell::RefCell<Vec<strand_services::Data>>> = Rc::default();
            let l = levels.clone();
            tally.dynamic().observe(Box::new(move |_, a| {
                if let strand_services::Applied::Keyed { diffs, .. } = a {
                    for d in diffs {
                        if let strand_core::VecDiff::Update { value, .. } = d
                            && let Ok(level) =
                                strand_services::record_field(value, "TallyItem", "level")
                        {
                            l.borrow_mut().push(level.clone());
                        }
                    }
                }
            }));
            // Its button writes 0.3, then 0.6: both shown at once (the last
            // wins), both sent with the item's key.
            let sc = step(&mut inst);
            let b = sc.of_kind(strand_scene::NodeKind::Box)[1];
            assert!(inst.event(b, "click", Vec::new()));
            assert!(step(&mut inst).texts().contains(&"b 0.6".to_string()));
            let deadline = Instant::now() + Duration::from_secs(5);
            while TALLY_WRITES.lock().unwrap().len() < 2
                || tally.cells().items.pending_item_writes(&rt, &2) > 0
            {
                assert!(Instant::now() < deadline, "the answers never came");
                std::thread::sleep(Duration::from_millis(5));
                step(&mut inst);
            }
            assert_eq!(
                *TALLY_WRITES.lock().unwrap(),
                [
                    "items Some(Int(2)).level Float(0.3)",
                    "items Some(Int(2)).level Float(0.6)"
                ]
            );
            use strand_services::Data::Float;
            assert_eq!(
                *levels.borrow(),
                [Float(0.3), Float(0.6)],
                "the answer to 0.3 came after the 0.6 write: an echo, ignored"
            );
            let texts = step(&mut inst).texts();
            assert!(texts.contains(&"b 0.6".to_string()), "{texts:?}");
            assert!(texts.contains(&"a 0".to_string()), "{texts:?}");
            // The first row's slider writes the first item.
            let slider = step(&mut inst).of_kind(strand_scene::NodeKind::Slider)[0];
            inst.write(
                slider,
                strand_scene::Prop::Value,
                strand_scene::PropValue::Number(0.25),
            )
            .unwrap();
            assert!(step(&mut inst).texts().contains(&"a 0.25".to_string()));
            let deadline = Instant::now() + Duration::from_secs(5);
            while TALLY_WRITES.lock().unwrap().len() < 3 {
                assert!(Instant::now() < deadline, "the slider's write never came");
                std::thread::sleep(Duration::from_millis(5));
                step(&mut inst);
            }
            assert_eq!(
                TALLY_WRITES.lock().unwrap()[2],
                "items Some(Int(1)).level Float(0.25)"
            );
            inst.shutdown();
            services.shutdown();
        }

        /// A bar shows the SSID; a closed popup lists the networks. The
        /// scan (the stream field) runs only while the popup is open,
        /// though the service runs all along.
        #[test]
        fn a_closed_popup_keeps_a_stream_field_off_while_a_bar_reads_another() {
            let _serial = WIFI_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            WIFI_LOG.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let wifi = services.register::<Wifi>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                WIFI_SCHEMA,
                "export state p = false\nbar B {\n  text wifi.ssid\n  popup { open: <-> p; for n in wifi.networks key n { text n } }\n}\n",
                wifi.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"home".to_string()) {
                assert!(Instant::now() < deadline, "the bar never showed the SSID");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(wifi.running());
            assert_eq!(wifi.field_readers(0), 1, "the bar reads the SSID");
            assert_eq!(wifi.field_readers(1), 0, "nobody reads the networks");
            assert_eq!(*WIFI_LOG.lock().unwrap(), ["start scanning=false"]);
            // The popup opens: the scan starts.
            inst.set("svc.p", Value::Bool(true)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"cafe".to_string()) {
                assert!(
                    Instant::now() < deadline,
                    "the popup never listed the networks"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(wifi.field_readers(1), 1);
            // It closes: the scan stops at once; the service runs on for
            // the bar.
            inst.set("svc.p", Value::Bool(false)).unwrap();
            step(&mut inst);
            assert_eq!(wifi.field_readers(1), 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while WIFI_LOG.lock().unwrap().len() < 3 {
                assert!(Instant::now() < deadline, "the scan never stopped");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                *WIFI_LOG.lock().unwrap(),
                [
                    "start scanning=false",
                    "scanning=true (networks)",
                    "scanning=false (networks)"
                ]
            );
            assert!(wifi.running());
            assert_eq!(wifi.starts(), 1);
            inst.shutdown();
            services.shutdown();
        }

        /// Live reload never restarts a built-in service (design.md, "Live
        /// reload": built-in services are kept): a bar reading `cpu.usage`
        /// and a shown popup reading a stream field (`wifi.networks`) go
        /// through a prop edit and a hard reload with each service started
        /// once, never stopped, and the stream never switched off and on
        /// (a Wi-Fi scan or a level meter would restart).
        #[test]
        fn live_reload_keeps_the_services_and_their_streams_running() {
            let _serial = WIFI_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            WIFI_LOG.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let builtin = Builtin::register(&services, &rt);
            let wifi = services.register::<Wifi>(&rt);
            let mut texts = strand_services::schemas();
            texts.push(WIFI_SCHEMA);
            let schema = Schema::builtin_with(&texts).unwrap();
            let src = |w: u32| {
                format!(
                    "export state p = true\nbar B {{\n  text pct(cpu.usage)\n  box {{ width: {w} }}\n  popup {{ open: <-> p; for n in wifi.networks key n {{ text n }} }}\n}}\n"
                )
            };
            let build = |prev: Option<&strand_compiler::reconcile::Build>, text: String| {
                let mut map = SourceMap::new();
                map.add("svc.strand", text);
                strand_compiler::reconcile::Build::compile_with(prev, map, &schema)
                    .unwrap_or_else(|d| panic!("{d:#?}"))
            };
            let first = build(None, src(10));
            let types = Rc::new(first.program.types.clone());
            let fallback = Rc::new(SchemaHost::new(&rt, &types, None));
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
            let mut host = Composite::new(fallback, types.clone());
            for svc in builtin.all().into_iter().chain([wifi.dynamic()]) {
                let name = svc.name();
                let items = svc.item_records();
                host.add(
                    Rc::new(StoreHost::new(&rt, svc, types.clone())),
                    &[name],
                    &items,
                );
            }
            let mut inst = Instance::from_build(&rt, &first, Rc::new(host), Storage::none());
            let mut scene = SceneMirror::new();
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"cafe".to_string()) {
                assert!(
                    Instant::now() < deadline,
                    "the popup never listed the networks"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let cpu = &builtin.cpu;
            let settled = |what: &str| {
                assert_eq!((cpu.starts(), cpu.stops()), (1, 0), "cpu, {what}");
                assert_eq!((wifi.starts(), wifi.stops()), (1, 0), "wifi, {what}");
                assert_eq!(cpu.readers(), 1, "cpu readers, {what}");
                assert_eq!(wifi.field_readers(1), 1, "networks readers, {what}");
            };
            settled("at boot");
            // A prop edit, reconciled in place.
            let second = build(Some(&first), src(11));
            inst.reload(&second);
            for _ in 0..5 {
                step(&mut inst);
            }
            settled("after a prop edit");
            // A hard reload: a new tree from the same build.
            inst.reload_hard(&second);
            for _ in 0..5 {
                step(&mut inst);
            }
            settled("after a hard reload");
            // Past the 5 s grace too: nothing was released for good.
            inst.tick(STOP_GRACE + Duration::from_secs(1));
            services.pump(&rt);
            settled("past the grace");
            // Give the service thread time to log anything it was sent.
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                *WIFI_LOG.lock().unwrap(),
                ["start scanning=false", "scanning=true (networks)"],
                "the scan was never switched off and on"
            );
            inst.shutdown();
            services.shutdown();
        }

        /// A service read only through a top-level `let` (or a `fn`) is
        /// held by the scopes that read the `let`, not by the top level:
        /// a closed popup showing it keeps it stopped, and its field
        /// unread; a top-level `on change` of a `let` holds it for good.
        #[test]
        fn a_let_read_only_by_a_closed_popup_holds_nothing() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "export state p = false\nlet lv = tally.level\nlet twice = lv * 2.0\nfn n() -> int { tally.items.count(i => true) }\nbar B {\n  text \"bar\"\n  popup { open: <-> p; text join(\" \", twice, n()) }\n}\n",
                tally.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            for _ in 0..5 {
                step(&mut inst);
            }
            assert_eq!(tally.readers(), 0, "nothing shown reads the `let`");
            assert!(!tally.running());
            assert_eq!(tally.starts(), 0);
            // The popup opens: it holds what `twice` and `n()` read.
            inst.set("svc.p", Value::Bool(true)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"0 0".to_string()) {
                assert!(Instant::now() < deadline, "the popup never showed");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(tally.readers(), 1);
            assert!(tally.running());
            assert_eq!(tally.field_readers(0), 1, "the popup reads the level");
            assert_eq!(tally.field_readers(1), 1, "and the items");
            inst.set("svc.p", Value::Bool(false)).unwrap();
            step(&mut inst);
            assert_eq!(tally.readers(), 0);
            assert_eq!(tally.field_readers(0), 0);
            inst.shutdown();

            // A top-level handler reading the `let` holds it all along.
            let (mut inst, _) = mount(
                &rt,
                SCHEMA,
                "export state seen = 0.0\nlet lv = tally.level\non change lv { seen = lv }\nbar B { text \"bar\" }\n",
                tally.dynamic(),
            );
            services.pump(&rt);
            inst.tick(Duration::ZERO);
            assert_eq!(tally.readers(), 1);
            assert_eq!(tally.field_readers(0), 1);
            inst.shutdown();
            services.shutdown();
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
            // Its actions and async method are its schema's, and so are
            // its item record's.
            super::assert_calls_match(&schema, &rt, &*client.dynamic());
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

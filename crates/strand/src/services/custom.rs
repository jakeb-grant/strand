//! [`CustomHost`]: the no-code services a config declares (`service ppd
//! from dbus system "net.hadess.PowerProfiles" { profile: text rw =
//! ActiveProfile }`, `from file`, `from listen`, `from poll`), each run by
//! `strand_services::custom` on the service contract, as a
//! [`ServiceHost`] (architecture.md, "Several service crates, one host":
//! declared custom services go to the member that implements their
//! source kind).
//!
//! The service reads untyped [`Data`]; each field's value here is a memo
//! converting it to the field's declared type ([`coerce`]): numbers from
//! text, enums by variant name, records by field name. A value that does
//! not convert is the type's default (null for an optional field), and is
//! reported once (log and `strand watch`) naming the field, its key and
//! the value, until a value converts again ([`Mismatch`]). An
//! `rw` field written (`ppd.profile = "performance"`, `<-> ppd.profile`)
//! is an item write of its value, so the service sets the property and
//! its echo is ignored. A reload that changes a declaration restarts only
//! that service ([`ServiceHost::restart`]); one that removes it stops it;
//! one that keeps it re-types its values as the new program numbers its
//! types ([`ServiceHost::retype`]) without restarting it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use strand_compiler::hir::{PollTarget, SourceSpec};
use strand_compiler::lower::CustomService;
use strand_compiler::ty::{Prim, Ty, TypeTable};
use strand_compiler::vm::host::{ActionTarget, PathSeg, ServiceHost};
use strand_compiler::vm::schema_host::default_value;
use strand_compiler::vm::{Num, Value};
use strand_core::{Error, Memo, NodeId, Runtime, Scope, Signal};
use strand_services::custom::{self, Custom, CustomValue, FieldSpec, Source, Spec};
use strand_services::{Client, Data, ServiceDiagnostic, Services, Step, ToData};

use super::convert::{to_data, to_value};

/// One declared service.
struct Entry {
    client: Client<Custom>,
    spec: i64,
    decl: CustomService,
    /// The types its fields are of, as the current program numbers them.
    typing: Rc<RefCell<Typing>>,
    /// Bumped when `typing` changes: the memos read it, so they convert
    /// again.
    epoch: Signal<u64>,
    values: Vec<Memo<Value>>,
    /// Owns the memos.
    scope: Scope,
}

/// A service's type table and its fields' types (in declaration order).
struct Typing {
    table: Rc<TypeTable>,
    fields: Vec<Ty>,
}

impl Typing {
    fn of(decl: &CustomService, types: &TypeTable) -> Typing {
        let record = types.record(decl.record);
        let fields = decl
            .fields
            .iter()
            .map(|f| {
                record
                    .fields
                    .iter()
                    .find(|d| d.name == f.name)
                    .map_or(Ty::Error, |d| d.ty.clone())
            })
            .collect();
        Typing {
            table: Rc::new(types.clone()),
            fields,
        }
    }
}

/// See the module docs.
pub struct CustomHost {
    services: Services,
    /// Where a relative `from file` path is (the config directory).
    config_dir: RefCell<Option<PathBuf>>,
    entries: RefCell<HashMap<String, Entry>>,
}

impl std::fmt::Debug for CustomHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<String> = self.entries.borrow().keys().cloned().collect();
        names.sort();
        f.debug_struct("CustomHost")
            .field("services", &names)
            .finish_non_exhaustive()
    }
}

/// `~/x` from the home directory, `x` from `dir`.
fn resolve(path: &str, dir: Option<&Path>) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(path);
    match dir {
        Some(d) if p.is_relative() => d.join(p),
        _ => p,
    }
}

/// The service-side spec of `decl`.
pub fn spec_of(decl: &CustomService, config_dir: Option<&Path>) -> Spec {
    let source = match &decl.source {
        SourceSpec::Dbus { system, name, path } => Source::Dbus {
            system: *system,
            name: name.clone(),
            path: path.clone(),
        },
        SourceSpec::File { path } => Source::File {
            path: resolve(path, config_dir),
        },
        SourceSpec::Listen { command } => Source::Listen {
            command: command.clone(),
        },
        SourceSpec::Poll { target, every } => Source::Poll {
            target: match target {
                PollTarget::Command(c) => custom::PollTarget::Command(c.clone()),
                PollTarget::File(f) => custom::PollTarget::File(resolve(f, config_dir)),
            },
            every: *every,
        },
    };
    Spec {
        name: decl.name.clone(),
        source,
        fields: decl
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.name.clone(),
                key: f.key.clone(),
                rw: f.rw,
            })
            .collect(),
    }
}

/// `d` as a value of type `ty`: see the module docs. `None`: it does not
/// convert.
pub fn coerce(types: &TypeTable, ty: &Ty, d: &Data) -> Option<Value> {
    if let Ty::Optional(inner) = ty {
        return match d {
            Data::Null => Some(Value::Null),
            d => coerce(types, inner, d),
        };
    }
    let text = |d: &Data| match d {
        Data::Text(t) => Some(t.trim().to_string()),
        _ => None,
    };
    let number = |d: &Data| -> Option<f64> {
        match d {
            Data::Int(n) => Some(*n as f64),
            Data::Float(f) => Some(*f),
            Data::Bool(b) => Some(f64::from(u8::from(*b))),
            Data::Text(t) => {
                let t = t.trim();
                t.parse::<f64>().ok().or_else(|| {
                    // A number and a unit (`45 °C`, `16303392 kB`).
                    t.split_whitespace().next()?.parse::<f64>().ok()
                })
            }
            _ => None,
        }
    };
    match (ty, d) {
        (_, Data::Null) => None,
        (Ty::Any, d) => Some(to_value(types, d)),
        (Ty::Prim(Prim::Bool), Data::Bool(b)) => Some(Value::Bool(*b)),
        (Ty::Prim(Prim::Bool), Data::Int(n)) => Some(Value::Bool(*n != 0)),
        (Ty::Prim(Prim::Bool), Data::Float(f)) if !f.is_nan() => Some(Value::Bool(*f != 0.0)),
        (Ty::Prim(Prim::Bool), Data::Text(t)) => match t.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "off" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        (Ty::Prim(Prim::Int), d) => number(d)
            .filter(|n| n.is_finite())
            .map(|n| Value::int(n.round() as i64)),
        (Ty::Prim(Prim::Float), d) => number(d).filter(|n| n.is_finite()).map(Value::float),
        // A fraction, as services hold percentages (`40%` in text is 0.4).
        (Ty::Prim(Prim::Percent), Data::Int(_) | Data::Float(_) | Data::Bool(_)) => {
            number(d).filter(|n| n.is_finite()).map(Value::float)
        }
        (Ty::Prim(Prim::Percent), Data::Text(t)) if !t.trim().ends_with('%') => {
            number(d).filter(|n| n.is_finite()).map(Value::float)
        }
        (Ty::Prim(Prim::Length), Data::Int(_) | Data::Float(_)) => number(d)
            .filter(|n| n.is_finite())
            .map(|n| Value::Num(n, Num::Px)),
        (Ty::Prim(Prim::Angle), Data::Int(_) | Data::Float(_)) => number(d)
            .filter(|n| n.is_finite())
            .map(|n| Value::Num(n, Num::Deg)),
        // Seconds; a count no duration holds (`1e300`, negative) is none.
        (Ty::Prim(Prim::Duration), Data::Int(_) | Data::Float(_)) => number(d)
            .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
            .map(Value::from),
        (Ty::Prim(Prim::Duration), Data::Duration(t)) => Some(Value::from(*t)),
        (Ty::Prim(Prim::Color), Data::Color(_)) => Some(to_value(types, d)),
        (Ty::Prim(Prim::Text | Prim::Path), d) => match d {
            Data::Text(t) => Some(Value::text(&**t)),
            Data::Int(n) => Some(Value::text(n.to_string())),
            Data::Float(f) => Some(Value::text(f.to_string())),
            Data::Bool(b) => Some(Value::text(b.to_string())),
            Data::Enum { variant, .. } => Some(Value::text(&**variant)),
            _ => None,
        },
        (Ty::Enum(e), Data::Text(_) | Data::Enum { .. }) => {
            let name = match d {
                Data::Enum { variant, .. } => variant.to_string(),
                _ => text(d)?,
            };
            let def = types.enum_(*e);
            def.variant(&name)
                .or_else(|| {
                    let lower = name.to_ascii_lowercase().replace('-', "_");
                    def.variant(&lower)
                })
                .map(|v| Value::Enum(*e, v))
        }
        (Ty::List(item, _), Data::List(items)) => Some(Value::list(
            items
                .iter()
                .filter_map(|i| coerce(types, item, i))
                .collect(),
        )),
        (Ty::Record(r), Data::Record { fields, .. }) => {
            let def = types.record(*r);
            let values = def
                .fields
                .iter()
                .map(|f| {
                    fields
                        .iter()
                        .find(|(n, _)| *n == f.name)
                        .and_then(|(_, d)| coerce(types, &f.ty, d))
                        .unwrap_or_else(|| default_value(types, &f.ty))
                })
                .collect();
            Some(Value::record(*r, values))
        }
        (ty, Data::Text(t)) => strand_compiler::instantiate::parse_text(types, ty, t.trim()),
        // Anything else does not convert: the type's default.
        _ => None,
    }
}

/// A field's value that does not convert to its declared type, reported
/// once (a log line and a `strand watch` notice naming the field, its key
/// and the value) until a value converts again, which resolves it.
struct Mismatch {
    services: Services,
    /// The service's declared name (`sensors`): its diagnostics' key.
    service: String,
    /// `sensors.cpu`.
    field: String,
    /// The key path it reads (`coretemp.temp1`).
    key: String,
    /// The message reported and not yet resolved.
    reported: RefCell<Option<String>>,
}

impl Mismatch {
    /// The field read `d`, which converted or not. No value yet (null)
    /// says nothing.
    fn saw(&self, types: &TypeTable, ty: &Ty, d: &Data, converted: bool) {
        if converted {
            let taken = self.reported.borrow_mut().take();
            if let Some(message) = taken {
                self.services.report(ServiceDiagnostic {
                    service: self.service.clone(),
                    message,
                    notice: false,
                    resolved: true,
                });
            }
            return;
        }
        if self.reported.borrow().is_some() || matches!(d, Data::Null) {
            return;
        }
        let message = format!(
            "`{}`: `{}` holds {}, which is not a `{}`; it reads as the type's default",
            self.field,
            self.key,
            shown(d),
            types.show(ty),
        );
        self.services.report(ServiceDiagnostic {
            service: self.service.clone(),
            message: message.clone(),
            notice: false,
            resolved: false,
        });
        *self.reported.borrow_mut() = Some(message);
    }
}

/// `d` as a message shows it: short, quoted text, kinds for the rest.
fn shown(d: &Data) -> String {
    match d {
        Data::Text(t) if t.chars().count() > 60 => {
            let cut: String = t.chars().take(60).collect();
            format!("{cut:?}…")
        }
        Data::Text(t) => format!("{:?}", &**t),
        Data::Int(n) => n.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::Null => "null".into(),
        d => format!("a {}", d.kind()),
    }
}

impl CustomHost {
    /// Custom services on `services` (they share its runtime and
    /// lifecycle), relative `from file` paths under `config_dir`.
    pub fn new(services: Services, config_dir: Option<PathBuf>) -> CustomHost {
        CustomHost {
            services,
            config_dir: RefCell::new(config_dir),
            entries: RefCell::new(HashMap::new()),
        }
    }

    /// Resolve relative `from file` paths under `dir` from now on.
    pub fn set_config_dir(&self, dir: Option<PathBuf>) {
        *self.config_dir.borrow_mut() = dir;
    }

    /// Whether `name` is a service declared here.
    pub fn serves(&self, name: &str) -> bool {
        self.entries.borrow().contains_key(name)
    }

    /// The client running `name` (tests).
    #[cfg(test)]
    pub fn client(&self, name: &str) -> Option<Client<Custom>> {
        self.entries.borrow().get(name).map(|e| e.client.clone())
    }

    /// Seed `client` for `decl` and build the memos converting its
    /// values.
    fn mount(
        &self,
        rt: &Runtime,
        client: Client<Custom>,
        decl: &CustomService,
        types: &TypeTable,
    ) -> Entry {
        let spec = custom::register(spec_of(decl, self.config_dir.borrow().as_deref()));
        let n = decl.fields.len();
        if let Err(e) = client.seed(rt, |s| *s = Custom::seeded(spec, n)) {
            log::warn!("{}: {e}", decl.name);
        }
        let typing = Rc::new(RefCell::new(Typing::of(decl, types)));
        let (scope, (epoch, values)) = rt.scope(|rt| {
            let epoch = rt.signal(0u64);
            let values = (0..n)
                .map(|i| {
                    let (client, typing) = (client.clone(), typing.clone());
                    let name = format!("{}.{}", decl.name, decl.fields[i].name);
                    let report = Mismatch {
                        services: self.services.clone(),
                        service: decl.name.clone(),
                        field: name.clone(),
                        key: decl.fields[i].key.join("."),
                        reported: RefCell::new(None),
                    };
                    let m = rt.memo(move |rt| {
                        epoch.get(rt)?;
                        let d = client
                            .cells()
                            .values
                            .with(rt, |v| v.get(&(i as i64)).map(|v| v.value.clone()))?
                            .unwrap_or_default();
                        let typing = typing.borrow();
                        let (types, ty) = (&*typing.table, &typing.fields[i]);
                        let v = coerce(types, ty, &d);
                        report.saw(types, ty, &d, v.is_some());
                        Ok(v.unwrap_or_else(|| default_value(types, ty)))
                    });
                    rt.set_name(m.id(), name);
                    m
                })
                .collect::<Vec<Memo<Value>>>();
            (epoch, values)
        });
        Entry {
            client,
            spec,
            decl: decl.clone(),
            typing,
            epoch,
            values,
            scope,
        }
    }

    fn index(&self, service: &str, field: &str) -> Result<usize, Error> {
        self.entries
            .borrow()
            .get(service)
            .and_then(|e| e.decl.fields.iter().position(|f| f.name == field))
            .ok_or_else(|| Error::failed(format!("`{service}` has no field `{field}`")))
    }

    /// Each service's starts, readers, running and raw values (tests).
    #[cfg(test)]
    pub fn debug_state(&self, rt: &Runtime) -> Vec<String> {
        self.entries
            .borrow()
            .iter()
            .map(|(n, e)| {
                format!(
                    "{n}: starts {} readers {} running {} values {:?}",
                    e.client.starts(),
                    e.client.readers(),
                    e.client.running(),
                    e.client
                        .cells()
                        .values
                        .get_untracked(rt)
                        .map(|v| v.items().to_vec())
                )
            })
            .collect()
    }

    /// Stop every service and dispose the cells (shutdown).
    pub fn dispose(&self, rt: &Runtime) {
        for (_, e) in self.entries.borrow_mut().drain() {
            custom::forget(e.spec);
            e.scope.dispose(rt);
            e.client.unregister(rt);
        }
    }
}

impl ServiceHost for CustomHost {
    fn declare(&self, rt: &Runtime, decl: &CustomService, types: &TypeTable) {
        let existing = self
            .entries
            .borrow()
            .get(&decl.name)
            .map(|e| e.decl == *decl);
        match existing {
            // The same declaration again (a hard reload mounts afresh):
            // the running service carries on.
            Some(true) => {}
            Some(false) => self.restart(rt, decl, types),
            None => {
                let client = self.services.register_as::<Custom>(rt, &decl.name);
                let entry = self.mount(rt, client, decl, types);
                self.entries.borrow_mut().insert(decl.name.clone(), entry);
            }
        }
    }

    fn restart(&self, rt: &Runtime, decl: &CustomService, types: &TypeTable) {
        let old = self.entries.borrow_mut().remove(&decl.name);
        let Some(old) = old else {
            return self.declare(rt, decl, types);
        };
        custom::forget(old.spec);
        old.scope.dispose(rt);
        let entry = self.mount(rt, old.client.clone(), decl, types);
        old.client.restart(rt);
        self.entries.borrow_mut().insert(decl.name.clone(), entry);
    }

    fn retype(&self, rt: &Runtime, decl: &CustomService, types: &TypeTable) {
        let same = {
            let entries = self.entries.borrow();
            let Some(e) = entries.get(&decl.name) else {
                drop(entries);
                return self.declare(rt, decl, types);
            };
            e.decl.source == decl.source && e.decl.fields == decl.fields
        };
        if !same {
            // Not just renumbered: the declaration changed after all.
            return self.restart(rt, decl, types);
        }
        let epoch = {
            let mut entries = self.entries.borrow_mut();
            let Some(e) = entries.get_mut(&decl.name) else {
                return;
            };
            e.decl = decl.clone();
            *e.typing.borrow_mut() = Typing::of(decl, types);
            e.epoch
        };
        if let Err(e) = epoch.update(rt, |n| *n += 1) {
            log::warn!("{}: {e}", decl.name);
        }
    }

    fn stop(&self, rt: &Runtime, name: &str) {
        let old = self.entries.borrow_mut().remove(name);
        if let Some(old) = old {
            custom::forget(old.spec);
            old.scope.dispose(rt);
            old.client.unregister(rt);
        }
    }

    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        let i = self.index(service, field)?;
        let m = self.entries.borrow().get(service).map(|e| e.values[i]);
        match m {
            Some(m) => m.get(rt),
            None => Err(Error::failed(format!("no service `{service}`"))),
        }
    }

    fn sources(&self, _rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        let entries = self.entries.borrow();
        let Some(e) = entries.get(service) else {
            return Vec::new();
        };
        match field.and_then(|f| e.decl.fields.iter().position(|d| d.name == f)) {
            Some(i) => vec![e.values[i].id()],
            None => e.values.iter().map(|m| m.id()).collect(),
        }
    }

    fn write(
        &self,
        rt: &Runtime,
        service: &str,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        let Some(PathSeg::Field(field)) = path.first() else {
            return Err(Error::failed(format!("`{service}` is written by field")));
        };
        let i = self.index(service, field)?;
        let (client, table, rw) = {
            let entries = self.entries.borrow();
            let e = entries
                .get(service)
                .ok_or_else(|| Error::failed(format!("no service `{service}`")))?;
            (
                e.client.clone(),
                e.typing.borrow().table.clone(),
                e.decl.fields[i].rw,
            )
        };
        if !rw {
            return Err(Error::failed(format!("`{service}.{field}` is read-only")));
        }
        // A leaf below the field: the field's whole new value is written
        // (a property is set whole).
        let current = client
            .cells()
            .values
            .get_key(rt, &(i as i64))?
            .unwrap_or(CustomValue {
                index: i as i64,
                value: Data::Null,
            });
        let new = if path.len() > 1 {
            let steps: Vec<Step> = super::convert::steps(&path[1..]);
            current
                .value
                .with_path(&steps, to_data(&table, &value))
                .map_err(|e| Error::failed(e.to_string()))?
        } else {
            to_data(&table, &value)
        };
        client.dynamic().write_item(
            rt,
            "CustomValue",
            &current.to_data(),
            &[Step::Field("value".into())],
            new,
        )
    }

    fn action(
        &self,
        _rt: &Runtime,
        _target: ActionTarget<'_>,
        name: &str,
        _args: &[Value],
    ) -> Result<(), Error> {
        Err(Error::failed(format!(
            "a no-code service has no action `{name}`"
        )))
    }

    fn call(
        &self,
        _rt: &Runtime,
        service: &str,
        method: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        Err(Error::failed(format!(
            "`{service}` has no method `{method}`"
        )))
    }

    fn event(
        &self,
        _rt: &Runtime,
        _service: &str,
        _event: &str,
    ) -> Option<strand_core::EventQueue<Vec<Value>>> {
        None
    }

    fn acquire(&self, rt: &Runtime, service: &str) {
        let c = self.entries.borrow().get(service).map(|e| e.client.clone());
        if let Some(c) = c {
            c.acquire(rt);
        }
    }

    fn release(&self, rt: &Runtime, service: &str) {
        let c = self.entries.borrow().get(service).map(|e| e.client.clone());
        if let Some(c) = c {
            c.release(rt);
        }
    }
}

/// D-Bus introspection for the compiler's `from dbus` check
/// ([`strand_compiler::check::dbus`]), on the environment's buses (a test
/// points `DBUS_SYSTEM_BUS_ADDRESS` and `DBUS_SESSION_BUS_ADDRESS` at a
/// private bus). Answers, failures included, are remembered for
/// [`strand_introspect::TTL`] ([`strand_introspect::Cache`], shared with the
/// LSP's), so a reload burst asks once and a daemon that does not answer
/// costs one bounded wait. It waits for an answer (`strand check`, and
/// `strand run`'s boot) until [`DbusCheck::stop_waiting`]; from then on it
/// answers from what it remembers, asks again off the caller's thread,
/// and calls its `recheck` when an answer differs ([`dbus_check`]).
#[derive(Default)]
pub struct BusIntrospector {
    cache: Arc<strand_introspect::Cache>,
    /// Not waiting: what to call when a late answer differs.
    ask: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Still waiting for answers (`ask` is used once this is false).
    waits: Arc<AtomicBool>,
}

impl std::fmt::Debug for BusIntrospector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusIntrospector")
            .field("waits", &self.waits())
            .finish_non_exhaustive()
    }
}

impl BusIntrospector {
    fn waits(&self) -> bool {
        self.ask.is_none() || self.waits.load(Ordering::SeqCst)
    }
}

/// Introspected properties as the compiler's check reads them.
pub fn bus_properties(
    props: Vec<strand_introspect::Property>,
) -> Vec<strand_compiler::check::dbus::BusProperty> {
    props
        .into_iter()
        .map(|p| strand_compiler::check::dbus::BusProperty {
            interface: p.interface,
            name: p.name,
            signature: p.signature,
            writable: p.writable,
        })
        .collect()
}

impl strand_compiler::check::dbus::Introspect for BusIntrospector {
    fn properties(
        &self,
        system: bool,
        name: &str,
        path: &str,
    ) -> Option<Result<Vec<strand_compiler::check::dbus::BusProperty>, String>> {
        let bus = if system {
            strand_introspect::Bus::System
        } else {
            strand_introspect::Bus::Session
        };
        let answer = match &self.ask {
            Some(ask) if !self.waits() => {
                let ask = ask.clone();
                self.cache
                    .properties_or_ask(&bus, name, path, move || ask())?
            }
            _ => self.cache.properties(&bus, name, path),
        };
        Some(answer.map(bus_properties))
    }
}

/// Ends the waiting of a [`dbus_check`].
#[derive(Clone, Debug)]
pub struct DbusCheck(Arc<AtomicBool>);

impl DbusCheck {
    /// From now on the check never waits on a bus: a service whose answer
    /// is not in yet is not checked, and `recheck` is called once it is.
    pub fn stop_waiting(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The loader's extra check: `from dbus` services against introspection.
/// It waits for answers (the boot) until [`DbusCheck::stop_waiting`];
/// then a compile (a reload) never waits on a bus, and `recheck` (which
/// must have the loader check the running files again,
/// `Loader::recheck`) is called, on another thread, when an answer
/// comes in that differs from the one the compile used.
pub fn dbus_check(
    recheck: impl Fn() + Send + Sync + 'static,
) -> (strand_compiler::reconcile::loader::ExtraCheck, DbusCheck) {
    dbus_check_on(Arc::new(strand_introspect::Cache::default()), recheck)
}

/// [`dbus_check`] over `cache` (tests ask through a fake bus).
fn dbus_check_on(
    cache: Arc<strand_introspect::Cache>,
    recheck: impl Fn() + Send + Sync + 'static,
) -> (strand_compiler::reconcile::loader::ExtraCheck, DbusCheck) {
    let waits = Arc::new(AtomicBool::new(true));
    let intro = BusIntrospector {
        cache,
        ask: Some(Arc::new(recheck)),
        waits: waits.clone(),
    };
    (
        strand_compiler::reconcile::loader::ExtraCheck(Box::new(move |c| {
            strand_compiler::check::dbus::check(&c.program, &intro)
        })),
        DbusCheck(waits),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use strand_compiler::schema::Schema;
    use strand_services::Data;

    use super::*;

    /// Every declared type against every kind of value a source can
    /// say: a value converts, or is `None` (the type's default); none
    /// panics, and none lets a value of another type through.
    #[test]
    fn coerce_converts_or_refuses_every_type_and_value() {
        let types = &Schema::builtin().types;
        let urgency = Ty::Enum(types.find_enum("Urgency").unwrap());
        let variant = Ty::Enum(types.find_enum("Variant").unwrap());
        let range = Ty::Record(types.find_record("Range").unwrap());
        let ints = Ty::List(Box::new(Ty::INT), false);
        let record = |end: Data| Data::Record {
            ty: "Range".into(),
            fields: vec![("end".into(), end)],
        };
        let datas = [
            Data::Null,
            Data::Bool(true),
            Data::Int(3),
            Data::Float(2.5),
            Data::Float(1e300),
            Data::Float(-1.0),
            Data::Float(f64::NAN),
            Data::text("x"),
            Data::text("42"),
            Data::Duration(Duration::from_millis(1500)),
            Data::List(vec![Data::Int(1), Data::text("2"), Data::text("no")]),
            record(Data::Int(3)),
            Data::Enum {
                ty: "Urgency".into(),
                variant: "critical".into(),
            },
        ];
        let tys = [
            Ty::BOOL,
            Ty::INT,
            Ty::FLOAT,
            Ty::PERCENT,
            Ty::Prim(Prim::Duration),
            Ty::Prim(Prim::Text),
            Ty::Prim(Prim::Path),
            Ty::Prim(Prim::Color),
            Ty::Prim(Prim::Length),
            urgency.clone(),
            ints.clone(),
            range.clone(),
            Ty::Optional(Box::new(Ty::INT)),
            Ty::Any,
        ];
        // What each value's kind may convert to (`true`: some value).
        for ty in &tys {
            for d in &datas {
                let v = coerce(types, ty, d);
                let Some(v) = v else { continue };
                let ok = match (ty, &v) {
                    (Ty::Any, _) => true,
                    (Ty::Optional(_), Value::Null) => true,
                    (Ty::Optional(_) | Ty::Prim(Prim::Int), v) => v.as_f64().is_some(),
                    (Ty::Prim(Prim::Bool), Value::Bool(_)) => true,
                    (Ty::Prim(Prim::Float | Prim::Percent), v) => {
                        v.as_f64().is_some_and(f64::is_finite)
                    }
                    (Ty::Prim(Prim::Duration), Value::Num(n, Num::Ms)) => n.is_finite(),
                    (Ty::Prim(Prim::Text | Prim::Path), Value::Text(_)) => true,
                    (Ty::Prim(Prim::Color), Value::Color(_)) => true,
                    (Ty::Prim(Prim::Length), Value::Num(_, Num::Px)) => true,
                    (Ty::Enum(_), Value::Enum(..)) => true,
                    (Ty::List(..), Value::List(_)) => true,
                    (Ty::Record(_), Value::Record(..)) => true,
                    _ => false,
                };
                assert!(ok, "{ty:?} from {d:?} gave {v:?}");
            }
        }
        // The cases that do convert.
        let c = |ty: &Ty, d: Data| coerce(types, ty, &d);
        assert_eq!(c(&Ty::BOOL, Data::Float(2.5)), Some(Value::Bool(true)));
        assert_eq!(c(&Ty::BOOL, Data::Float(0.0)), Some(Value::Bool(false)));
        assert_eq!(c(&Ty::BOOL, Data::Float(f64::NAN)), None);
        assert_eq!(c(&Ty::BOOL, Data::text("on")), Some(Value::Bool(true)));
        assert_eq!(c(&Ty::BOOL, Data::List(vec![])), None);
        assert_eq!(c(&Ty::INT, Data::text("45 °C")), Some(Value::int(45)));
        assert_eq!(c(&Ty::INT, Data::Float(2.5)), Some(Value::int(3)));
        assert_eq!(c(&Ty::FLOAT, Data::Float(f64::NAN)), None);
        assert_eq!(c(&Ty::PERCENT, Data::Float(0.4)), Some(Value::float(0.4)));
        assert_eq!(
            c(&Ty::PERCENT, Data::text("40%")),
            Some(Value::Num(40.0, Num::Percent))
        );
        let dur = Ty::Prim(Prim::Duration);
        assert_eq!(
            c(&dur, Data::Float(1.5)),
            Some(Value::from(Duration::from_millis(1500)))
        );
        // Overflow and negative counts are none, not a panic.
        assert_eq!(c(&dur, Data::Float(1e300)), None);
        assert_eq!(c(&dur, Data::Float(-1.0)), None);
        assert_eq!(c(&dur, Data::Float(f64::INFINITY)), None);
        assert_eq!(
            c(&dur, Data::Int(i64::MAX)),
            Some(Value::from(Duration::from_secs_f64(i64::MAX as f64)))
        );
        assert_eq!(
            c(&dur, Data::text("200ms")),
            Some(Value::Num(200.0, Num::Ms))
        );
        assert_eq!(c(&dur, Data::Bool(true)), None);
        assert_eq!(
            c(&Ty::Prim(Prim::Color), Data::text("#ff0000")),
            Some(Value::Color(
                strand_scene::Color::from_hex("#ff0000").unwrap()
            ))
        );
        assert_eq!(c(&Ty::Prim(Prim::Color), Data::Int(3)), None);
        // Enums by name, any case, `-` as `_`; never from a number.
        let critical = c(&urgency, Data::text("Critical")).unwrap();
        assert!(matches!(critical, Value::Enum(..)));
        assert!(matches!(
            c(&variant, Data::text("tonal-spot")),
            Some(Value::Enum(..))
        ));
        assert_eq!(c(&urgency, Data::Int(3)), None);
        assert_eq!(c(&urgency, Data::text("nope")), None);
        // Lists keep the items that convert; records by field name.
        assert_eq!(
            c(
                &ints,
                Data::List(vec![Data::Int(1), Data::text("2"), Data::text("no")])
            ),
            Some(Value::list(vec![Value::int(1), Value::int(2)]))
        );
        assert_eq!(c(&ints, record(Data::Int(1))), None);
        let r = c(&range, record(Data::text("7"))).unwrap();
        assert_eq!(r.field(types, "end"), Some(&Value::int(7)));
        assert_eq!(r.field(types, "start"), Some(&Value::int(0)));
        assert_eq!(c(&range, Data::List(vec![])), None);
        assert_eq!(c(&range, Data::Int(1)), None);
        assert_eq!(c(&Ty::Prim(Prim::Text), Data::List(vec![])), None);
        assert_eq!(
            c(&Ty::Optional(Box::new(Ty::INT)), Data::Null),
            Some(Value::Null)
        );
    }

    /// `strand run`'s loader never waits on a bus after its boot: a save
    /// of a config whose `from dbus` daemon does not answer compiles at
    /// once (the remembered answer is used and asked again off the
    /// compiler's thread), and a service added by a reload is checked
    /// once its answer is in, through `recheck` and `Loader::recheck`.
    #[test]
    fn the_loader_checks_dbus_services_off_the_reload_path() {
        use std::sync::atomic::AtomicUsize;
        use strand_compiler::reconcile::loader::Loader;
        let dir = std::env::temp_dir().join(format!("strand-dbus-loader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.strand");
        let hung =
            "service thing from dbus session \"org.example.Hung\" { level: float = Level }\n";
        std::fs::write(&file, format!("{hung}bar B {{ text \"x\" }}\n")).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let n = asked.clone();
        let cache = Arc::new(strand_introspect::Cache::with_fetch(
            Duration::from_millis(50),
            move |_, name, _| {
                n.fetch_add(1, Ordering::SeqCst);
                if name == "org.example.Hung" {
                    std::thread::sleep(Duration::from_millis(1500));
                    return Err("no answer".into());
                }
                std::thread::sleep(Duration::from_millis(200));
                Ok(vec![strand_introspect::Property {
                    interface: "org.example.Late".into(),
                    name: "Level".into(),
                    signature: "d".into(),
                    readable: true,
                    writable: false,
                }])
            },
        ));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (check, waits) = dbus_check_on(cache, move || {
            let _ = tx.send(());
        });
        let mut loader =
            Loader::new(&dir, crate::services::schema().clone(), None).with_check(check);
        let t = std::time::Instant::now();
        let boot = loader.boot();
        assert!(boot.build.is_some());
        assert!(
            t.elapsed() >= Duration::from_millis(1400) && asked.load(Ordering::SeqCst) == 1,
            "the boot waits for the answer"
        );
        waits.stop_waiting();
        // Past the remembered answer's ttl: a markup save does not wait.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&file, format!("{hung}bar B {{ text \"y\" }}\n")).unwrap();
        let t = std::time::Instant::now();
        let out = loader.changed([(file.clone(), true)]);
        assert!(
            t.elapsed() < Duration::from_millis(700),
            "the reload waited on the bus: {:?}",
            t.elapsed()
        );
        assert!(out.build.is_some(), "{:?}", out.diagnostics);
        // A service added by a reload: not checked yet, committed; its
        // late answer (a read-only property written `rw`) checks again.
        let late =
            "service late from dbus session \"org.example.Late\" { level: float rw = Level }\n";
        std::fs::write(&file, format!("{hung}{late}bar B {{ text \"y\" }}\n")).unwrap();
        let t = std::time::Instant::now();
        let out = loader.changed([(file.clone(), true)]);
        assert!(
            t.elapsed() < Duration::from_millis(700),
            "{:?}",
            t.elapsed()
        );
        assert!(out.build.is_some(), "{:?}", out.diagnostics);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the late answer asks for a check");
        let out = loader.recheck();
        assert!(out.build.is_none(), "the running program is unchanged");
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == "check::dbus_read_only"),
            "{:?}",
            out.diagnostics
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

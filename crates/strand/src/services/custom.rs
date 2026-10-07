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
//! not convert is the type's default (null for an optional field). An
//! `rw` field written (`ppd.profile = "performance"`, `<-> ppd.profile`)
//! is an item write of its value, so the service sets the property and
//! its echo is ignored. A reload that changes a declaration restarts only
//! that service ([`ServiceHost::restart`]); one that removes it stops it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use strand_compiler::hir::{PollTarget, SourceSpec};
use strand_compiler::lower::CustomService;
use strand_compiler::ty::{Prim, Ty, TypeTable};
use strand_compiler::vm::Value;
use strand_compiler::vm::host::{ActionTarget, PathSeg, ServiceHost};
use strand_compiler::vm::schema_host::default_value;
use strand_core::{Error, Memo, NodeId, Runtime, Scope};
use strand_services::custom::{self, Custom, CustomValue, FieldSpec, Source, Spec};
use strand_services::{Client, Data, Services, Step, ToData};

use super::convert::{to_data, to_value};

/// One declared service.
struct Entry {
    client: Client<Custom>,
    spec: i64,
    decl: CustomService,
    /// The types its fields are of.
    table: Rc<TypeTable>,
    values: Vec<Memo<Value>>,
    /// Owns the memos.
    scope: Scope,
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
        (Ty::Prim(Prim::Bool), Data::Bool(b)) => Some(Value::Bool(*b)),
        (Ty::Prim(Prim::Bool), Data::Int(n)) => Some(Value::Bool(*n != 0)),
        (Ty::Prim(Prim::Bool), Data::Text(t)) => match t.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "off" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        (Ty::Prim(Prim::Int), d) => number(d)
            .filter(|n| n.is_finite())
            .map(|n| Value::int(n.round() as i64)),
        (Ty::Prim(Prim::Float), d) => number(d).map(Value::float),
        (Ty::Prim(Prim::Duration), Data::Int(_) | Data::Float(_)) => number(d)
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|s| Value::from(std::time::Duration::from_secs_f64(s))),
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
        (_, d) => Some(to_value(types, d)),
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
        let record = types.record(decl.record);
        let field_types: Vec<Ty> = decl
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
        let shared = Rc::new(types.clone());
        let (scope, values) = rt.scope(|rt| {
            field_types
                .iter()
                .enumerate()
                .map(|(i, ty)| {
                    let (client, ty, types) = (client.clone(), ty.clone(), shared.clone());
                    let name = format!("{}.{}", decl.name, decl.fields[i].name);
                    let m = rt.memo(move |rt| {
                        let d = client
                            .cells()
                            .values
                            .with(rt, |v| v.get(&(i as i64)).map(|v| v.value.clone()))?
                            .unwrap_or_default();
                        Ok(coerce(&types, &ty, &d).unwrap_or_else(|| default_value(&types, &ty)))
                    });
                    rt.set_name(m.id(), name);
                    m
                })
                .collect::<Vec<Memo<Value>>>()
        });
        Entry {
            client,
            spec,
            decl: decl.clone(),
            table: shared,
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
            e.client.stop_now(rt);
            custom::forget(e.spec);
            e.scope.dispose(rt);
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
                let client = self.services.register::<Custom>(rt);
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

    fn stop(&self, rt: &Runtime, name: &str) {
        let old = self.entries.borrow_mut().remove(name);
        if let Some(old) = old {
            old.client.stop_now(rt);
            custom::forget(old.spec);
            old.scope.dispose(rt);
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
            (e.client.clone(), e.table.clone(), e.decl.fields[i].rw)
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
/// [`INTROSPECT_TTL`], so a reload burst (or an LSP's keystrokes) asks
/// once and a daemon that does not answer costs one bounded wait.
#[derive(Debug, Default)]
pub struct BusIntrospector {
    seen: std::sync::Mutex<HashMap<Object, Seen>>,
}

type Answer = Result<Vec<strand_compiler::check::dbus::BusProperty>, String>;

/// Bus (system?), name and path.
type Object = (bool, String, String);

/// When it was asked, and the answer.
type Seen = (std::time::Instant, Answer);

/// How long an introspection answer is reused.
pub const INTROSPECT_TTL: std::time::Duration = std::time::Duration::from_secs(10);

impl strand_compiler::check::dbus::Introspect for BusIntrospector {
    fn properties(&self, system: bool, name: &str, path: &str) -> Answer {
        let key = (system, name.to_string(), path.to_string());
        if let Ok(seen) = self.seen.lock()
            && let Some((at, answer)) = seen.get(&key)
            && at.elapsed() < INTROSPECT_TTL
        {
            return answer.clone();
        }
        let bus = if system {
            strand_introspect::Bus::System
        } else {
            strand_introspect::Bus::Session
        };
        let answer: Answer = strand_introspect::properties(&bus, name, path).map(|props| {
            props
                .into_iter()
                .map(|p| strand_compiler::check::dbus::BusProperty {
                    interface: p.interface,
                    name: p.name,
                    signature: p.signature,
                    writable: p.writable,
                })
                .collect()
        });
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(key, (std::time::Instant::now(), answer.clone()));
        }
        answer
    }
}

/// The loader's extra check: `from dbus` services against introspection.
pub fn dbus_check() -> strand_compiler::reconcile::loader::ExtraCheck {
    let intro = BusIntrospector::default();
    strand_compiler::reconcile::loader::ExtraCheck(Box::new(move |c| {
        strand_compiler::check::dbus::check(&c.program, &intro)
    }))
}

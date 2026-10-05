//! A [`ServiceHost`] with every service of the schema, populated from it.
//!
//! Each field of each service is a `Signal<Value>` holding the field
//! type's default (false, 0, "", empty lists, null), created up front so a
//! binding reading `battery.percent` depends on exactly that field. Tests
//! set fields ([`SchemaHost::set`]), emit events ([`SchemaHost::emit`])
//! and read the actions a program ran ([`SchemaHost::actions`]): this is
//! the deterministic mock. With [`SchemaHost::real`] the `clock` and
//! `calendar` services run on the wall clock, which is what the hello bar
//! needs before the M3 service crates replace the rest.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::FixedOffset;
use strand_core::{Error, EventQueue, KeyedSignal, Memo, NodeId, Runtime, Signal};

use super::builtins::default_of;
use super::clock::{Clock, Zone};
use super::host::{ActionTarget, Fetch, PathSeg, ServiceHost};
use super::value::{AsyncValue, Value, ValueKey, keyed_vec, list_of};
use crate::ty::{RecordId, Ty, TypeTable};

/// The mock clock's time: 2026-10-05 09:41:07 UTC (a Monday).
pub const MOCK_TIME: u64 = 1_791_193_267;

/// An action a program ran, as the host saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionCall {
    /// `notifications` for a service action, else the item's record name
    /// and key: `Workspace(2)`.
    pub target: String,
    pub name: String,
    pub args: Vec<Value>,
}

impl std::fmt::Display for ActionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}({})", self.target, self.name, self.args.len())
    }
}

/// Event queues by (service, event).
type Events = HashMap<(String, String), EventQueue<Vec<Value>>>;

/// One field of a service: a plain value, or a keyed collection (a list
/// of records with a key) that `for` follows by diffs.
#[derive(Clone, Copy, Debug)]
enum Field {
    Plain(Signal<Value>),
    /// The collection, and its items as one list value for plain reads.
    Keyed(KeyedSignal<ValueKey, Value>, Memo<Value>),
}

/// A `rw` write the mock saw: `audio.sink.volume` and the value.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteCall {
    pub path: String,
    pub value: Value,
}

/// A held fetch: its method, whether it was released, its task's waker.
type HeldFetch = (
    String,
    Rc<Cell<bool>>,
    Rc<RefCell<Option<std::task::Waker>>>,
);

/// Fetches the mock holds pending ([`SchemaHost::hold`]).
#[derive(Default)]
struct Held {
    methods: std::collections::BTreeSet<String>,
    /// Released when the method is released.
    waiting: Vec<HeldFetch>,
}

/// See the module docs.
pub struct SchemaHost {
    types: Rc<TypeTable>,
    services: RefCell<BTreeMap<String, RecordId>>,
    fields: RefCell<HashMap<(String, String), Field>>,
    writes: Rc<RefCell<Vec<WriteCall>>>,
    held: RefCell<Held>,
    events: RefCell<Events>,
    clock: Option<Clock>,
    /// The mock logs every action for tests; the real host keeps no
    /// history (a long-running shell would grow it forever).
    record_actions: bool,
    actions: RefCell<Vec<ActionCall>>,
    refs: RefCell<BTreeMap<String, i64>>,
}

impl std::fmt::Debug for SchemaHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaHost")
            .field("services", &self.services.borrow().len())
            .finish_non_exhaustive()
    }
}

fn fail(msg: impl Into<String>) -> Error {
    Error::failed(msg.into())
}

impl SchemaHost {
    /// Every schema service with defaults and no clock.
    pub fn new(rt: &Runtime, types: &TypeTable, clock: Option<Clock>) -> SchemaHost {
        let host = SchemaHost {
            types: Rc::new(types.clone()),
            services: RefCell::default(),
            fields: RefCell::default(),
            writes: Rc::default(),
            held: RefCell::default(),
            events: RefCell::default(),
            clock,
            record_actions: false,
            actions: RefCell::default(),
            refs: RefCell::default(),
        };
        for (name, &r) in &crate::schema::Schema::builtin().services {
            host.add_service(rt, name, r);
        }
        host
    }

    /// The deterministic mock: the clock fixed at [`MOCK_TIME`] in UTC.
    pub fn mock(rt: &Runtime, types: &TypeTable) -> SchemaHost {
        let utc = FixedOffset::east_opt(0).map_or(Zone::Local, Zone::Fixed);
        let clock = Clock::new(rt, types, utc, UNIX_EPOCH + Duration::from_secs(MOCK_TIME));
        let mut host = SchemaHost::new(rt, types, Some(clock));
        host.record_actions = true;
        host
    }

    /// The runtime host: the real clock and calendar in local time, every
    /// other service at its defaults until M3.
    pub fn real(rt: &Runtime, types: &TypeTable) -> SchemaHost {
        let clock = Clock::new(rt, types, Zone::Local, SystemTime::now());
        SchemaHost::new(rt, types, Some(clock))
    }

    fn add_service(&self, rt: &Runtime, name: &str, r: RecordId) {
        let def = self.types.record(r).clone();
        // Created with no owner, so a service outlives whatever first read
        // it.
        rt.untrack(|rt| {
            for f in &def.fields {
                let field = match self.list_key(&f.ty) {
                    Some(path) => {
                        let k = rt.keyed(keyed_vec(self.types.clone(), |t| &**t, path));
                        rt.set_name(k.id(), format!("{name}.{}", f.name));
                        let list = rt.memo(move |rt| k.with(rt, list_of));
                        Field::Keyed(k, list)
                    }
                    None => {
                        let s = rt.signal(default_of(&self.types, &f.ty));
                        rt.set_name(s.id(), format!("{name}.{}", f.name));
                        Field::Plain(s)
                    }
                };
                self.fields
                    .borrow_mut()
                    .insert((name.to_string(), f.name.clone()), field);
            }
            for e in &def.events {
                let q = rt.events::<Vec<Value>>();
                self.events
                    .borrow_mut()
                    .insert((name.to_string(), e.name.clone()), q);
            }
        });
        self.services.borrow_mut().insert(name.to_string(), r);
    }

    /// A list of keyed records is published as a keyed collection: its
    /// key path.
    fn list_key(&self, ty: &Ty) -> Option<Option<Vec<String>>> {
        let Ty::List(item, true) = ty else {
            return None;
        };
        match &**item {
            Ty::Record(r) => self.types.record(*r).key.clone().map(Some),
            _ => None,
        }
    }

    pub fn types(&self) -> &TypeTable {
        &self.types
    }

    fn field(&self, service: &str, field: &str) -> Result<Field, Error> {
        self.fields
            .borrow()
            .get(&(service.to_string(), field.to_string()))
            .copied()
            .ok_or_else(|| fail(format!("`{service}` has no field `{field}`")))
    }

    fn signal(&self, service: &str, field: &str) -> Result<Signal<Value>, Error> {
        match self.field(service, field)? {
            Field::Plain(s) => Ok(s),
            Field::Keyed(..) => Err(fail(format!("`{service}.{field}` is a keyed list"))),
        }
    }

    /// Set `service.field` or a field inside it (`audio.sink.volume`): the
    /// service reporting a value. A keyed list is replaced by key, so the
    /// change reaches readers as diffs.
    pub fn set(&self, rt: &Runtime, path: &str, value: Value) -> Result<(), Error> {
        let mut parts = path.split('.');
        let (Some(service), Some(field)) = (parts.next(), parts.next()) else {
            return Err(fail(format!("`{path}` is not a service field")));
        };
        let rest: Vec<&str> = parts.collect();
        match self.field(service, field)? {
            Field::Keyed(k, _) if rest.is_empty() => {
                let items = value.as_list().map(<[Value]>::to_vec).unwrap_or_default();
                k.replace_all(rt, items)
            }
            Field::Keyed(..) => Err(fail(format!("`{path}`: set the whole list"))),
            Field::Plain(sig) => {
                if rest.is_empty() {
                    return sig.set(rt, value);
                }
                let cur = sig.get_untracked(rt)?;
                let new = self.set_in(&cur, &rest, value)?;
                sig.set(rt, new)
            }
        }
    }

    /// The `rw` writes the program made, oldest first; clears the log
    /// (only the mock records them).
    pub fn take_writes(&self) -> Vec<WriteCall> {
        std::mem::take(&mut *self.writes.borrow_mut())
    }

    /// Hold `service.method` fetches pending until [`SchemaHost::release_fetch`]
    /// (tests of `??` over a pending `Async`).
    pub fn hold(&self, method: &str) {
        self.held.borrow_mut().methods.insert(method.to_string());
    }

    /// Let held fetches of `service.method` finish (each wakes its task).
    pub fn release_fetch(&self, method: &str) {
        let mut h = self.held.borrow_mut();
        h.methods.remove(method);
        h.waiting.retain(|(m, done, waker)| {
            if m != method {
                return true;
            }
            done.set(true);
            if let Some(w) = waker.borrow_mut().take() {
                w.wake();
            }
            false
        });
    }

    /// A shared handle on the write log for `write_tagged`'s send.
    fn writes_log(&self) -> std::rc::Weak<RefCell<Vec<WriteCall>>> {
        Rc::downgrade(&self.writes)
    }

    fn set_path(&self, cur: &Value, path: &[PathSeg], value: Value) -> Result<Value, Error> {
        let Some((first, rest)) = path.split_first() else {
            return Ok(value);
        };
        match first {
            PathSeg::Field(name) => {
                let Value::Record(r) = cur else {
                    return Err(fail(format!("no record to set `{name}` in")));
                };
                let def = self.types.record(r.ty);
                let i = def
                    .fields
                    .iter()
                    .position(|f| f.name == *name)
                    .ok_or_else(|| fail(format!("`{}` has no field `{name}`", def.name)))?;
                let mut fields = r.fields.clone();
                fields[i] = self.set_path(&fields[i], rest, value)?;
                Ok(Value::record(r.ty, fields))
            }
            PathSeg::Index(i) => {
                let Some(list) = cur.as_list() else {
                    return Err(fail("cannot index a missing list"));
                };
                if *i >= list.len() {
                    return Err(fail(format!("index {i} out of range for {}", list.len())));
                }
                let mut items = list.to_vec();
                items[*i] = self.set_path(&items[*i], rest, value)?;
                Ok(Value::list(items))
            }
        }
    }

    fn set_in(&self, cur: &Value, path: &[&str], value: Value) -> Result<Value, Error> {
        let Some((first, rest)) = path.split_first() else {
            return Ok(value);
        };
        let Value::Record(r) = cur else {
            return Err(fail(format!("no record to set `{first}` in")));
        };
        let def = self.types.record(r.ty);
        let i = def
            .fields
            .iter()
            .position(|f| f.name == *first)
            .ok_or_else(|| fail(format!("`{}` has no field `{first}`", def.name)))?;
        let mut fields = r.fields.clone();
        fields[i] = self.set_in(&fields[i], rest, value)?;
        Ok(Value::record(r.ty, fields))
    }

    /// The current value of `service.field` (untracked).
    pub fn get(&self, rt: &Runtime, path: &str) -> Result<Value, Error> {
        let mut parts = path.split('.');
        let (Some(service), Some(field)) = (parts.next(), parts.next()) else {
            return Err(fail(format!("`{path}` is not a service field")));
        };
        let mut v = rt.untrack(|rt| self.read(rt, service, field))?;
        for p in parts {
            v = v.field(&self.types, p).cloned().unwrap_or(Value::Null);
        }
        Ok(v)
    }

    /// A record of the named type with the given fields, the rest at
    /// their defaults: `host.record("Workspace", &[("id", 1.into())])`.
    pub fn record(&self, name: &str, fields: &[(&str, Value)]) -> Value {
        let Some(r) = self.types.find_record(name) else {
            return Value::Null;
        };
        let def = self.types.record(r);
        let values = def
            .fields
            .iter()
            .map(|f| {
                fields
                    .iter()
                    .find(|(n, _)| *n == f.name)
                    .map_or_else(|| default_of(&self.types, &f.ty), |(_, v)| v.clone())
            })
            .collect();
        Value::record(r, values)
    }

    /// An enum value by type and variant name: `host.variant("Urgency",
    /// "critical")`.
    pub fn variant(&self, enum_name: &str, variant: &str) -> Value {
        self.types
            .enums
            .iter()
            .position(|e| e.name == enum_name)
            .and_then(|i| {
                let e = crate::ty::EnumId(i as u32);
                self.types
                    .enum_(e)
                    .variant(variant)
                    .map(|v| Value::Enum(e, v))
            })
            .unwrap_or(Value::Null)
    }

    /// Emit `service.event` with its arguments.
    pub fn emit(&self, rt: &Runtime, path: &str, args: Vec<Value>) -> Result<(), Error> {
        let (service, event) = path
            .split_once('.')
            .ok_or_else(|| fail(format!("`{path}` is not an event")))?;
        let q = self
            .events
            .borrow()
            .get(&(service.to_string(), event.to_string()))
            .copied()
            .ok_or_else(|| fail(format!("`{service}` has no event `{event}`")))?;
        q.emit(rt, args)
    }

    /// The actions run so far, oldest first; clears the log. Only the mock
    /// ([`SchemaHost::mock`]) records them.
    pub fn take_actions(&self) -> Vec<ActionCall> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    /// The actions run so far, without clearing.
    pub fn actions(&self) -> Vec<ActionCall> {
        self.actions.borrow().clone()
    }

    /// Readers currently holding `service` (acquire minus release).
    pub fn readers(&self, service: &str) -> i64 {
        self.refs.borrow().get(service).copied().unwrap_or(0)
    }

    /// Move the clock (the host loop's wall-clock wake-up).
    pub fn set_time(&self, rt: &Runtime, now: SystemTime) {
        if let Some(c) = &self.clock {
            c.set_time(rt, now);
        }
    }

    fn item_name(&self, v: &Value) -> String {
        match v {
            Value::Record(r) => {
                let name = self.types.record(r.ty).name.clone();
                let key = v.identity(&self.types);
                if key == *v {
                    name
                } else {
                    format!("{name}({})", key.show(&self.types))
                }
            }
            v => v.show(&self.types),
        }
    }

    /// The mock's behaviour for actions it can model: a notification
    /// expired, dismissed or activated leaves the popups.
    fn simulate(&self, rt: &Runtime, item: &Value, name: &str) -> Result<(), Error> {
        let Value::Record(r) = item else {
            return Ok(());
        };
        if self.types.record(r.ty).name == "Notification"
            && matches!(name, "expire" | "dismiss" | "activate")
        {
            let key = ValueKey(item.identity(&self.types));
            match self.field("notifications", "popups")? {
                Field::Keyed(k, _) => {
                    if k.get_key(rt, &key)?.is_some() {
                        k.remove_key(rt, &key)?;
                    }
                }
                Field::Plain(sig) => {
                    let cur = sig.get_untracked(rt)?;
                    if let Some(list) = cur.as_list() {
                        let types = &self.types;
                        let kept: Vec<Value> = list
                            .iter()
                            .filter(|n| ValueKey(n.identity(types)) != key)
                            .cloned()
                            .collect();
                        if kept.len() != list.len() {
                            sig.set(rt, Value::list(kept))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl ServiceHost for SchemaHost {
    fn declare(&self, rt: &Runtime, name: &str, record: RecordId) {
        if !self.services.borrow().contains_key(name) {
            self.add_service(rt, name, record);
        }
    }

    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        if service == "clock"
            && let Some(c) = &self.clock
        {
            return c.read(rt, field);
        }
        match self.field(service, field)? {
            Field::Plain(s) => s.get(rt),
            Field::Keyed(_, list) => list.get(rt),
        }
    }

    fn sources(&self, _rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        let mut out = Vec::new();
        if matches!(service, "clock" | "calendar")
            && let Some(c) = &self.clock
        {
            out.extend(c.ids());
        }
        let ids = |f: &Field, out: &mut Vec<NodeId>| match f {
            Field::Plain(s) => out.push(s.id()),
            Field::Keyed(k, l) => {
                out.push(k.id());
                out.push(l.id());
            }
        };
        let fields = self.fields.borrow();
        match field {
            Some(f) => {
                if let Some(x) = fields.get(&(service.to_string(), f.to_string())) {
                    ids(x, &mut out);
                }
            }
            None => {
                for ((s, _), x) in fields.iter() {
                    if s == service {
                        ids(x, &mut out);
                    }
                }
            }
        }
        out
    }

    fn read_keyed(
        &self,
        _rt: &Runtime,
        service: &str,
        field: &str,
    ) -> Option<KeyedSignal<ValueKey, Value>> {
        match self.field(service, field).ok()? {
            Field::Keyed(k, _) => Some(k),
            Field::Plain(_) => None,
        }
    }

    fn write(
        &self,
        rt: &Runtime,
        service: &str,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        let Some((PathSeg::Field(field), rest)) = path.split_first() else {
            return Err(fail(format!("`{service}` cannot be written")));
        };
        let sig = self.signal(service, field)?;
        let cur = sig.get_untracked(rt)?;
        let new = self.set_path(&cur, rest, value.clone())?;
        let shown: String = path.iter().map(ToString::to_string).collect();
        let log = self.record_actions.then(|| WriteCall {
            path: format!("{service}{shown}"),
            value,
        });
        let writes = self.writes_log();
        // The mock's service applies every write as sent (no echo comes
        // back); a real one matches its reports with `receive`.
        sig.write_tagged(rt, new, move |_, _, _| {
            if let (Some(log), Some(w)) = (log, writes.upgrade()) {
                w.borrow_mut().push(log);
            }
        })?;
        Ok(())
    }

    fn fetch(&self, rt: &Runtime, service: &str, method: &str, args: Vec<Value>) -> Fetch {
        let name = format!("{service}.{method}");
        let r = self.call(rt, service, method, &args);
        let held = self.held.borrow().methods.contains(&name);
        let gate = held.then(|| {
            let done = Rc::new(Cell::new(false));
            let waker = Rc::new(RefCell::new(None));
            self.held
                .borrow_mut()
                .waiting
                .push((name, done.clone(), waker.clone()));
            (done, waker)
        });
        Box::pin(async move {
            if let Some((done, waker)) = gate {
                std::future::poll_fn(|cx| {
                    if done.get() {
                        std::task::Poll::Ready(())
                    } else {
                        *waker.borrow_mut() = Some(cx.waker().clone());
                        std::task::Poll::Pending
                    }
                })
                .await;
            }
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

    fn call(
        &self,
        rt: &Runtime,
        service: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        match (service, method) {
            ("clock", "format") => match &self.clock {
                Some(c) => c.format(rt, arg(0).as_text().unwrap_or("")),
                None => Ok(Value::text("")),
            },
            ("calendar", "days") => match &self.clock {
                Some(c) => c.days(rt, &self.types, &arg(0)),
                None => Ok(Value::list(Vec::new())),
            },
            ("workspaces", "on") => {
                let all = self.read(rt, "workspaces", "all")?;
                let screen = arg(0)
                    .field(&self.types, "name")
                    .cloned()
                    .unwrap_or(Value::Null);
                let items = all.as_list().unwrap_or(&[]);
                Ok(Value::list(
                    items
                        .iter()
                        .filter(|w| w.field(&self.types, "screen") == Some(&screen))
                        .cloned()
                        .collect(),
                ))
            }
            ("apps", "search") => {
                let q = arg(0).as_text().unwrap_or("").to_lowercase();
                let all = self.read(rt, "apps", "all")?;
                let (Some(hit), Some(range)) = (
                    self.types.find_record("Hit"),
                    self.types.find_record("Range"),
                ) else {
                    return Ok(Value::Null);
                };
                let mut hits = Vec::new();
                for app in all.as_list().unwrap_or(&[]) {
                    let name = app
                        .field(&self.types, "name")
                        .and_then(Value::as_text)
                        .unwrap_or("")
                        .to_string();
                    let Some(at) = name.to_lowercase().find(&q) else {
                        continue;
                    };
                    let score = if q.is_empty() {
                        0.0
                    } else {
                        q.len() as f64 / name.len().max(1) as f64
                    };
                    let ranges = if q.is_empty() {
                        Vec::new()
                    } else {
                        vec![Value::record(
                            range,
                            vec![Value::int(at as i64), Value::int((at + q.len()) as i64)],
                        )]
                    };
                    hits.push(Value::record(
                        hit,
                        vec![app.clone(), Value::float(score), Value::list(ranges)],
                    ));
                }
                Ok(Value::Async(Rc::new(AsyncValue::ready(Value::list(hits)))))
            }
            _ => Err(fail(format!("`{service}.{method}` is not available yet"))),
        }
    }

    fn action(
        &self,
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
    ) -> Result<(), Error> {
        let label = match target {
            ActionTarget::Service(s) => s.to_string(),
            ActionTarget::Item(v) if self.record_actions => self.item_name(v),
            ActionTarget::Item(_) => String::new(),
        };
        if self.record_actions {
            self.actions.borrow_mut().push(ActionCall {
                target: label,
                name: name.to_string(),
                args: args.to_vec(),
            });
        }
        if let ActionTarget::Item(v) = target {
            self.simulate(rt, v, name)?;
        }
        Ok(())
    }

    fn event(&self, _rt: &Runtime, service: &str, event: &str) -> Option<EventQueue<Vec<Value>>> {
        self.events
            .borrow()
            .get(&(service.to_string(), event.to_string()))
            .copied()
    }

    fn acquire(&self, service: &str) {
        *self
            .refs
            .borrow_mut()
            .entry(service.to_string())
            .or_default() += 1;
    }

    fn release(&self, service: &str) {
        *self
            .refs
            .borrow_mut()
            .entry(service.to_string())
            .or_default() -= 1;
    }

    fn next_wake(&self, rt: &Runtime) -> Option<SystemTime> {
        self.clock.as_ref().and_then(|c| c.next_wake(rt))
    }

    fn wake(&self, rt: &Runtime, now: SystemTime) {
        self.set_time(rt, now);
    }
}

/// The default value of a field type (re-exported for hosts built on
/// this one).
pub fn default_value(types: &TypeTable, ty: &Ty) -> Value {
    default_of(types, ty)
}

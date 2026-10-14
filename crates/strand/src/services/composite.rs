//! [`Composite`]: one [`ServiceHost`] over several (architecture.md,
//! "Several service crates, one host").
//!
//! Every call is routed by service name to the member serving it; an
//! item's action (`ws.focus()`) or write (`s.volume = 0.5` for `s` in
//! `audio.sinks`) goes to the member that hands out or takes that
//! record. Names no member serves (the clock and the calendar, the
//! services later tracks bring, `declare`d custom services) go to the
//! fallback, a [`SchemaHost`] answering at the schema's defaults.
//! `next_wake` is the earliest any member asks for; `wake` reaches all.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Instant, SystemTime};

use strand_compiler::lower::CustomService;
use strand_compiler::ty::TypeTable;
use strand_compiler::vm::host::{ActionCtx, ActionTarget, Fetch, PathSeg, ServiceHost};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Value, ValueKey};
use strand_core::{Error, EventQueue, KeyedSignal, NodeId, Runtime};

/// See the module docs.
pub struct Composite {
    members: Vec<Rc<dyn ServiceHost>>,
    /// Service name → member.
    by_name: HashMap<String, usize>,
    /// Record name → the member handing out its items, or whose actions
    /// take them.
    by_item: HashMap<String, usize>,
    fallback: Rc<SchemaHost>,
    /// The config's no-code services (`service … from dbus|file|listen|
    /// poll`), when the real services run.
    custom: Option<Rc<super::CustomHost>>,
    types: Rc<TypeTable>,
    /// (M6) Who took each hold (`acquire` and `acquire_field`, by service
    /// and field): a release goes to the same host, so a reload that
    /// adds or drops a config `service` shadowing a builtin one lets go
    /// of the builtin it held (spec.md 4.2).
    holds: RefCell<HashMap<HoldKey, Vec<Holder>>>,
    /// (M6) The press of the last action a handler ran with one, and when
    /// ([`Composite::action_press`]).
    action_press: std::cell::Cell<Option<(u32, Instant)>>,
}

/// A hold's service and field.
type HoldKey = (String, Option<String>);

/// Which host took a hold.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Holder {
    Custom,
    Member(usize),
    Fallback,
}

impl std::fmt::Debug for Composite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&String> = self.by_name.keys().collect();
        names.sort();
        f.debug_struct("Composite")
            .field("served", &names)
            .finish_non_exhaustive()
    }
}

impl Composite {
    /// Everything at `fallback` until members are added.
    pub fn new(fallback: Rc<SchemaHost>, types: Rc<TypeTable>) -> Composite {
        Composite {
            members: Vec::new(),
            by_name: HashMap::new(),
            by_item: HashMap::new(),
            fallback,
            custom: None,
            types,
            holds: RefCell::default(),
            action_press: std::cell::Cell::new(None),
        }
    }

    /// (M6) The press of the last action a handler ran with one (an
    /// `on secondary { item.menu.open() }`), and when it ran: a popup the
    /// service's answer opens (the tray's `opened`) was opened by that
    /// press (`run::PressCause`).
    pub fn action_press(&self) -> Option<(u32, Instant)> {
        self.action_press.get()
    }

    /// Serve declared custom services with `host` (else the fallback
    /// answers them at their defaults).
    pub fn set_custom(&mut self, host: Rc<super::CustomHost>) {
        self.custom = Some(host);
    }

    /// Add `host`, serving `names` and the item writes and actions of
    /// `items` (`DynService::item_records`). A name or a record belongs
    /// to one member: a second claim is refused (logged).
    pub fn add(&mut self, host: Rc<dyn ServiceHost>, names: &[&str], items: &[String]) {
        let i = self.members.len();
        self.members.push(host);
        for n in names {
            if self.by_name.contains_key(*n) {
                log::error!("service `{n}` is served twice; keeping the first");
                continue;
            }
            self.by_name.insert(n.to_string(), i);
        }
        for r in items {
            if self.by_item.contains_key(r) {
                log::error!("`{r}` items are served twice; keeping the first");
                continue;
            }
            self.by_item.insert(r.clone(), i);
        }
    }

    /// (M6, the owner's ruling) A declared custom service comes first: a
    /// config's `service battery` shadows the builtin `battery` (spec.md
    /// 4.2).
    fn route(&self, service: &str) -> &dyn ServiceHost {
        if let Some(c) = &self.custom
            && c.serves(service)
        {
            return &**c;
        }
        match self.by_name.get(service) {
            Some(&i) => &*self.members[i],
            None => &*self.fallback,
        }
    }

    /// The builtin `service`'s host, never a config's shadowing one.
    fn holder_builtin(&self, service: &str) -> Holder {
        match self.by_name.get(service) {
            Some(&i) => Holder::Member(i),
            None => Holder::Fallback,
        }
    }

    /// The host [`Composite::route`] picks, as a [`Holder`].
    fn holder(&self, service: &str) -> Holder {
        match &self.custom {
            Some(c) if c.serves(service) => Holder::Custom,
            _ => self.holder_builtin(service),
        }
    }

    fn host(&self, h: Holder) -> &dyn ServiceHost {
        match (h, &self.custom) {
            (Holder::Custom, Some(c)) => &**c,
            (Holder::Member(i), _) if i < self.members.len() => &*self.members[i],
            _ => &*self.fallback,
        }
    }

    /// Take a hold on `service` (`field`) at the host serving it, and
    /// remember which.
    fn take(&self, rt: &Runtime, service: &str, field: Option<&str>, h: Holder) {
        let host = self.host(h);
        match field {
            None => host.acquire(rt, service),
            Some(f) => host.acquire_field(rt, service, f),
        }
        self.holds
            .borrow_mut()
            .entry((service.to_string(), field.map(str::to_string)))
            .or_default()
            .push(h);
    }

    /// Let go of a hold where it was taken: a stale one first (taken at
    /// a host the name no longer routes to, by a scope a reload is
    /// disposing), else the last.
    fn give(&self, rt: &Runtime, service: &str, field: Option<&str>, now: Holder) {
        let key = (service.to_string(), field.map(str::to_string));
        let h = {
            let mut holds = self.holds.borrow_mut();
            let taken = holds.get_mut(&key).and_then(|v| {
                let at = v
                    .iter()
                    .rposition(|x| *x != now)
                    .unwrap_or(v.len().checked_sub(1)?);
                Some(v.remove(at))
            });
            if holds.get(&key).is_some_and(Vec::is_empty) {
                holds.remove(&key);
            }
            taken.unwrap_or(now)
        };
        let host = self.host(h);
        match field {
            None => host.release(rt, service),
            Some(f) => host.release_field(rt, service, f),
        }
    }

    /// Where custom service declarations go.
    fn declarer(&self) -> &dyn ServiceHost {
        match &self.custom {
            Some(c) => &**c,
            None => &*self.fallback,
        }
    }

    fn route_item(&self, item: &Value) -> &dyn ServiceHost {
        let member = match item {
            Value::Record(r) => self
                .types
                .records
                .get(r.ty.0 as usize)
                .and_then(|def| self.by_item.get(&def.name)),
            _ => None,
        };
        match member {
            Some(&i) => &*self.members[i],
            None => &*self.fallback,
        }
    }
}

impl ServiceHost for Composite {
    fn declare(&self, rt: &Runtime, service: &CustomService, types: &TypeTable) {
        self.declarer().declare(rt, service, types);
    }

    fn restart(&self, rt: &Runtime, service: &CustomService, types: &TypeTable) {
        self.declarer().restart(rt, service, types);
    }

    fn retype(&self, rt: &Runtime, service: &CustomService, types: &TypeTable) {
        self.declarer().retype(rt, service, types);
    }

    fn stop(&self, rt: &Runtime, name: &str) {
        self.declarer().stop(rt, name);
    }

    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        self.route(service).read(rt, service, field)
    }

    fn sources(&self, rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        self.route(service).sources(rt, service, field)
    }

    fn read_builtin(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        self.host(self.holder_builtin(service))
            .read_builtin(rt, service, field)
    }

    fn sources_builtin(&self, rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        self.host(self.holder_builtin(service))
            .sources_builtin(rt, service, field)
    }

    fn acquire_builtin(&self, rt: &Runtime, service: &str) {
        self.host(self.holder_builtin(service))
            .acquire_builtin(rt, service);
    }

    fn release_builtin(&self, rt: &Runtime, service: &str) {
        self.host(self.holder_builtin(service))
            .release_builtin(rt, service);
    }

    fn action_writes(&self, rt: &Runtime, service: &str) -> Vec<NodeId> {
        self.route(service).action_writes(rt, service)
    }

    fn read_keyed(
        &self,
        rt: &Runtime,
        service: &str,
        field: &str,
    ) -> Option<KeyedSignal<ValueKey, Value>> {
        self.route(service).read_keyed(rt, service, field)
    }

    fn write(
        &self,
        rt: &Runtime,
        service: &str,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        self.route(service).write(rt, service, path, value)
    }

    fn write_item(
        &self,
        rt: &Runtime,
        item: &Value,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        self.route_item(item).write_item(rt, item, path, value)
    }

    fn call(
        &self,
        rt: &Runtime,
        service: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        self.route(service).call(rt, service, method, args)
    }

    fn fetch(&self, rt: &Runtime, service: &str, method: &str, args: Vec<Value>) -> Fetch {
        self.route(service).fetch(rt, service, method, args)
    }

    fn fetch_reads(&self, rt: &Runtime, service: &str, method: &str) -> Result<(), Error> {
        self.route(service).fetch_reads(rt, service, method)
    }

    fn action(
        &self,
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
        ctx: ActionCtx,
    ) -> Result<(), Error> {
        if let Some(i) = ctx.input {
            self.action_press.set(Some((i.serial, Instant::now())));
        }
        match target {
            ActionTarget::Service(s) => self.route(s).action(rt, target, name, args, ctx),
            ActionTarget::Item(v) => self.route_item(v).action(rt, target, name, args, ctx),
        }
    }

    fn event(&self, rt: &Runtime, service: &str, event: &str) -> Option<EventQueue<Vec<Value>>> {
        self.route(service).event(rt, service, event)
    }

    fn acquire(&self, rt: &Runtime, service: &str) {
        self.take(rt, service, None, self.holder(service));
    }

    fn release(&self, rt: &Runtime, service: &str) {
        self.give(rt, service, None, self.holder(service));
    }

    fn acquire_field(&self, rt: &Runtime, service: &str, field: &str) {
        self.take(rt, service, Some(field), self.holder(service));
    }

    fn release_field(&self, rt: &Runtime, service: &str, field: &str) {
        self.give(rt, service, Some(field), self.holder(service));
    }

    fn next_wake(&self, rt: &Runtime) -> Option<SystemTime> {
        self.members
            .iter()
            .filter_map(|m| m.next_wake(rt))
            .chain(self.fallback.next_wake(rt))
            .min()
    }

    fn wake(&self, rt: &Runtime, now: SystemTime) {
        for m in &self.members {
            m.wake(rt, now);
        }
        self.fallback.wake(rt, now);
    }
}

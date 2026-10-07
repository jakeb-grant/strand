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

use std::collections::HashMap;
use std::rc::Rc;
use std::time::SystemTime;

use strand_compiler::lower::CustomService;
use strand_compiler::ty::TypeTable;
use strand_compiler::vm::host::{ActionTarget, Fetch, PathSeg, ServiceHost};
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
        }
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

    fn route(&self, service: &str) -> &dyn ServiceHost {
        match self.by_name.get(service) {
            Some(&i) => &*self.members[i],
            None => match &self.custom {
                Some(c) if c.serves(service) => &**c,
                _ => &*self.fallback,
            },
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

    fn stop(&self, rt: &Runtime, name: &str) {
        self.declarer().stop(rt, name);
    }

    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        self.route(service).read(rt, service, field)
    }

    fn sources(&self, rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        self.route(service).sources(rt, service, field)
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

    fn action(
        &self,
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
    ) -> Result<(), Error> {
        match target {
            ActionTarget::Service(s) => self.route(s).action(rt, target, name, args),
            ActionTarget::Item(v) => self.route_item(v).action(rt, target, name, args),
        }
    }

    fn event(&self, rt: &Runtime, service: &str, event: &str) -> Option<EventQueue<Vec<Value>>> {
        self.route(service).event(rt, service, event)
    }

    fn acquire(&self, rt: &Runtime, service: &str) {
        self.route(service).acquire(rt, service);
    }

    fn release(&self, rt: &Runtime, service: &str) {
        self.route(service).release(rt, service);
    }

    fn acquire_field(&self, rt: &Runtime, service: &str, field: &str) {
        self.route(service).acquire_field(rt, service, field);
    }

    fn release_field(&self, rt: &Runtime, service: &str, field: &str) {
        self.route(service).release_field(rt, service, field);
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

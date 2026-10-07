//! [`StoreHost`]: one `strand-services` store as a [`ServiceHost`]
//! (architecture.md, "Several service crates, one host").
//!
//! Each field of the store is a `Memo<Value>` over the store's typed
//! cell ([`DynService::read`], converted by name), so a binding reading
//! `cpu.usage` depends on exactly that field and an equal value stops
//! there. A keyed list (`workspaces.all`) is also mirrored as a core
//! keyed collection of values, fed by the store's diffs, so a `for` over
//! it follows them; events are lossless queues fed the same way. Writes,
//! actions and calls are converted back to [`Data`] and handed to the
//! store, which tags writes (`write_tagged`, and `write_item_tagged` for
//! an item of a keyed list: `s.volume` for `s` in `audio.sinks`) so their
//! echoes are ignored.

use std::rc::Rc;

use strand_compiler::ty::{Ty, TypeTable};
use strand_compiler::vm::host::{ActionTarget, Fetch, PathSeg, ServiceHost};
use strand_compiler::vm::value::{keyed_vec, list_of};
use strand_compiler::vm::{Value, ValueKey};
use strand_core::{Error, EventQueue, KeyedSignal, Memo, NodeId, Runtime, Scope, VecDiff};
use strand_services::{Applied, Data, DynService};

use super::convert::{steps, to_data, to_value};

/// One field's cells on the language side.
#[derive(Clone, Copy, Debug)]
enum Field {
    Plain(Memo<Value>),
    /// The collection, and its items as one list value for plain reads.
    Keyed(KeyedSignal<ValueKey, Value>, Memo<Value>),
}

/// See the module docs.
pub struct StoreHost {
    svc: Rc<dyn DynService>,
    types: Rc<TypeTable>,
    fields: Vec<Field>,
    events: Vec<EventQueue<Vec<Value>>>,
    /// Owns the cells above.
    scope: Scope,
}

impl std::fmt::Debug for StoreHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreHost")
            .field("service", &self.svc.name())
            .field("readers", &self.svc.readers())
            .finish_non_exhaustive()
    }
}

/// The key path a keyed list field of service `service` is keyed by
/// (its item record's `key`), from the schema the program was checked
/// against; `None` keys by identity.
fn key_path(types: &TypeTable, service: &str, field: &str) -> Option<Vec<String>> {
    let rec = types.record(types.find_record(service)?);
    let f = rec.fields.iter().find(|f| f.name == field)?;
    match &f.ty {
        Ty::List(item, _) => match &**item {
            Ty::Record(r) => types.records.get(r.0 as usize)?.key.clone(),
            _ => None,
        },
        _ => None,
    }
}

fn diff_value(types: &TypeTable, d: &VecDiff<Data, Data>) -> VecDiff<ValueKey, Value> {
    let key = |k: &Data| ValueKey(to_value(types, k));
    match d {
        VecDiff::Reset { items } => VecDiff::Reset {
            items: items
                .iter()
                .map(|(k, v)| (key(k), to_value(types, v)))
                .collect(),
        },
        VecDiff::Insert {
            index,
            key: k,
            value,
        } => VecDiff::Insert {
            index: *index,
            key: key(k),
            value: to_value(types, value),
        },
        VecDiff::Update {
            index,
            key: k,
            value,
        } => VecDiff::Update {
            index: *index,
            key: key(k),
            value: to_value(types, value),
        },
        VecDiff::Remove { index, key: k } => VecDiff::Remove {
            index: *index,
            key: key(k),
        },
        VecDiff::Move { from, to, key: k } => VecDiff::Move {
            from: *from,
            to: *to,
            key: key(k),
        },
    }
}

impl StoreHost {
    /// The host of `svc`, its values read as values of `types` (the
    /// schema the program was checked against). Call it where no owner is
    /// current: its cells live in a scope of their own.
    pub fn new(rt: &Runtime, svc: Rc<dyn DynService>, types: Rc<TypeTable>) -> StoreHost {
        let name = svc.name();
        let (scope, (fields, events)) = rt.scope(|rt| {
            rt.untrack(|rt| {
                let mut fields = Vec::new();
                for (i, info) in svc.fields().iter().enumerate() {
                    let field = if info.keyed {
                        let path = key_path(&types, name, info.name);
                        let items: Vec<Value> = svc
                            .keyed_items(rt, i)
                            .unwrap_or_default()
                            .iter()
                            .map(|d| to_value(&types, d))
                            .collect();
                        let k = rt.keyed(keyed_vec(types.clone(), |t| &**t, path));
                        if let Err(e) = k.replace_all(rt, items) {
                            log::warn!("{name}.{}: {e}", info.name);
                        }
                        rt.set_name(k.id(), format!("{name}.{}", info.name));
                        let list = rt.memo(move |rt| k.with(rt, list_of));
                        Field::Keyed(k, list)
                    } else {
                        let (svc, types) = (svc.clone(), types.clone());
                        let m = rt.memo(move |rt| svc.read(rt, i).map(|d| to_value(&types, &d)));
                        rt.set_name(m.id(), format!("{name}.{} value", info.name));
                        Field::Plain(m)
                    };
                    fields.push(field);
                }
                let events: Vec<EventQueue<Vec<Value>>> =
                    svc.events().iter().map(|_| rt.events()).collect();
                (fields, events)
            })
        });
        rt.set_name(scope.id(), format!("{name} host"));
        // Keyed diffs and events the store applies, mirrored.
        let (keyed, queues, t) = (fields.clone(), events.clone(), types.clone());
        let svc_items = Rc::downgrade(&svc);
        svc.observe(Box::new(move |rt, applied| match applied {
            Applied::Keyed {
                field,
                diffs,
                initial,
            } => {
                if let Some(Field::Keyed(k, _)) = keyed.get(*field) {
                    let done = if *initial {
                        // A boot value: the store rebaselined its cell, and
                        // so does the mirror (`on change` never fires at
                        // boot, nor when a service starts late).
                        match svc_items.upgrade() {
                            Some(svc) => svc.keyed_items(rt, *field).and_then(|items| {
                                let items: Vec<Value> =
                                    items.iter().map(|d| to_value(&t, d)).collect();
                                k.replace_all_reloaded(rt, items).map(drop)
                            }),
                            None => Ok(()),
                        }
                    } else {
                        let diffs: Vec<_> = diffs.iter().map(|d| diff_value(&t, d)).collect();
                        k.apply(rt, &diffs).map(drop)
                    };
                    if let Err(e) = done {
                        log::warn!("{name}: keyed field #{field}: {e}");
                    }
                }
            }
            Applied::Event { event, args } => {
                if let Some(q) = queues.get(*event) {
                    let args = args.iter().map(|d| to_value(&t, d)).collect();
                    if let Err(e) = q.emit(rt, args) {
                        log::warn!("{name}: event #{event}: {e}");
                    }
                }
            }
        }));
        StoreHost {
            svc,
            types,
            fields,
            events,
            scope,
        }
    }

    /// The service's name.
    pub fn name(&self) -> &'static str {
        self.svc.name()
    }

    fn index(&self, field: &str) -> Result<usize, Error> {
        self.svc
            .fields()
            .iter()
            .position(|f| f.name == field)
            .ok_or_else(|| Error::failed(format!("`{}` has no field `{field}`", self.name())))
    }

    fn ids(&self, i: usize, out: &mut Vec<NodeId>) {
        match self.fields.get(i) {
            Some(Field::Plain(m)) => out.push(m.id()),
            Some(Field::Keyed(k, l)) => {
                out.push(k.id());
                out.push(l.id());
            }
            None => {}
        }
        out.extend(self.svc.ids(i));
    }

    fn data_args(&self, args: &[Value]) -> Vec<Data> {
        args.iter().map(|v| to_data(&self.types, v)).collect()
    }

    /// Dispose its cells (the registry going away).
    pub fn dispose(&self, rt: &Runtime) {
        rt.dispose(self.scope.id());
    }
}

impl ServiceHost for StoreHost {
    fn read(&self, rt: &Runtime, _service: &str, field: &str) -> Result<Value, Error> {
        match self.fields.get(self.index(field)?) {
            Some(Field::Plain(m)) => m.get(rt),
            Some(Field::Keyed(_, l)) => l.get(rt),
            None => Err(Error::failed(format!("`{}.{field}`", self.name()))),
        }
    }

    fn sources(&self, _rt: &Runtime, _service: &str, field: Option<&str>) -> Vec<NodeId> {
        let mut out = Vec::new();
        match field {
            Some(f) => {
                if let Ok(i) = self.index(f) {
                    self.ids(i, &mut out);
                }
            }
            None => {
                for i in 0..self.fields.len() {
                    self.ids(i, &mut out);
                }
            }
        }
        out
    }

    fn read_keyed(
        &self,
        _rt: &Runtime,
        _service: &str,
        field: &str,
    ) -> Option<KeyedSignal<ValueKey, Value>> {
        match self.fields.get(self.index(field).ok()?)? {
            Field::Keyed(k, _) => Some(*k),
            Field::Plain(_) => None,
        }
    }

    fn write(
        &self,
        rt: &Runtime,
        _service: &str,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        let Some((PathSeg::Field(field), rest)) = path.split_first() else {
            return Err(Error::failed(format!(
                "`{}` cannot be written",
                self.name()
            )));
        };
        let i = self.index(field)?;
        // A leaf below the field (`audio.sink.volume`) is `rw` in its
        // record, which the checker saw; the field itself must be.
        if rest.is_empty() && !self.svc.fields()[i].rw {
            return Err(Error::failed(format!(
                "`{}.{field}` is read-only",
                self.name()
            )));
        }
        self.svc
            .write(rt, i, &steps(rest), to_data(&self.types, &value))
    }

    fn write_item(
        &self,
        rt: &Runtime,
        item: &Value,
        path: &[PathSeg],
        value: Value,
    ) -> Result<(), Error> {
        let record = match item {
            Value::Record(r) => self
                .types
                .records
                .get(r.ty.0 as usize)
                .map(|d| d.name.clone()),
            _ => None,
        };
        let Some(record) = record else {
            return Err(Error::failed(format!(
                "`{}`: no item to write",
                self.name()
            )));
        };
        self.svc.write_item(
            rt,
            &record,
            &to_data(&self.types, item),
            &steps(path),
            to_data(&self.types, &value),
        )
    }

    fn call(
        &self,
        rt: &Runtime,
        _service: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        match self.svc.call(rt, method, &self.data_args(args)) {
            Some(r) => r.map(|d| to_value(&self.types, &d)),
            None if self.svc.methods().contains(&method) => Err(Error::failed(format!(
                "`{}.{method}` is asynchronous: it is fetched (`ServiceHost::fetch`), not called",
                self.name()
            ))),
            None => Err(Error::failed(format!(
                "`{}` has no method `{method}`",
                self.name()
            ))),
        }
    }

    fn fetch(&self, rt: &Runtime, service: &str, method: &str, args: Vec<Value>) -> Fetch {
        if !self.svc.methods().contains(&method) {
            // A `fn` method: computed at once.
            let r = self.call(rt, service, method, &args);
            return Box::pin(async move { r });
        }
        let fut = self.svc.fetch(rt, method, &self.data_args(&args));
        let types = self.types.clone();
        Box::pin(async move {
            fut.await
                .map(|d| to_value(&types, &d))
                .map_err(Error::failed)
        })
    }

    fn action(
        &self,
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
    ) -> Result<(), Error> {
        let args = self.data_args(args);
        match target {
            ActionTarget::Service(_) => self.svc.action(rt, name, None, &args),
            ActionTarget::Item(v) => {
                self.svc
                    .action(rt, name, Some(&to_data(&self.types, v)), &args)
            }
        }
    }

    fn event(&self, _rt: &Runtime, _service: &str, event: &str) -> Option<EventQueue<Vec<Value>>> {
        let i = self.svc.events().iter().position(|e| e.name == event)?;
        self.events.get(i).copied()
    }

    fn acquire(&self, rt: &Runtime, _service: &str) {
        self.svc.acquire(rt);
    }

    fn release(&self, rt: &Runtime, _service: &str) {
        self.svc.release(rt);
    }

    fn acquire_field(&self, _rt: &Runtime, _service: &str, field: &str) {
        if let Ok(i) = self.index(field) {
            self.svc.acquire_field(i);
        }
    }

    fn release_field(&self, _rt: &Runtime, _service: &str, field: &str) {
        if let Ok(i) = self.index(field) {
            self.svc.release_field(i);
        }
    }
}

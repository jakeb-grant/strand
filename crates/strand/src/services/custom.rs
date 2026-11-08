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
//! text, enums by variant name, records by field name, and (doc-formats)
//! the type deciding a list or one value, an XML element's text or its
//! fields, and a `Date` from date text or Unix time. A value that does
//! not convert is the type's default (null for an optional field), and is
//! reported once (log and `strand watch`) naming the field, its key and
//! the value, until a value converts again ([`Mismatch`]); so is a value
//! inside it, a list's item or a record's field, with its path. An
//! `rw` field written (`ppd.profile = "performance"`, `<-> ppd.profile`)
//! is an item write of its value, so the service sets the property and
//! its echo is ignored. (M6) An action (`ppd.hold("performance")`, of
//! `action hold(profile: text, …) = HoldProfile`) is sent to the service
//! by its index with its arguments as data, and the service calls the
//! method. A reload that changes a declaration restarts only
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
use strand_compiler::vm::host::{ActionCtx, ActionTarget, PathSeg, ServiceHost};
use strand_compiler::vm::schema_host::default_value;
use strand_compiler::vm::{Num, Value, clock};
use strand_core::{Error, Memo, NodeId, Runtime, Scope, Signal};
use strand_services::custom::{
    self, ActionSpec, Custom, CustomAction, CustomValue, FieldSpec, Source, Spec, TextField,
};
use strand_services::{Client, Data, ServiceDiagnostic, Services, Step, ToData};

use super::convert::{to_service_data, to_value};

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
    /// (dbus-action-run) Its actions' runs (`bolt.enroll.error`), each an
    /// `ActionRun`, by index.
    runs: Vec<Memo<Value>>,
    /// (services-round) `on svc.line(doc)`'s queue (a `from listen`
    /// declaration's), each line as the declaration's record.
    line: strand_core::EventQueue<Vec<Value>>,
    /// Where the client's one observer sends lines: this mount's queue,
    /// emptied when it is disposed.
    sink: Rc<LineSink>,
    /// Its fields' mismatch reports: resolved when the service restarts
    /// or stops (a mismatch is usually fixed by changing the declared
    /// type, which restarts it; a field that still mismatches is reported
    /// afresh by the new mount).
    mismatches: Vec<Rc<Mismatch>>,
    /// Owns the memos.
    scope: Scope,
}

impl Entry {
    /// Resolve every open mismatch report and dispose the memos; the
    /// client's observer sends nothing until the next mount.
    fn dispose(self, rt: &Runtime) -> (Client<Custom>, Rc<LineSink>) {
        for m in &self.mismatches {
            m.resolve();
        }
        custom::forget(self.spec);
        *self.sink.0.borrow_mut() = None;
        self.scope.dispose(rt);
        (self.client, self.sink)
    }
}

/// (services-round) The current mount's `line` queue and how to build a
/// line's record, which the one observer a client gets
/// ([`LineSink::observe`]) looks up for each line: a restart mounts anew
/// on the same client, so an observer per mount would pile up, each
/// emitting into a disposed queue.
#[derive(Default)]
struct LineSink(RefCell<Option<LineTarget>>);

struct LineTarget {
    queue: strand_core::EventQueue<Vec<Value>>,
    names: Vec<String>,
    typing: Rc<RefCell<Typing>>,
    service: String,
    /// (doc-formats) The last line's value that did not convert, logged.
    nested: RefCell<Option<String>>,
}

impl LineSink {
    /// The sink of a newly registered `client`, observing it once.
    fn observe(client: &Client<Custom>) -> Rc<LineSink> {
        let sink = Rc::new(LineSink::default());
        let s = sink.clone();
        client.dynamic().observe(Box::new(move |rt, applied| {
            let strand_services::Applied::Event { args, .. } = applied else {
                return;
            };
            let Some(Data::List(values)) = args.first() else {
                return;
            };
            let target = s.0.borrow();
            let Some(t) = target.as_ref() else { return };
            let typing = t.typing.borrow();
            let doc = Data::Record {
                ty: "".into(),
                fields: t
                    .names
                    .iter()
                    .zip(values.iter())
                    .map(|(n, v)| (n.clone().into(), v.clone()))
                    .collect(),
            };
            let ty = Ty::Record(typing.record);
            let mut nested = None;
            let v = coerce_noting(&typing.table, &ty, &doc, &mut nested)
                .unwrap_or_else(|| default_value(&typing.table, &ty));
            // (doc-formats) A value inside a line that did not convert,
            // logged once until another does not.
            let note = nested.map(|n| {
                let at = n.path.trim_start_matches('.');
                let then = if at.ends_with(']') {
                    "the item is left out"
                } else {
                    "it reads as the type's default"
                };
                format!("`{at}` holds {}, which is not a `{}`; {then}", n.held, n.ty)
            });
            if let Some(note) = &note
                && t.nested.borrow().as_ref() != Some(note)
            {
                log::warn!("`{}.line`: {note}", t.service);
            }
            *t.nested.borrow_mut() = note;
            let (queue, service) = (t.queue, t.service.clone());
            drop(typing);
            drop(target);
            if let Err(e) = queue.emit(rt, vec![v]) {
                log::warn!("{service}.line: {e}");
            }
        }));
        sink
    }
}

/// A service's type table and its fields' types (in declaration order).
struct Typing {
    table: Rc<TypeTable>,
    fields: Vec<Ty>,
    /// The declaration's own record (a `line` event's `doc`).
    record: strand_compiler::ty::RecordId,
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
            record: decl.record,
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

/// (doc-formats) Whether a field of type `ty` is text, as the service
/// side reads XML for it (`Document::fields`).
fn text_field(ty: Option<&Ty>) -> TextField {
    match ty {
        Some(Ty::Prim(Prim::Text)) => TextField::Required,
        Some(Ty::Optional(t)) if matches!(**t, Ty::Prim(Prim::Text)) => TextField::Optional,
        _ => TextField::No,
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

/// The service-side spec of `decl`, its fields' declared types `types`
/// (in its fields' order; a missing one is not text).
pub fn spec_of(decl: &CustomService, config_dir: Option<&Path>, types: &[Ty]) -> Spec {
    let source = match &decl.source {
        SourceSpec::Dbus { system, name, path } => Source::Dbus {
            system: *system,
            name: name.clone(),
            path: path.clone(),
        },
        SourceSpec::File { path } => Source::File {
            path: resolve(path, config_dir),
        },
        SourceSpec::Dir { path } => Source::Dir {
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
            .enumerate()
            .map(|(i, f)| FieldSpec {
                name: f.name.clone(),
                key: f.key.clone(),
                rw: f.rw,
                text: text_field(types.get(i)),
            })
            .collect(),
        actions: decl
            .actions
            .iter()
            .map(|a| ActionSpec {
                name: a.name.clone(),
                method: a.method.clone(),
            })
            .collect(),
    }
}

/// (doc-formats) A value inside a field's value that did not convert:
/// where (`[0].pubDate`, after the field's key), what it held, and the
/// type it is not. The field itself converted, the value taking its
/// type's default there; [`Mismatch`] reports it once.
#[derive(Clone, Debug, PartialEq)]
pub struct Nested {
    pub path: String,
    pub held: String,
    pub ty: String,
}

/// `d` as a value of type `ty`: see the module docs. `None`: it does not
/// convert.
pub fn coerce(types: &TypeTable, ty: &Ty, d: &Data) -> Option<Value> {
    coerce_noting(types, ty, d, &mut None)
}

/// Whether text holds several values, not one value and its unit:
/// several lines (`22\n80\n443`), or a word after the first that is
/// itself a number (`1234 5678`, `22\t80`), where `45 °C` and
/// `16303392 kB` are one number and its unit.
fn several_values(t: &str) -> bool {
    let t = t.trim();
    t.contains('\n')
        || t.split_whitespace()
            .skip(1)
            .any(|w| w.parse::<f64>().is_ok())
}

/// [`coerce`], noting in `nested` the first value inside `d` that held
/// something and did not convert (a list's item, a record's field).
///
/// (doc-formats) The declared type decides, for every document:
/// - a list type over one value is a list of it, if it converts;
/// - a single type over a list reads its first item;
/// - an XML element read as text, a number, a bool, an enum, a path or
///   a `Date` is its text ([`custom::element_text`]); one with no text
///   does not convert (JSON objects never read as text);
/// - length, angle and duration read plain number text (px, deg,
///   seconds), as from a JSON number;
/// - a record's fields are found by [`custom::lookup`] (exact names,
///   then attributes and local names, then an element's `text`); a list
///   field takes every match, a single one the first;
/// - blank text read as an optional type other than text is null.
pub fn coerce_noting(
    types: &TypeTable,
    ty: &Ty,
    d: &Data,
    nested: &mut Option<Nested>,
) -> Option<Value> {
    go(types, ty, d, &mut String::new(), nested)
}

/// Whether a value of `ty` is read from text (an element's text).
fn reads_text(types: &TypeTable, ty: &Ty) -> bool {
    match ty {
        Ty::Prim(_) | Ty::Enum(_) => true,
        Ty::Record(r) => clock::is_date(types, *r),
        _ => false,
    }
}

/// `ty` without an `?`.
fn strip_optional(ty: &Ty) -> &Ty {
    match ty {
        Ty::Optional(inner) => inner,
        ty => ty,
    }
}

/// `ty` without any `?` or list around it.
fn strip_list(ty: &Ty) -> &Ty {
    match ty {
        Ty::Optional(inner) | Ty::List(inner, _) => strip_list(inner),
        ty => ty,
    }
}

/// [`go`] for a value inside another at `path`, noted when it held
/// something and did not convert.
fn noted(
    types: &TypeTable,
    ty: &Ty,
    d: &Data,
    path: &mut String,
    nested: &mut Option<Nested>,
) -> Option<Value> {
    let v = go(types, ty, d, path, nested);
    if v.is_none() && !matches!(d, Data::Null) && nested.is_none() {
        *nested = Some(Nested {
            path: path.clone(),
            held: shown(d),
            ty: types.show(ty).to_string(),
        });
    }
    v
}

/// The items of `items` as a list of `item`s: one that does not convert
/// is left out (and noted).
fn list_of<'d>(
    types: &TypeTable,
    item: &Ty,
    items: impl Iterator<Item = &'d Data>,
    path: &mut String,
    nested: &mut Option<Nested>,
) -> Value {
    let at = path.len();
    let values = items
        .enumerate()
        .filter_map(|(i, d)| {
            path.push_str(&format!("[{i}]"));
            let v = noted(types, item, d, path, nested);
            path.truncate(at);
            v
        })
        .collect();
    Value::list(values)
}

fn go(
    types: &TypeTable,
    ty: &Ty,
    d: &Data,
    path: &mut String,
    nested: &mut Option<Nested>,
) -> Option<Value> {
    if let Ty::Optional(inner) = ty {
        return match d {
            Data::Null => Some(Value::Null),
            // (doc-formats) Blank text is no value of a type other than
            // text (`<cap:polygon/>` read as a `Date?`).
            Data::Text(t) if t.trim().is_empty() && !matches!(**inner, Ty::Prim(Prim::Text)) => {
                Some(Value::Null)
            }
            d => go(types, inner, d, path, nested),
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
    // A plain number written as text (`"30"`, `" 45.5 "`).
    let plain = |d: &Data| match d {
        Data::Text(t) => t.trim().parse::<f64>().ok(),
        _ => None,
    };
    match (ty, d) {
        (_, Data::Null) => None,
        (Ty::Any, d) => Some(to_value(types, d)),
        // (doc-formats) A single value from a list: its first item.
        (ty, Data::List(items)) if !matches!(ty, Ty::List(..)) => {
            go(types, ty, items.first()?, path, nested)
        }
        // (doc-formats) An element read as text: its text content.
        (ty, d) if custom::is_element(d) && reads_text(types, ty) => {
            let t = custom::element_text(d);
            if t.is_empty() {
                return None;
            }
            go(types, ty, &Data::text(t), path, nested)
        }
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
        // (doc-formats) XML has no numbers: plain number text reads as
        // a JSON number does (GPX's `lat="45.77"` as an angle).
        (Ty::Prim(Prim::Angle), d) if plain(d).is_some() => plain(d)
            .filter(|n| n.is_finite())
            .map(|n| Value::Num(n, Num::Deg)),
        // Seconds; a count no duration holds (`1e300`, negative) is none.
        (Ty::Prim(Prim::Duration), Data::Int(_) | Data::Float(_)) => number(d)
            .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
            .map(Value::from),
        (Ty::Prim(Prim::Duration), d) if plain(d).is_some() => plain(d)
            .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
            .map(Value::from),
        (Ty::Prim(Prim::Duration), Data::Duration(t)) => Some(Value::from(*t)),
        (Ty::Prim(Prim::Duration), Data::NegDuration(_)) => Some(to_value(types, d)),
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
        (Ty::List(item, _), Data::List(items)) => {
            Some(list_of(types, item, items.iter(), path, nested))
        }
        // (doc-formats) Blank text (an empty element, `<trkseg>
        // </trkseg>`) is an empty list of anything but text.
        (Ty::List(item, _), Data::Text(t))
            if t.trim().is_empty() && !matches!(**item, Ty::Prim(Prim::Text)) =>
        {
            Some(Value::list(Vec::new()))
        }
        // (doc-formats) A list from one value: a list of it, if it
        // converts; a record (a JSON object, an element) only when it
        // holds one of the item record's fields, so an API's error
        // object read as `[Release]` is reported, not one empty item.
        // Text of several values is not one item of a list of numbers,
        // bools, enums or the like: a number with a unit (`45 °C`) reads
        // its first word, so a command's `22\n80\n443` or `pidof`'s
        // `1234 5678` read as `[int]` would be `[22]` or `[1234]`, the
        // rest lost without a report. Several lines, or a further word
        // that is itself a number, is several values; a unit is not.
        // An element's text (`<ports>22 80</ports>`) the same.
        (Ty::List(item, _), d)
            if matches!(strip_optional(item), Ty::Prim(p) if !matches!(p, Prim::Text | Prim::Path))
                && match d {
                    Data::Text(t) => several_values(t),
                    d if custom::is_element(d) => several_values(&custom::element_text(d)),
                    _ => false,
                } =>
        {
            None
        }
        (Ty::List(item, _), d) => {
            if let Ty::Record(r) = strip_optional(item)
                && matches!(d, Data::Record { .. })
                && !clock::is_date(types, *r)
                && !types
                    .record(*r)
                    .fields
                    .iter()
                    .any(|f| !custom::lookup(d, &f.name).is_empty())
            {
                return None;
            }
            go(types, item, d, path, nested).map(|v| Value::list(vec![v]))
        }
        // (doc-formats) A number as its digits read: Unix seconds or
        // milliseconds, `YYYYMMDD`, or (fewer digits) no date.
        (Ty::Record(r), Data::Int(_) | Data::Float(_)) if clock::is_date(types, *r) => {
            let parts = clock::date_from_number(number(d)?, clock::Zone::Local)?;
            Some(clock::date_value(*r, parts))
        }
        (Ty::Record(r), Data::Record { .. }) => {
            let def = types.record(*r);
            let at = path.len();
            let values = def
                .fields
                .iter()
                .map(|f| {
                    let found = custom::lookup(d, &f.name);
                    path.push('.');
                    path.push_str(&f.name);
                    let item = match &f.ty {
                        Ty::List(item, _) => Some(&**item),
                        Ty::Optional(inner) => match &**inner {
                            Ty::List(item, _) => Some(&**item),
                            _ => None,
                        },
                        _ => None,
                    };
                    let v = match (found.as_slice(), item) {
                        ([], _) => None,
                        ([one], _) => noted(types, &f.ty, one, path, nested),
                        (many, Some(item)) => Some(list_of(
                            types,
                            item,
                            many.iter().map(|c| &**c),
                            path,
                            nested,
                        )),
                        ([first, ..], None) => noted(types, &f.ty, first, path, nested),
                    };
                    path.truncate(at);
                    v.unwrap_or_else(|| default_value(types, &f.ty))
                })
                .collect();
            Some(Value::record(*r, values))
        }
        // (doc-formats) Text read as a record with a `text` field: that
        // field, the others absent, as an element of text alone would
        // be with attributes (`<guid>x</guid>` beside `<guid
        // isPermaLink="false">x</guid>`).
        // The `text` field must read text, not the record again
        // (`type Node { text: Node? }` would wrap the same text forever).
        (Ty::Record(r), Data::Text(_))
            if types
                .record(*r)
                .fields
                .iter()
                .any(|f| f.name == "text" && !matches!(strip_list(&f.ty), Ty::Record(_))) =>
        {
            let element = Data::Record {
                ty: custom::ELEMENT.into(),
                fields: vec![("#text".into(), d.clone())],
            };
            go(types, ty, &element, path, nested)
        }
        // (doc-formats) Blank text (an empty element, `<ports/>`) is a
        // record of its fields' defaults, as an element with none of
        // its fields is; not a `Date`, which has no default.
        (Ty::Record(r), Data::Text(t)) if t.trim().is_empty() && !clock::is_date(types, *r) => {
            let def = types.record(*r);
            let values = def
                .fields
                .iter()
                .map(|f| default_value(types, &f.ty))
                .collect();
            Some(Value::record(*r, values))
        }
        (ty, Data::Text(t)) => strand_compiler::instantiate::parse_text(types, ty, t.trim()),
        // Anything else does not convert: the type's default.
        _ => None,
    }
}

/// (flex) What the command of an action with a result printed (`out`),
/// read as its declared type `ty`, as a `from poll` service reads a
/// command's output into a field: a `text` (or `path`) is the output
/// itself, trimmed; any other type reads the output as a document (JSON,
/// XML, `key=value` lines, else text) and converts it ([`coerce`]: numbers
/// from text, enums by variant name, records by field name, the declared
/// type deciding a list or one value). Nothing
/// printed is `null` for an optional type. `Err`: it does not convert.
pub fn read_output(types: &TypeTable, ty: &Ty, out: &str) -> Result<Value, String> {
    let t = out.trim();
    let inner = match ty {
        Ty::Optional(inner) => {
            if t.is_empty() {
                return Ok(Value::Null);
            }
            &**inner
        }
        ty => ty,
    };
    if matches!(inner, Ty::Prim(Prim::Text | Prim::Path)) {
        return Ok(Value::text(t));
    }
    let data = custom::Document::parse(t).into_data();
    coerce(types, inner, &data)
        .ok_or_else(|| format!("what it printed is not a `{}`", types.show(inner)))
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
    /// The message reported and not yet resolved, and whether it is of
    /// a value inside the field's (doc-formats).
    reported: RefCell<Option<(bool, String)>>,
}

impl Mismatch {
    /// The field read `d`, which converted or not, a value inside it
    /// perhaps not (`nested`). No value yet (null) says nothing. A field
    /// that converts wholly resolves the open report. Its own value not
    /// converting, or one inside it, is reported once until then, and
    /// replaces an open report of the other kind (the whole field
    /// failing after a value inside it did, or the reverse).
    fn saw(&self, types: &TypeTable, ty: &Ty, d: &Data, converted: bool, nested: Option<&Nested>) {
        let message = match (converted, nested) {
            (true, None) => return self.resolve(),
            (true, Some(n)) => (
                true,
                format!(
                    "`{}`: `{}{}` holds {}, which is not a `{}`; {}",
                    self.field,
                    self.key,
                    n.path,
                    n.held,
                    n.ty,
                    // A list's item is left out; a record's field defaults.
                    if n.path.ends_with(']') {
                        "the item is left out"
                    } else {
                        "it reads as the type's default"
                    },
                ),
            ),
            (false, _) if matches!(d, Data::Null) => return,
            (false, _) => (
                false,
                format!(
                    "`{}`: `{}` holds {}, which is not a `{}`; it reads as the type's default",
                    self.field,
                    self.key,
                    shown(d),
                    types.show(ty),
                ),
            ),
        };
        // One report of each kind until the field converts wholly: a
        // second bad value of the same kind is not news.
        if matches!(&*self.reported.borrow(), Some((nested, _)) if *nested == message.0) {
            return;
        }
        // The other kind (the whole field failing after a value inside
        // it did, or the reverse): the open report is over.
        self.resolve();
        self.services.report(ServiceDiagnostic {
            service: self.service.clone(),
            message: message.1.clone(),
            notice: false,
            resolved: false,
            standing: true,
        });
        *self.reported.borrow_mut() = Some(message);
    }
}

impl Mismatch {
    /// The report, if open, is resolved (the value converts again, or
    /// the field's service restarted or stopped).
    fn resolve(&self) {
        let taken = self.reported.borrow_mut().take();
        if let Some((_, message)) = taken {
            self.services.report(ServiceDiagnostic {
                service: self.service.clone(),
                message,
                notice: false,
                resolved: true,
                standing: true,
            });
        }
    }
}

/// (dbus-action-run) What the service says of an action's calls as an
/// `ActionRun` of `types`: `running` while one is in flight, `started` the
/// oldest one's time to the minute, `error` why the last one failed. Null
/// fields when the schema has no `ActionRun` (never: it is builtin).
fn action_run(types: &TypeTable, run: &custom::CustomRun) -> Value {
    let Some(record) = types.find_schema_record("ActionRun") else {
        return Value::Null;
    };
    let started = run
        .started
        .zip(types.find_schema_record("Date"))
        .and_then(|(secs, date)| {
            use chrono::TimeZone;
            let t = chrono::Local.timestamp_opt(secs, 0).single()?;
            Some(clock::datetime_record(date, t.naive_local()))
        })
        .unwrap_or(Value::Null);
    let error = run.error.as_deref().map_or(Value::Null, Value::text);
    Value::record(record, vec![Value::Bool(run.running > 0), started, error])
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
        // (doc-formats) An XML element by its text, when it has some.
        d if custom::is_element(d) && !custom::element_text(d).is_empty() => {
            shown(&Data::text(custom::element_text(d)))
        }
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

    /// The spec id `name` runs (tests).
    #[cfg(test)]
    pub fn spec(&self, name: &str) -> Option<i64> {
        self.entries.borrow().get(name).map(|e| e.spec)
    }

    /// Seed `client` for `decl` and build the memos converting its
    /// values.
    fn mount(
        &self,
        rt: &Runtime,
        client: Client<Custom>,
        sink: Rc<LineSink>,
        decl: &CustomService,
        types: &TypeTable,
    ) -> Entry {
        let typing = Typing::of(decl, types);
        let spec = custom::register(spec_of(
            decl,
            self.config_dir.borrow().as_deref(),
            &typing.fields,
        ));
        // (history) A `from poll` samples on a clock: a value it takes
        // while hidden and kept is a poll.
        client.polls(match &decl.source {
            SourceSpec::Poll { every, .. } => Some(*every),
            _ => None,
        });
        let n = decl.fields.len();
        if let Err(e) = client.seed(rt, |s| *s = Custom::seeded(spec, n)) {
            log::warn!("{}: {e}", decl.name);
        }
        let typing = Rc::new(RefCell::new(typing));
        let mut mismatches = Vec::with_capacity(n);
        let (scope, (epoch, values, runs, line)) = rt.scope(|rt| {
            let epoch = rt.signal(0u64);
            let values = (0..n)
                .map(|i| {
                    let (client, typing) = (client.clone(), typing.clone());
                    let name = format!("{}.{}", decl.name, decl.fields[i].name);
                    let report = Rc::new(Mismatch {
                        services: self.services.clone(),
                        service: decl.name.clone(),
                        field: name.clone(),
                        key: decl.fields[i].key.join("."),
                        reported: RefCell::new(None),
                    });
                    mismatches.push(report.clone());
                    let m = rt.memo(move |rt| {
                        epoch.get(rt)?;
                        let d = client
                            .cells()
                            .values
                            .with(rt, |v| v.get(&(i as i64)).map(|v| v.value.clone()))?
                            .unwrap_or_default();
                        let typing = typing.borrow();
                        let (types, ty) = (&*typing.table, &typing.fields[i]);
                        let mut nested = None;
                        let v = coerce_noting(types, ty, &d, &mut nested);
                        report.saw(types, ty, &d, v.is_some(), nested.as_ref());
                        Ok(v.unwrap_or_else(|| default_value(types, ty)))
                    });
                    rt.set_name(m.id(), name);
                    m
                })
                .collect::<Vec<Memo<Value>>>();
            // (dbus-action-run) Each action's run, as the service tells
            // it, an `ActionRun` as a declared command action's run is.
            let runs = (0..decl.actions.len())
                .map(|i| {
                    let (client, typing) = (client.clone(), typing.clone());
                    let m = rt.memo(move |rt| {
                        epoch.get(rt)?;
                        let run = client
                            .cells()
                            .runs
                            .with(rt, |v| v.get(&(i as i64)).cloned())?
                            .unwrap_or_default();
                        Ok(action_run(&typing.borrow().table, &run))
                    });
                    rt.set_name(m.id(), format!("{}.{}", decl.name, decl.actions[i].name));
                    m
                })
                .collect::<Vec<Memo<Value>>>();
            (epoch, values, runs, rt.events::<Vec<Value>>())
        });
        // (services-round) Each `line` the service sends, as the
        // declaration's record: its fields converted as the memos convert
        // them, a value that does not convert its type's default.
        *sink.0.borrow_mut() = Some(LineTarget {
            queue: line,
            names: decl.fields.iter().map(|f| f.name.clone()).collect(),
            typing: typing.clone(),
            service: decl.name.clone(),
            nested: RefCell::new(None),
        });
        Entry {
            client,
            spec,
            decl: decl.clone(),
            typing,
            epoch,
            values,
            runs,
            line,
            sink,
            mismatches,
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
        let entries: Vec<Entry> = self.entries.borrow_mut().drain().map(|(_, e)| e).collect();
        for e in entries {
            e.dispose(rt).0.unregister(rt);
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
                let sink = LineSink::observe(&client);
                let entry = self.mount(rt, client, sink, decl, types);
                self.entries.borrow_mut().insert(decl.name.clone(), entry);
            }
        }
    }

    fn restart(&self, rt: &Runtime, decl: &CustomService, types: &TypeTable) {
        let old = self.entries.borrow_mut().remove(&decl.name);
        let Some(old) = old else {
            return self.declare(rt, decl, types);
        };
        let (client, sink) = old.dispose(rt);
        let entry = self.mount(rt, client.clone(), sink, decl, types);
        client.restart(rt);
        self.entries.borrow_mut().insert(decl.name.clone(), entry);
    }

    fn retype(&self, rt: &Runtime, decl: &CustomService, types: &TypeTable) {
        let same = {
            let entries = self.entries.borrow();
            let Some(e) = entries.get(&decl.name) else {
                drop(entries);
                return self.declare(rt, decl, types);
            };
            // (doc-formats) A field becoming text, or no longer, reads
            // XML differently on the service side: a restart.
            let text = |fields: &[Ty]| -> Vec<TextField> {
                fields.iter().map(|t| text_field(Some(t))).collect()
            };
            e.decl.source == decl.source
                && e.decl.fields == decl.fields
                && e.decl.actions == decl.actions
                && text(&e.typing.borrow().fields) == text(&Typing::of(decl, types).fields)
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
            old.dispose(rt).0.unregister(rt);
        }
    }

    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        // (dbus-action-run) `bolt.enroll`, read without a call: its run.
        let run = self.entries.borrow().get(service).and_then(|e| {
            let i = e.decl.actions.iter().position(|a| a.name == field)?;
            e.runs.get(i).copied()
        });
        if let Some(m) = run {
            return m.get(rt);
        }
        let i = self.index(service, field)?;
        let m = self.entries.borrow().get(service).map(|e| e.values[i]);
        match m {
            Some(m) => m.get(rt),
            None => Err(Error::failed(format!("no service `{service}`"))),
        }
    }

    fn ready(&self, rt: &Runtime, service: &str) -> bool {
        let client = self.entries.borrow().get(service).map(|e| e.client.clone());
        client.is_some_and(|c| c.ready(rt))
    }

    fn sources(&self, _rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        let entries = self.entries.borrow();
        let Some(e) = entries.get(service) else {
            return Vec::new();
        };
        let ready = e.client.ready_cell().id();
        if field == Some(strand_compiler::schema::READY) {
            return vec![ready];
        }
        if let Some(i) = field.and_then(|f| e.decl.actions.iter().position(|a| a.name == f))
            && let Some(m) = e.runs.get(i)
        {
            return vec![m.id()];
        }
        match field.and_then(|f| e.decl.fields.iter().position(|d| d.name == f)) {
            Some(i) => vec![e.values[i].id()],
            None => e
                .values
                .iter()
                .chain(&e.runs)
                .map(|m| m.id())
                .chain([ready])
                .collect(),
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
                .with_path(&steps, to_service_data(&table, &value)?)
                .map_err(|e| Error::failed(e.to_string()))?
        } else {
            to_service_data(&table, &value)?
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
        rt: &Runtime,
        target: ActionTarget<'_>,
        name: &str,
        args: &[Value],
        ctx: ActionCtx,
    ) -> Result<(), Error> {
        let ActionTarget::Service(service) = target else {
            return Err(Error::failed(format!(
                "a no-code service has no item action `{name}`"
            )));
        };
        let (client, index, table) = {
            let entries = self.entries.borrow();
            let e = entries
                .get(service)
                .ok_or_else(|| Error::failed(format!("no service `{service}`")))?;
            let index = e
                .decl
                .actions
                .iter()
                .position(|a| a.name == name)
                .ok_or_else(|| Error::failed(format!("`{service}` has no action `{name}`")))?;
            (e.client.clone(), index, e.typing.borrow().table.clone())
        };
        let args = args
            .iter()
            .map(|v| to_service_data(&table, v))
            .collect::<Result<_, _>>()?;
        client.act_with(rt, CustomAction { index, args }, ctx, false)
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

    /// (services-round) A `from listen` declaration's `line`: asking for
    /// it (a handler `on svc.line(doc)`) is what has the service send
    /// each line ([`custom::hear_lines`]), and the handler is a reader:
    /// the command runs while it is mounted, its fields read or not
    /// (released with the scope that mounted it).
    fn event(
        &self,
        rt: &Runtime,
        service: &str,
        event: &str,
    ) -> Option<strand_core::EventQueue<Vec<Value>>> {
        let entries = self.entries.borrow();
        let e = entries.get(service)?;
        if event != "line" || !matches!(e.decl.source, SourceSpec::Listen { .. }) {
            return None;
        }
        custom::hear_lines(e.spec);
        e.client.acquire(rt);
        let (client, weak, spec) = (e.client.clone(), rt.downgrade(), e.spec);
        rt.on_cleanup(move || {
            custom::unhear_lines(spec);
            if let Some(rt) = weak.upgrade() {
                client.release(&rt);
            }
        });
        Some(e.line)
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

    fn keep(&self, rt: &Runtime, service: &str, every: Option<std::time::Duration>) {
        let c = self.entries.borrow().get(service).map(|e| e.client.clone());
        if let Some(c) = c {
            c.keep(rt, every);
        }
    }

    fn sampled(&self, _rt: &Runtime, service: &str) -> Option<std::time::Duration> {
        self.entries.borrow().get(service)?.client.sampled()
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
    /// (M6) The objects' methods, for services' actions.
    methods: Arc<strand_introspect::Cache<Vec<strand_introspect::Method>>>,
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
            emits: bus_emits(p.emits),
        })
        .collect()
}

/// (dbus-unfollowed) strand-introspect's [`strand_introspect::Emits`]
/// as the compiler's check reads it.
pub fn bus_emits(e: strand_introspect::Emits) -> strand_compiler::check::dbus::Emits {
    use strand_compiler::check::dbus::Emits as To;
    use strand_introspect::Emits as From;
    match e {
        From::True => To::True,
        From::Invalidates => To::Invalidates,
        From::Const => To::Const,
        From::False => To::False,
    }
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

    fn methods(
        &self,
        system: bool,
        name: &str,
        path: &str,
    ) -> Option<Result<Vec<strand_compiler::check::dbus::BusMethod>, String>> {
        let bus = if system {
            strand_introspect::Bus::System
        } else {
            strand_introspect::Bus::Session
        };
        let answer = match &self.ask {
            Some(ask) if !self.waits() => {
                let ask = ask.clone();
                self.methods
                    .answer_or_ask(&bus, name, path, move || ask())?
            }
            _ => self.methods.answer(&bus, name, path),
        };
        Some(answer.map(bus_methods))
    }
}

/// (M6) Introspected methods as the compiler's check reads them.
pub fn bus_methods(
    methods: Vec<strand_introspect::Method>,
) -> Vec<strand_compiler::check::dbus::BusMethod> {
    methods
        .into_iter()
        .map(|m| strand_compiler::check::dbus::BusMethod {
            interface: m.interface,
            name: m.name,
            inputs: m.inputs,
        })
        .collect()
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
///
/// It also checks the paths `from file` and `from poll` read under
/// `config_dir` ([`strand_compiler::check::paths`]).
pub fn dbus_check(
    config_dir: Option<PathBuf>,
    recheck: impl Fn() + Send + Sync + 'static,
) -> (strand_compiler::reconcile::loader::ExtraCheck, DbusCheck) {
    dbus_check_on(
        Arc::new(strand_introspect::Cache::default()),
        config_dir,
        recheck,
    )
}

/// [`dbus_check`] over `cache` (tests ask through a fake bus).
fn dbus_check_on(
    cache: Arc<strand_introspect::Cache>,
    config_dir: Option<PathBuf>,
    recheck: impl Fn() + Send + Sync + 'static,
) -> (strand_compiler::reconcile::loader::ExtraCheck, DbusCheck) {
    let waits = Arc::new(AtomicBool::new(true));
    let intro = BusIntrospector {
        cache,
        methods: Arc::default(),
        ask: Some(Arc::new(recheck)),
        waits: waits.clone(),
    };
    (
        strand_compiler::reconcile::loader::ExtraCheck(Box::new(move |c| {
            let mut d = strand_compiler::check::dbus::check(&c.program, &intro);
            d.extend(strand_compiler::check::paths::check(
                &c.program,
                config_dir.as_deref(),
            ));
            d
        })),
        DbusCheck(waits),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// (M6, the review of the owner's ruling) `BusIntrospector::methods`
    /// hands the check the object's methods as the bus answered them,
    /// failures included; without waiting, an answer not yet in is `None`
    /// and the recheck is asked for once it is.
    #[test]
    fn the_introspector_answers_methods_as_the_bus_does() {
        let method = strand_introspect::Method {
            interface: "net.hadess.PowerProfiles".into(),
            name: "HoldProfile".into(),
            inputs: vec!["s".into(), "s".into(), "s".into()],
        };
        let answer = method.clone();
        let methods = Arc::new(strand_introspect::Cache::with_fetch(
            strand_introspect::TTL,
            move |_, name, _| {
                if name == "org.example.Gone" {
                    Err("no answer".into())
                } else {
                    Ok(vec![answer.clone()])
                }
            },
        ));
        let waiting = BusIntrospector {
            cache: Arc::default(),
            methods: methods.clone(),
            ask: None,
            waits: Arc::default(),
        };
        use strand_compiler::check::dbus::Introspect;
        let got = waiting.methods(
            true,
            "net.hadess.PowerProfiles",
            "/net/hadess/PowerProfiles",
        );
        let got = got.unwrap().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (
                got[0].interface.as_str(),
                got[0].name.as_str(),
                got[0].inputs.len()
            ),
            ("net.hadess.PowerProfiles", "HoldProfile", 3)
        );
        assert_eq!(
            waiting.methods(false, "org.example.Gone", "/"),
            Some(Err("no answer".into()))
        );
        // Not waiting: asked off the caller's thread, then answered.
        let asked = Arc::new(AtomicBool::new(false));
        let flag = asked.clone();
        let quick = BusIntrospector {
            cache: Arc::default(),
            methods: Arc::new(strand_introspect::Cache::with_fetch(
                strand_introspect::TTL,
                move |_, _, _| Ok(vec![method.clone()]),
            )),
            ask: Some(Arc::new(move || flag.store(true, Ordering::SeqCst))),
            waits: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(quick.methods(false, "org.example.Late", "/"), None);
        let start = std::time::Instant::now();
        while !asked.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(10), "never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            quick.methods(false, "org.example.Late", "/"),
            Some(Ok(m)) if m.len() == 1
        ));
    }

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
        let date = Ty::Record(types.find_schema_record("Date").unwrap());
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
            date.clone(),
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
        // (doc-formats) A `Date` from date text and from Unix time.
        for d in [
            Data::text("2026-11-07T19:59:32Z"),
            Data::text(" Sat, 07 Nov 2026 19:59:32 +0000 "),
            Data::text("1794084315"),
            Data::Int(1_794_084_315),
            Data::Float(1_794_081_310_891.0),
        ] {
            let v = c(&date, d.clone());
            assert!(matches!(v, Some(Value::Record(..))), "{d:?} gave {v:?}");
        }
        assert_eq!(c(&date, Data::Float(f64::NAN)), None);
        assert_eq!(c(&date, Data::Bool(true)), None);
        // Garbage in a `Date?` is no date: the memo's default, null.
        let maybe = Ty::Optional(Box::new(date.clone()));
        assert_eq!(c(&maybe, Data::text("08:04 PM")), None);
        assert_eq!(default_value(types, &maybe), Value::Null);
    }

    /// (doc-formats) A `Date` field filled from JSON APIs: GitHub's RFC
    /// 3339 `published_at`, USGS's Unix milliseconds, wttr.in's date
    /// alone.
    #[test]
    fn a_date_field_reads_text_and_unix_time() {
        let types = &Schema::builtin().types;
        let date = Ty::Record(types.find_schema_record("Date").unwrap());
        let field = |doc: &str, key: &str| {
            let k: Vec<String> = key.split('.').map(str::to_string).collect();
            let d = custom::Document::parse(doc).get(&k).unwrap();
            coerce(types, &date, &d).unwrap_or_else(|| panic!("{key} in {doc}"))
        };
        let n = |v: &Value, f: &str| v.field(types, f).cloned();
        let gh = field(
            r#"{"tag_name": "v0.52.1", "published_at": "2026-11-03T09:47:31Z"}"#,
            "published_at",
        );
        assert_eq!(n(&gh, "year"), Some(Value::int(2026)));
        assert_eq!(n(&gh, "month"), Some(Value::int(11)));
        assert!(matches!(n(&gh, "minute"), Some(Value::Num(..))));
        let usgs = field(
            r#"{"features": [{"properties": {"mag": 4.6, "time": 1794081310891}}]}"#,
            "time",
        );
        assert_eq!(n(&usgs, "year"), Some(Value::int(2026)), "{usgs:?}");
        let wttr = field(r#"{"weather": [{"date": "2026-11-07"}]}"#, "date");
        assert_eq!(n(&wttr, "day"), Some(Value::int(7)));
        assert_eq!(n(&wttr, "hour"), Some(Value::Null));
        // A number reads as its digits do: 8 are YYYYMMDD, fewer no date.
        let ymd = field(r#"{"date": 20261107}"#, "date");
        assert_eq!(n(&ymd, "year"), Some(Value::int(2026)));
        assert_eq!(n(&ymd, "day"), Some(Value::int(7)));
        assert_eq!(coerce(types, &date, &Data::Int(2026)), None);
        assert_eq!(coerce(types, &date, &Data::Int(0)), None);
    }

    /// (doc-formats) The types a test config declares.
    fn typed(src: &str) -> TypeTable {
        let mut map = strand_compiler::SourceMap::new();
        map.add("t.strand", src.to_string());
        let c = strand_compiler::compile(&map);
        assert_eq!(c.errors(), 0, "{:?}", c.diagnostics);
        c.program.types.clone()
    }

    fn user(types: &TypeTable, name: &str) -> Ty {
        Ty::Record(types.find_record(name).unwrap())
    }

    fn xml_at(doc: &str, key: &str) -> Data {
        let k: Vec<String> = key.split('.').map(str::to_string).collect();
        custom::Document::parse(doc).get(&k).unwrap()
    }

    /// (doc-formats) The declared type decides: a list type over one
    /// value is a list of it (if it converts, else nothing, reported); a
    /// single type over a list reads its first item.
    #[test]
    fn a_list_type_wraps_one_value_and_a_single_type_takes_the_first() {
        let types = &Schema::builtin().types;
        let c = |ty: &Ty, d: Data| coerce(types, ty, &d);
        let ints = Ty::List(Box::new(Ty::INT), false);
        let texts = Ty::List(Box::new(Ty::Prim(Prim::Text)), false);
        assert_eq!(
            c(&ints, Data::Int(3)),
            Some(Value::list(vec![Value::int(3)]))
        );
        assert_eq!(
            c(&ints, Data::text("4")),
            Some(Value::list(vec![Value::int(4)]))
        );
        assert_eq!(c(&ints, Data::text("x")), None, "a report, not a silent []");
        assert_eq!(c(&ints, Data::Null), None);
        // A value per line is not one item: `22` with the rest lost.
        assert_eq!(c(&ints, Data::text("22\n80\n443")), None);
        let floats = Ty::List(Box::new(Ty::Prim(Prim::Float)), false);
        assert_eq!(c(&floats, Data::text("0.5\n0.25")), None);
        assert!(read_output(types, &ints, "22\n80\n443\n").is_err());
        // Nor are values on one line, by spaces or tabs (`pidof`).
        assert_eq!(c(&ints, Data::text("1234 5678")), None);
        assert_eq!(c(&ints, Data::text("22\t80\t443")), None);
        assert_eq!(c(&floats, Data::text("0.5 0.25")), None);
        assert!(read_output(types, &ints, "1234 5678\n").is_err());
        let durations = Ty::List(Box::new(Ty::Prim(Prim::Duration)), false);
        assert_eq!(c(&durations, Data::text("5 10")), None);
        let element = xml_at("<r><ports>22 80 443</ports><d>1</d></r>", "ports");
        assert_eq!(c(&ints, element), None);
        let element = xml_at("<r><ports p=\"tcp\">22 80 443</ports><d>1</d></r>", "ports");
        assert!(custom::is_element(&element));
        assert_eq!(c(&ints, element), None);
        let element = xml_at("<r><t u=\"C\">45</t><d>1</d></r>", "t");
        assert_eq!(c(&ints, element), Some(Value::list(vec![Value::int(45)])));
        // A number and its unit is still one item.
        assert_eq!(
            c(&ints, Data::text("45 °C")),
            Some(Value::list(vec![Value::int(45)]))
        );
        assert_eq!(
            c(&ints, Data::text("16303392 kB")),
            Some(Value::list(vec![Value::int(16303392)]))
        );
        // A single number still reads the first of them, as on main.
        assert_eq!(c(&Ty::INT, Data::text("1234 5678")), Some(Value::int(1234)));
        assert_eq!(
            c(&texts, Data::text("a 1")),
            Some(Value::list(vec![Value::text("a 1")])),
            "text keeps it whole"
        );
        assert_eq!(
            c(&texts, Data::text("a\nb")),
            Some(Value::list(vec![Value::text("a\nb")])),
            "text keeps it whole"
        );
        let two = || Data::List(vec![Data::text("5"), Data::text("x")]);
        assert_eq!(c(&Ty::INT, two()), Some(Value::int(5)));
        assert_eq!(
            c(&Ty::INT, Data::List(vec![Data::text("x"), Data::Int(5)])),
            None,
            "the first item, not the first that converts"
        );
        assert_eq!(c(&Ty::INT, Data::List(vec![])), None);
        // The same from XML: one element or several.
        let one = xml_at("<r><c>a</c><d>1</d></r>", "c");
        assert_eq!(c(&texts, one), Some(Value::list(vec![Value::text("a")])));
        let many = xml_at("<r><c>a</c><c>b</c></r>", "c");
        assert_eq!(
            c(&texts, many.clone()),
            Some(Value::list(vec![Value::text("a"), Value::text("b")]))
        );
        assert_eq!(c(&Ty::Prim(Prim::Text), many), Some(Value::text("a")));
    }

    /// (doc-formats) A record read as a list of records is a one-item
    /// list only when it holds one of the item's fields: an API's error
    /// object (GitHub's rate limit) read as `[Release]` does not convert
    /// and is reported, where it would be one item of defaults.
    #[test]
    fn a_record_without_the_item_s_fields_is_no_list_of_it() {
        let types = &typed(
            "type Release { tag_name: text; published_at: Date?; html_url: text }
type Port { portid: int; state: text? }
",
        );
        let releases = Ty::List(Box::new(user(types, "Release")), false);
        let error = Data::Record {
            ty: "".into(),
            fields: vec![
                ("message".into(), Data::text("API rate limit exceeded")),
                ("documentation_url".into(), Data::text("https://x")),
            ],
        };
        assert_eq!(coerce(types, &releases, &error), None);
        let maybe = Ty::List(
            Box::new(Ty::Optional(Box::new(user(types, "Release")))),
            false,
        );
        assert_eq!(coerce(types, &maybe, &error), None);
        // One that holds a field is a one-item list, JSON or XML.
        let one = Data::Record {
            ty: "".into(),
            fields: vec![("tag_name".into(), Data::text("v1"))],
        };
        let Some(Value::List(l)) = coerce(types, &releases, &one) else {
            panic!("one release")
        };
        assert_eq!(l.len(), 1);
        let ports = Ty::List(Box::new(user(types, "Port")), false);
        let port = xml_at("<ports><port portid=\"22\"/></ports>", "port");
        let Some(Value::List(l)) = coerce(types, &ports, &port) else {
            panic!("one port")
        };
        assert_eq!(l[0].field(types, "portid"), Some(&Value::int(22)));
        let other = xml_at("<r><port><x>1</x></port></r>", "port");
        assert_eq!(coerce(types, &ports, &other), None);
    }

    /// (doc-formats) An empty element read as a record or a list of
    /// anything but text is empty, with nothing to report: GPX's empty
    /// `<trkseg>`, nmap's `<ports/>`.
    #[test]
    fn an_empty_element_is_an_empty_record_or_list() {
        let types = &typed(
            "type Pt { lat: angle; lon: angle }
type Seg { trkpt: [Pt] }
type Trk { name: text?; trkseg: [Seg] }
type Ports { port: [Pt] }
",
        );
        let trk = xml_at(
            "<gpx><trk><name>a</name><trkseg>\n</trkseg></trk></gpx>",
            "trk",
        );
        let mut nested = None;
        let t = coerce_noting(types, &user(types, "Trk"), &trk, &mut nested).unwrap();
        let Some(Value::List(segs)) = t.field(types, "trkseg") else {
            panic!("{t:?}")
        };
        assert!(segs.is_empty());
        assert_eq!(nested, None, "nothing to report");
        let ports = coerce(types, &user(types, "Ports"), &Data::text("")).unwrap();
        assert_eq!(ports.field(types, "port"), Some(&Value::list(Vec::new())));
        // A list of text keeps the blank text; a `Date` is no blank.
        let texts = Ty::List(Box::new(Ty::Prim(Prim::Text)), false);
        assert_eq!(
            coerce(types, &texts, &Data::text("")),
            Some(Value::list(vec![Value::text("")]))
        );
        let date = Ty::Record(types.find_schema_record("Date").unwrap());
        assert_eq!(coerce(types, &date, &Data::text(" ")), None);
    }

    /// (doc-formats) A record whose `text` field is the record again does
    /// not read text: wrapping it would recurse forever.
    #[test]
    fn a_recursive_text_field_does_not_read_text() {
        let types = &typed(
            "type Node { text: Node?; n: int? }
type Tree { text: [Tree] }
",
        );
        assert_eq!(coerce(types, &user(types, "Node"), &Data::text("x")), None);
        assert_eq!(coerce(types, &user(types, "Tree"), &Data::text("x")), None);
        let d = xml_at("<r><node>x</node></r>", "node");
        assert_eq!(coerce(types, &user(types, "Node"), &d), None);
    }

    /// (doc-formats) An element read as a scalar is its text: an
    /// attribute beside it changes nothing; one with no text at all
    /// (Atom's `<link href rel/>`) does not convert.
    #[test]
    fn an_element_reads_as_its_text() {
        let types = &typed(
            "type Size { units: text; text: int }
type Guid { isPermaLink: bool; text: text }
",
        );
        let c = |ty: &Ty, d: &Data| coerce(types, ty, d);
        let guid = xml_at(
            "<i><guid isPermaLink=\"false\"> tag:x,1 </guid></i>",
            "guid",
        );
        assert_eq!(
            c(&Ty::Prim(Prim::Text), &guid),
            Some(Value::text("tag:x,1"))
        );
        let g = c(&user(types, "Guid"), &guid).unwrap();
        assert_eq!(g.field(types, "isPermaLink"), Some(&Value::Bool(false)));
        assert_eq!(g.field(types, "text"), Some(&Value::text("tag:x,1")));
        let size = xml_at(
            "<node><size units=\"bytes\">68719476736</size></node>",
            "size",
        );
        assert_eq!(c(&Ty::INT, &size), Some(Value::int(68_719_476_736)));
        let s = c(&user(types, "Size"), &size).unwrap();
        assert_eq!(s.field(types, "units"), Some(&Value::text("bytes")));
        assert_eq!(s.field(types, "text"), Some(&Value::int(68_719_476_736)));
        let link = xml_at(
            "<e><link href=\"x\" rel=\"alternate\"/><id>1</id></e>",
            "link",
        );
        assert_eq!(c(&Ty::Prim(Prim::Text), &link), None);
        // JSON objects never read as text.
        let json = Data::Record {
            ty: "".into(),
            fields: vec![("text".into(), Data::text("x"))],
        };
        assert_eq!(c(&Ty::Prim(Prim::Text), &json), None);
        // An enum and a date from an element's text.
        let date = Ty::Record(types.find_schema_record("Date").unwrap());
        let updated = xml_at(
            "<e><updated kind=\"x\">2026-11-03T09:47:53Z</updated></e>",
            "updated",
        );
        assert!(matches!(c(&date, &updated), Some(Value::Record(..))));
    }

    /// (doc-formats) The type decides text or record whatever the
    /// element's shape: one of text alone reads into a record with a
    /// `text` field as that field, as its siblings with an attribute do
    /// (RSS's bare `<guid>`, `<category>` without `domain`, fontconfig's
    /// `<dir>` without `prefix`, Atom's `<title>` without `type`).
    #[test]
    fn text_alone_reads_into_a_record_with_a_text_field() {
        let types = &typed(
            "type Guid { isPermaLink: bool?; text: text }
type Dir { prefix: text?; text: text }
type Size { units: text?; text: int }
type Named { name: text }
",
        );
        let c = |ty: &Ty, d: &Data| coerce(types, ty, d);
        let guid = xml_at("<i><guid>https://x/1</guid></i>", "guid");
        let g = c(&user(types, "Guid"), &guid).unwrap();
        assert_eq!(g.field(types, "text"), Some(&Value::text("https://x/1")));
        assert_eq!(g.field(types, "isPermaLink"), Some(&Value::Null));
        // fontconfig: four `<dir>`s, one with an attribute; all four read.
        let conf = "<fontconfig><dir>/usr/share/fonts</dir><dir>/usr/local/share/fonts</dir><dir prefix=\"xdg\">fonts</dir><dir>~/.fonts</dir></fontconfig>";
        let dirs = Ty::List(Box::new(user(types, "Dir")), false);
        let Some(Value::List(l)) = c(&dirs, &xml_at(conf, "dir")) else {
            panic!("dirs")
        };
        let read: Vec<_> = l
            .iter()
            .map(|d| {
                (
                    d.field(types, "text").cloned(),
                    d.field(types, "prefix").cloned(),
                )
            })
            .collect();
        assert_eq!(
            read,
            vec![
                (Some(Value::text("/usr/share/fonts")), Some(Value::Null)),
                (
                    Some(Value::text("/usr/local/share/fonts")),
                    Some(Value::Null)
                ),
                (Some(Value::text("fonts")), Some(Value::text("xdg"))),
                (Some(Value::text("~/.fonts")), Some(Value::Null)),
            ]
        );
        // The `text` field keeps its own type.
        let size = c(&user(types, "Size"), &Data::text(" 42 ")).unwrap();
        assert_eq!(size.field(types, "text"), Some(&Value::int(42)));
        // A record with no `text` field still does not read text.
        assert_eq!(c(&user(types, "Named"), &Data::text("x")), None);
    }

    /// (doc-formats) A declared type's fields find their names exactly,
    /// then as attributes, then by the part after a prefix, then (for
    /// `text`) the element's text.
    #[test]
    fn type_fields_match_by_local_name() {
        let types = &typed(concat!(
            "type Post { title: text; creator: text; encoded: text; link: text }\n",
            "type Outline { text: text; xmlUrl: text?; outline: [Outline] }\n",
        ));
        let item = xml_at(
            concat!(
                "<rss xmlns:dc=\"urn:dc\" xmlns:content=\"urn:c\" xmlns:atom=\"urn:a\">",
                "<item><title>T</title><atom:link href=\"self\"/><link>home</link>",
                "<dc:creator>Ada</dc:creator><content:encoded><![CDATA[<p>x</p>]]></content:encoded>",
                "</item></rss>"
            ),
            "item",
        );
        let p = coerce(types, &user(types, "Post"), &item).unwrap();
        assert_eq!(p.field(types, "creator"), Some(&Value::text("Ada")));
        assert_eq!(p.field(types, "encoded"), Some(&Value::text("<p>x</p>")));
        assert_eq!(
            p.field(types, "link"),
            Some(&Value::text("home")),
            "never atom:link"
        );
        let opml = custom::Document::parse(include_str!(
            "../../../strand-services/tests/fixtures/xml/opml.xml"
        ));
        let body = opml.get(&["body".to_string()]).unwrap();
        let outlines = coerce(
            types,
            &Ty::List(Box::new(user(types, "Outline")), false),
            &xml_at_data(&body, "outline"),
        )
        .unwrap();
        let Value::List(items) = &outlines else {
            panic!("{outlines:?}")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].field(types, "text"),
            Some(&Value::text("Tech")),
            "@text first"
        );
        let inner = items[0].field(types, "outline").unwrap();
        let Value::List(inner) = inner else {
            panic!("{inner:?}")
        };
        assert_eq!(
            inner[1].field(types, "text"),
            Some(&Value::text("Dev's log"))
        );
        assert_eq!(
            inner[0].field(types, "xmlUrl"),
            Some(&Value::text("https://example.org/tools.rss"))
        );
        assert_eq!(items[1].field(types, "outline"), Some(&Value::list(vec![])));
    }

    fn xml_at_data(d: &Data, name: &str) -> Data {
        let found = custom::lookup(d, name);
        match found.len() {
            1 => found[0].clone().into_owned(),
            _ => Data::List(found.into_iter().map(|c| c.into_owned()).collect()),
        }
    }

    /// (doc-formats) XML has no numbers: length, angle and duration read
    /// plain number text as a JSON number (px, deg, seconds).
    #[test]
    fn number_types_read_number_text() {
        let types = &typed("type Point { lat: angle; lon: angle; ele: float; time: Date }\n");
        let c = |ty: &Ty, d: Data| coerce(types, ty, &d);
        assert_eq!(
            c(&Ty::Prim(Prim::Angle), Data::text("45.5")),
            Some(Value::Num(45.5, Num::Deg))
        );
        assert_eq!(
            c(&Ty::Prim(Prim::Duration), Data::text(" 30 ")),
            Some(Value::from(Duration::from_secs(30)))
        );
        assert_eq!(
            c(&Ty::Prim(Prim::Duration), Data::text("200ms")),
            Some(Value::Num(200.0, Num::Ms)),
            "a unit still reads"
        );
        assert_eq!(
            c(&Ty::Prim(Prim::Length), Data::text("12")),
            Some(Value::Num(12.0, Num::Px))
        );
        assert_eq!(c(&Ty::Prim(Prim::Duration), Data::text("-3")), None);
        let gpx = custom::Document::parse(include_str!(
            "../../../strand-services/tests/fixtures/xml/gpx.xml"
        ));
        let pts = gpx.get(&["trkpt".to_string()]).unwrap();
        let pts = c(&Ty::List(Box::new(user(types, "Point")), false), pts).unwrap();
        let Value::List(pts) = &pts else {
            panic!("{pts:?}")
        };
        assert_eq!(pts.len(), 2);
        assert_eq!(
            pts[1].field(types, "lat"),
            Some(&Value::Num(45.773, Num::Deg))
        );
        assert_eq!(pts[1].field(types, "ele"), Some(&Value::float(302.0)));
        let year = pts[0]
            .field(types, "time")
            .and_then(|d| d.field(types, "year"))
            .cloned();
        assert_eq!(year, Some(Value::int(2010)));
    }

    /// (doc-formats) Blank text read as an optional type other than text
    /// is null, with nothing to report; a text keeps it.
    #[test]
    fn blank_optional_values_are_null() {
        let types = &typed("type R { n: int?; d: Date?; t: text?; e: int?; p: path? }\n");
        let r = xml_at("<x><r><n/><d> </d><t></t><p/></r></x>", "r");
        let mut nested = None;
        let v = coerce_noting(types, &user(types, "R"), &r, &mut nested).unwrap();
        assert_eq!(v.field(types, "n"), Some(&Value::Null));
        assert_eq!(v.field(types, "d"), Some(&Value::Null));
        assert_eq!(v.field(types, "t"), Some(&Value::text("")));
        assert_eq!(v.field(types, "e"), Some(&Value::Null), "missing");
        assert_eq!(v.field(types, "p"), Some(&Value::Null), "a blank path");
        assert_eq!(nested, None, "nothing to report");
        let maybe = Ty::Optional(Box::new(Ty::INT));
        assert_eq!(coerce(types, &maybe, &Data::text("  ")), Some(Value::Null));
    }

    /// (doc-formats) A field inside a declared type that held a value and
    /// did not convert is noted with its path (it used to take its
    /// default silently); a missing one is not.
    #[test]
    fn a_nested_field_that_does_not_convert_is_reported() {
        let types = &typed("type Post { title: text; pubDate: Date?; n: int }\n");
        let posts = Ty::List(Box::new(user(types, "Post")), false);
        let items = xml_at(
            concat!(
                "<rss><channel><item><title>a</title><pubDate>Sat Nov  7 20:45:15 2026</pubDate></item>",
                "<item><title>b</title><n>many</n></item></channel></rss>"
            ),
            "item",
        );
        let mut nested = None;
        let v = coerce_noting(types, &posts, &items, &mut nested).unwrap();
        let Value::List(list) = &v else {
            panic!("{v:?}")
        };
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].field(types, "pubDate"), Some(&Value::Null));
        assert_eq!(
            nested,
            Some(Nested {
                path: "[0].pubDate".into(),
                held: "\"Sat Nov  7 20:45:15 2026\"".into(),
                ty: "Date?".into(),
            })
        );
        // A list item that does not convert is left out, and noted.
        let mut nested = None;
        let ints = Ty::List(Box::new(Ty::INT), false);
        let v = coerce_noting(
            types,
            &ints,
            &Data::List(vec![Data::Int(1), Data::text("x")]),
            &mut nested,
        );
        assert_eq!(v, Some(Value::list(vec![Value::int(1)])));
        assert_eq!(nested.map(|n| n.path), Some("[1]".to_string()));
        // Missing fields say nothing.
        let mut nested = None;
        coerce_noting(
            types,
            &posts,
            &xml_at("<r><item/></r>", "item"),
            &mut nested,
        );
        assert_eq!(nested, None);
    }

    /// (services-round) A `from dir` service's listing converts into the
    /// language's `[File]`: each file's path, name, size and modified
    /// `Date` as `walls.files` reads them.
    #[test]
    fn a_listed_directory_is_a_list_of_files() {
        let types = &Schema::builtin().types;
        let file = types.find_record("File").unwrap();
        let files = Ty::List(Box::new(Ty::Record(file)), false);
        let dir = std::env::temp_dir().join(format!("strand-dir-coerce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.png"), b"12345").unwrap();
        std::fs::write(dir.join("a.jpg"), b"1").unwrap();
        let listed = strand_services::custom::list_dir(&dir).unwrap().unwrap();
        let v = coerce(types, &files, &Data::List(listed)).unwrap();
        let Value::List(items) = &v else {
            panic!("{v:?}")
        };
        assert_eq!(items.len(), 2);
        let a = &items[0];
        assert_eq!(a.field(types, "name"), Some(&Value::text("a.jpg")));
        assert_eq!(
            a.field(types, "path"),
            Some(&Value::text(dir.join("a.jpg").to_string_lossy()))
        );
        assert_eq!(items[1].field(types, "size"), Some(&Value::int(5)));
        let year = a
            .field(types, "modified")
            .and_then(|d| d.field(types, "year"))
            .and_then(Value::as_f64);
        assert!(year.is_some_and(|y| y >= 2024.0), "{year:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
        // The boot's question hangs for 1.5 s, and every later one until the
        // test is over, so a reload that waits for one sees `answers` move.
        let answers = Arc::new(AtomicUsize::new(0));
        let answered = answers.clone();
        let (end, ended) = std::sync::mpsc::channel::<()>();
        let ended = std::sync::Mutex::new(ended);
        let cache = Arc::new(strand_introspect::Cache::with_fetch(
            Duration::from_millis(50),
            move |_, name, _| {
                let nth = n.fetch_add(1, Ordering::SeqCst);
                if name == "org.example.Hung" {
                    if nth == 0 {
                        std::thread::sleep(Duration::from_millis(1500));
                    } else if let Ok(ended) = ended.lock() {
                        let _ = ended.recv_timeout(Duration::from_secs(60));
                    }
                    answered.fetch_add(1, Ordering::SeqCst);
                    return Err("no answer".into());
                }
                std::thread::sleep(Duration::from_millis(200));
                Ok(vec![strand_introspect::Property {
                    interface: "org.example.Late".into(),
                    name: "Level".into(),
                    signature: "d".into(),
                    readable: true,
                    writable: false,
                    emits: strand_introspect::Emits::True,
                }])
            },
        ));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (check, waits) = dbus_check_on(cache, None, move || {
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
        let out = loader.changed([(file.clone(), true)]);
        assert_eq!(
            answers.load(Ordering::SeqCst),
            1,
            "the reload waited on the bus"
        );
        assert!(out.build.is_some(), "{:?}", out.diagnostics);
        // A service added by a reload: not checked yet, committed; its
        // late answer (a read-only property written `rw`) checks again.
        let late =
            "service late from dbus session \"org.example.Late\" { level: float rw = Level }\n";
        std::fs::write(&file, format!("{hung}{late}bar B {{ text \"y\" }}\n")).unwrap();
        let out = loader.changed([(file.clone(), true)]);
        assert_eq!(
            answers.load(Ordering::SeqCst),
            1,
            "the reload waited on the bus"
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
        drop(end);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `from dbus` service whose bus does not answer at boot commits
    /// with a `check::dbus_unchecked` warning in the boot's outcome (what
    /// `strand run` logs and tells `strand watch`); once the daemon
    /// answers (asked again by a later compile), `recheck` checks the
    /// running files again and the outcome says the warning is gone
    /// (`cleared`, no diagnostics).
    #[test]
    fn a_dbus_warning_is_reported_and_resolved_by_a_recheck() {
        use std::sync::atomic::AtomicBool;
        use strand_compiler::reconcile::loader::Loader;
        let dir = std::env::temp_dir().join(format!("strand-dbus-warn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.strand");
        let svc = "service ppd from dbus system \"net.hadess.PowerProfiles\" { profile: text rw = ActiveProfile }\n";
        std::fs::write(&file, format!("{svc}bar B {{ text ppd.profile }}\n")).unwrap();
        let up = Arc::new(AtomicBool::new(false));
        let u = up.clone();
        let cache = Arc::new(strand_introspect::Cache::with_fetch(
            Duration::from_millis(50),
            move |_, _, _| {
                if !u.load(Ordering::SeqCst) {
                    return Err("no system bus".into());
                }
                Ok(vec![strand_introspect::Property {
                    interface: "net.hadess.PowerProfiles".into(),
                    name: "ActiveProfile".into(),
                    signature: "s".into(),
                    readable: true,
                    writable: true,
                    emits: strand_introspect::Emits::True,
                }])
            },
        ));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (check, waits) = dbus_check_on(cache, None, move || {
            let _ = tx.send(());
        });
        let mut loader =
            Loader::new(&dir, crate::services::schema().clone(), None).with_check(check);
        let boot = loader.boot();
        waits.stop_waiting();
        assert!(boot.build.is_some());
        let codes: Vec<String> = boot
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        assert_eq!(
            codes,
            ["check::dbus_unchecked"],
            "the boot reports the warning"
        );
        assert!(!boot.diagnostics[0].is_error());
        // The daemon comes up; a save compiles with the remembered
        // answer (still warned) and asks again off the reload path.
        up.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(
            &file,
            format!("{svc}bar B {{ text ppd.profile {{ opacity: 0.5 }} }}\n"),
        )
        .unwrap();
        let out = loader.changed([(file.clone(), true)]);
        assert!(out.build.is_some(), "{:?}", out.diagnostics);
        assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the answer asks for a check");
        let out = loader.recheck();
        assert!(out.build.is_none(), "the running program is unchanged");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out.cleared, "the warning is resolved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (dbus-unfollowed) From the introspection XML to the warning: a
    /// field bound to a property whose interface says
    /// `EmitsChangedSignal=false` (after its properties, as logind
    /// writes it) is `check::dbus_unfollowed` at boot, the config still
    /// loading; a property annotated `const` is silent.
    #[test]
    fn a_never_signalled_property_from_introspection_is_warned_about() {
        use strand_compiler::reconcile::loader::Loader;
        let dir = std::env::temp_dir().join(format!("strand-dbus-emits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.strand");
        std::fs::write(
            &file,
            "service login from dbus system \"org.freedesktop.login1\" \"/org/freedesktop/login1\" {\n  docked: bool = Docked\n  lid: text = HandleLidSwitch\n}\nbar B { text login.docked ? login.lid : \"\" }\n",
        )
        .unwrap();
        const XML: &str = r#"<node>
  <interface name="org.freedesktop.login1.Manager">
    <property name="Docked" type="b" access="read"/>
    <property name="HandleLidSwitch" type="s" access="read">
      <annotation name="org.freedesktop.DBus.Property.EmitsChangedSignal" value="const"/>
    </property>
    <annotation name="org.freedesktop.DBus.Property.EmitsChangedSignal" value="false"/>
  </interface>
</node>"#;
        let cache = Arc::new(strand_introspect::Cache::with_fetch(
            Duration::from_secs(60),
            |_, _, _| Ok(strand_introspect::parse(XML)),
        ));
        let (check, waits) = dbus_check_on(cache, None, || {});
        let mut loader =
            Loader::new(&dir, crate::services::schema().clone(), None).with_check(check);
        let boot = loader.boot();
        waits.stop_waiting();
        assert!(boot.build.is_some(), "{:?}", boot.diagnostics);
        let shown: Vec<(String, String)> = boot
            .diagnostics
            .iter()
            .map(|d| (d.code.to_string(), d.message.clone()))
            .collect();
        assert_eq!(
            shown,
            [(
                "check::dbus_unfollowed".to_string(),
                "`org.freedesktop.login1`'s `Docked` sends no change signal: `login.docked` keeps the value it read at start".to_string()
            )]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The design's ppd service, as the PowerProfiles daemon is reached
    /// through the environment's system bus (`DBUS_SYSTEM_BUS_ADDRESS`, a
    /// private bus with dbusmock's `power_profiles_daemon`): the loader's
    /// boot compile with [`dbus_check`] holds a misspelled property back
    /// with its did-you-mean, and commits the correct one. The compile runs
    /// in a child process (this test, re-run), so the address is that
    /// process's environment and no other test's.
    #[test]
    fn the_loader_checks_from_dbus_services_on_the_system_bus() {
        use strand_compiler::reconcile::loader::Loader;
        use strand_services::testing::{DbusMock, PrivateBus};
        const CHILD: &str = "STRAND_TEST_LOADER_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let (check, _waits) = dbus_check(None, || {});
            let mut loader = Loader::new(
                std::path::Path::new(&dir),
                crate::services::schema().clone(),
                None,
            )
            .with_check(check);
            let boot = loader.boot();
            // On lines of their own (the harness prints the test name first).
            println!("\nBUILD {}", boot.build.is_some());
            for d in &boot.diagnostics {
                let fix: Vec<&str> = d
                    .suggestions
                    .iter()
                    .map(|s| s.replacement.as_str())
                    .collect();
                println!("DIAG {} {}", d.code, fix.join(","));
            }
            return;
        }
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let Some(_ppd) = DbusMock::start(
            &bus,
            "power_profiles_daemon",
            true,
            None,
            "net.hadess.PowerProfiles",
        ) else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("strand-ppd-loader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let boot = |field: &str| -> String {
            std::fs::write(
                dir.join("ppd.strand"),
                format!(
                    "service ppd from dbus system \"net.hadess.PowerProfiles\" {{ {field} }}\nbar B {{ text join(\" \", ppd.profile) }}\n"
                ),
            )
            .unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "services::custom::tests::the_loader_checks_from_dbus_services_on_the_system_bus",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, &dir)
                .envs(bus.env())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                out.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            stdout
                .lines()
                .filter(|l| l.starts_with("BUILD ") || l.starts_with("DIAG "))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(boot("profile: text rw = ActiveProfile"), "BUILD true");
        let bad = boot("profile: text rw = ActiveProfil");
        assert_eq!(
            bad, "BUILD false\nDIAG check::dbus_property ActiveProfile",
            "held back, with the did-you-mean"
        );
        // A type the property's signature (`s`) does not convert to.
        let ty = boot("profile: bool = ActiveProfile");
        assert_eq!(ty, "BUILD false\nDIAG check::dbus_type ", "{ty}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

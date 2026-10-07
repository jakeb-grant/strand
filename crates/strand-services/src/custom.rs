//! No-code services (design.md: `service ppd from dbus system
//! "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }`, and
//! `from file`, `from listen`, `from poll`): a [`Spec`] the language side
//! builds from the declaration, run on the service contract like any
//! builtin service (start on the first reader, stop 5 s after the last,
//! retried with backoff when the run fails, failures as diagnostics).
//!
//! Each declared service is one [`Custom`] store (registered once per
//! declaration): its fields are the items of one keyed list, `values`,
//! keyed by the field's index, holding what the source said as untyped
//! [`Data`]; the language side converts each to its declared type (this
//! crate never sees the language's types). A write of an `rw` field is an
//! item write: the D-Bus property is `Set` and the item reported.
//!
//! - **dbus**: the object at the path (by default the bus name with `.` as
//!   `/`) is introspected; each field reads the property its key names
//!   (the interface named like the bus name first, else the first that
//!   has it), and `PropertiesChanged` drives the fields. The daemon is
//!   followed across restarts; nothing polls.
//! - **file**: the file is read now and whenever it changes (an inotify
//!   watch on it and its directory: atomic replacements, sysfs
//!   notifications); nothing polls.
//! - **listen**: the command runs while the service runs; each line it
//!   prints is a document (read leniently: bytes that are not UTF-8 do not
//!   stop it). It ending is a failure (retried with backoff). Commands run
//!   in a process group of their own, ended whole (`SIGTERM`, then
//!   `SIGKILL` after [`KILL_GRACE`]) when the service stops or restarts.
//! - **poll**: the command (or file) is read every interval, only while a
//!   reader is visible.
//!
//! A document ([`Document`]) is JSON when it parses as JSON, else
//! `key=value` (or `key: value`) lines when every line is one, else plain
//! text. A field's key path walks a JSON object (a first key the top
//! level lacks is searched for below it); in lines it is the key; a
//! scalar document (a number, a word) is every field's value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use zbus::zvariant::{OwnedValue, Value as ZValue};

use crate::dbus::{self, Daemon, DaemonEvent};
use crate::{Cx, Data, Msg, ServiceError, Store, Write, service};

/// The schema text of the [`Custom`] store. Never added to the
/// language's schema: each declaration is its own record there.
pub const SCHEMA: &str = "\
/// One field's value of a no-code service, by the field's index.
record CustomValue key index {
  /// The field's index in its declaration.
  index: int
  /// What the source said.
  value: any
}

/// A no-code service's fields.
service custom {
  /// Which declaration it runs.
  spec: int
  /// Its fields' values, by index.
  values: [CustomValue]
}
";

/// How long a polled command may run before it is killed.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest document read (a file, a line, a command's output).
pub const MAX_DOCUMENT: usize = 1 << 20;

/// What a declaration reads from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Dbus {
        system: bool,
        name: String,
        path: Option<String>,
    },
    File {
        path: PathBuf,
    },
    Listen {
        command: Vec<String>,
    },
    Poll {
        target: PollTarget,
        every: Duration,
    },
}

/// What a `poll` service reads.
#[derive(Clone, Debug, PartialEq)]
pub enum PollTarget {
    Command(Vec<String>),
    File(PathBuf),
}

/// One field: its name, the key path it reads, whether it is written.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSpec {
    pub name: String,
    pub key: Vec<String>,
    pub rw: bool,
}

/// A no-code service declaration, as the language side hands it over.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// The service's name (`ppd`).
    pub name: String,
    pub source: Source,
    pub fields: Vec<FieldSpec>,
}

static SPECS: Mutex<Option<HashMap<i64, Arc<Spec>>>> = Mutex::new(None);
static NEXT_SPEC: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// Make `spec` known to the bodies; the id goes in the store's `spec`
/// field ([`Custom::seeded`]).
pub fn register(spec: Spec) -> i64 {
    let id = NEXT_SPEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SPECS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(id, Arc::new(spec));
    id
}

/// Forget spec `id` (a declaration replaced or removed).
pub fn forget(id: i64) {
    if let Some(m) = SPECS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        m.remove(&id);
    }
}

fn spec_of(id: i64) -> Option<Arc<Spec>> {
    SPECS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()?
        .get(&id)
        .cloned()
}

/// One field's value.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "CustomValue", key = index)]
pub struct CustomValue {
    pub index: i64,
    pub value: Data,
}

/// See the module docs.
#[service(name = "custom")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Custom {
    /// Which declaration it runs.
    pub spec: i64,
    /// Its fields' values, by index.
    #[store(keyed)]
    pub values: Vec<CustomValue>,
}

impl Custom {
    /// The state a host seeds for spec `id` with `fields` fields, all
    /// null.
    pub fn seeded(id: i64, fields: usize) -> Custom {
        Custom {
            spec: id,
            values: (0..fields)
                .map(|i| CustomValue {
                    index: i as i64,
                    value: Data::Null,
                })
                .collect(),
        }
    }

    fn set(&mut self, i: usize, v: Data) {
        if let Some(slot) = self.values.get_mut(i) {
            slot.value = v;
        }
    }
}

// --- Documents --------------------------------------------------------------

/// A source's output, parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum Document {
    Json(serde_json::Value),
    Pairs(Vec<(String, String)>),
    Text(String),
}

impl Document {
    /// See the module docs.
    pub fn parse(text: &str) -> Document {
        let t = text.trim();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            return Document::Json(v);
        }
        let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
        let pairs: Option<Vec<(String, String)>> = lines
            .iter()
            .map(|l| {
                let (k, v) = l.split_once('=').or_else(|| l.split_once(':'))?;
                let k = k.trim();
                (!k.is_empty() && !k.contains(char::is_whitespace)).then(|| {
                    let v = v.trim();
                    let v = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .unwrap_or(v);
                    (k.to_string(), v.to_string())
                })
            })
            .collect();
        match pairs {
            Some(p) if !p.is_empty() => Document::Pairs(p),
            _ => Document::Text(t.to_string()),
        }
    }

    /// The value at `key` (see the module docs); `None`: not in it.
    pub fn get(&self, key: &[String]) -> Option<Data> {
        match self {
            Document::Json(v) => match v {
                serde_json::Value::Object(_) => json_path(v, key).map(json_data),
                other => Some(json_data(other)),
            },
            Document::Pairs(p) => {
                let k = key.join(".");
                p.iter()
                    .find(|(n, _)| *n == k)
                    .or_else(|| p.iter().find(|(n, _)| n.eq_ignore_ascii_case(&k)))
                    .map(|(_, v)| Data::text(v.as_str()))
            }
            Document::Text(t) => Some(Data::text(t.as_str())),
        }
    }
}

fn json_path<'a>(v: &'a serde_json::Value, key: &[String]) -> Option<&'a serde_json::Value> {
    let (first, rest) = key.split_first()?;
    let start = match v.get(first.as_str()) {
        Some(s) => s,
        None => find_key(v, first, 0)?,
    };
    rest.iter().try_fold(start, |cur, k| match cur {
        serde_json::Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => cur.get(k.as_str()),
    })
}

/// The first value under key `k` below `v`, depth first.
fn find_key<'a>(v: &'a serde_json::Value, k: &str, depth: usize) -> Option<&'a serde_json::Value> {
    if depth > 16 {
        return None;
    }
    match v {
        serde_json::Value::Object(m) => m
            .get(k)
            .or_else(|| m.values().find_map(|c| find_key(c, k, depth + 1))),
        serde_json::Value::Array(a) => a.iter().find_map(|c| find_key(c, k, depth + 1)),
        _ => None,
    }
}

/// JSON as untyped data: objects are records of no type name.
pub fn json_data(v: &serde_json::Value) -> Data {
    match v {
        serde_json::Value::Null => Data::Null,
        serde_json::Value::Bool(b) => Data::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Data::Int(i),
            None => Data::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Data::text(s.as_str()),
        serde_json::Value::Array(a) => Data::List(a.iter().map(json_data).collect()),
        serde_json::Value::Object(m) => Data::Record {
            ty: "".into(),
            fields: m
                .iter()
                .map(|(k, v)| (k.clone().into(), json_data(v)))
                .collect(),
        },
    }
}

/// Apply `doc` to `state`: every field the document holds.
fn apply_doc(spec: &Spec, doc: &Document, state: &mut Custom) {
    for (i, f) in spec.fields.iter().enumerate() {
        if let Some(v) = doc.get(&f.key) {
            state.set(i, v);
        }
    }
}

// --- D-Bus values -----------------------------------------------------------

/// A D-Bus value as untyped data: integers as `Int`, `a{sv}` (and any
/// dictionary with text keys) as a record of no type name, structs as
/// lists, variants unwrapped.
pub fn dbus_data(v: &ZValue<'_>) -> Data {
    match v {
        ZValue::U8(n) => Data::Int(i64::from(*n)),
        ZValue::Bool(b) => Data::Bool(*b),
        ZValue::I16(n) => Data::Int(i64::from(*n)),
        ZValue::U16(n) => Data::Int(i64::from(*n)),
        ZValue::I32(n) => Data::Int(i64::from(*n)),
        ZValue::U32(n) => Data::Int(i64::from(*n)),
        ZValue::I64(n) => Data::Int(*n),
        ZValue::U64(n) => Data::Int(i64::try_from(*n).unwrap_or(i64::MAX)),
        ZValue::F64(f) => Data::Float(*f),
        ZValue::Str(s) => Data::text(s.as_str()),
        ZValue::Signature(s) => Data::text(s.to_string()),
        ZValue::ObjectPath(p) => Data::text(p.as_str()),
        ZValue::Value(inner) => dbus_data(inner),
        ZValue::Array(a) => Data::List(a.iter().map(dbus_data).collect()),
        ZValue::Dict(d) => Data::Record {
            ty: "".into(),
            fields: d
                .iter()
                .map(|(k, v)| {
                    let key = match dbus_data(k) {
                        Data::Text(t) => t.to_string(),
                        other => format!("{other:?}"),
                    };
                    (key.into(), dbus_data(v))
                })
                .collect(),
        },
        ZValue::Structure(s) => Data::List(s.fields().iter().map(dbus_data).collect()),
        #[allow(unreachable_patterns)]
        _ => Data::Null,
    }
}

fn text_of(d: &Data) -> Option<String> {
    match d {
        Data::Text(t) => Some(t.to_string()),
        Data::Enum { variant, .. } => Some(variant.to_string()),
        Data::Int(n) => Some(n.to_string()),
        Data::Float(f) => Some(f.to_string()),
        Data::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn int_of(d: &Data) -> Option<i64> {
    match d {
        Data::Int(n) => Some(*n),
        Data::Float(f) if f.is_finite() => Some(f.round() as i64),
        Data::Bool(b) => Some(i64::from(*b)),
        Data::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

/// `d` as a D-Bus value of type `signature`, for `Set`. Basic types and
/// arrays of strings; anything else cannot be written.
pub fn to_dbus(d: &Data, signature: &str) -> Result<OwnedValue, String> {
    let bad = || format!("a {} cannot be written as a D-Bus `{signature}`", d.kind());
    let int = || int_of(d).ok_or_else(bad);
    let v: ZValue<'static> = match signature {
        "b" => match d {
            Data::Bool(b) => ZValue::Bool(*b),
            _ => ZValue::Bool(int()? != 0),
        },
        "y" => ZValue::U8(u8::try_from(int()?).map_err(|_| bad())?),
        "n" => ZValue::I16(i16::try_from(int()?).map_err(|_| bad())?),
        "q" => ZValue::U16(u16::try_from(int()?).map_err(|_| bad())?),
        "i" => ZValue::I32(i32::try_from(int()?).map_err(|_| bad())?),
        "u" => ZValue::U32(u32::try_from(int()?).map_err(|_| bad())?),
        "x" => ZValue::I64(int()?),
        "t" => ZValue::U64(u64::try_from(int()?).map_err(|_| bad())?),
        "d" => match d {
            Data::Float(f) => ZValue::F64(*f),
            Data::Int(n) => ZValue::F64(*n as f64),
            Data::Text(t) => ZValue::F64(t.trim().parse().map_err(|_| bad())?),
            _ => return Err(bad()),
        },
        "s" => ZValue::from(text_of(d).ok_or_else(bad)?),
        "o" => ZValue::ObjectPath(
            zbus::zvariant::ObjectPath::try_from(text_of(d).ok_or_else(bad)?)
                .map_err(|e| e.to_string())?,
        ),
        "as" => match d {
            Data::List(items) => ZValue::from(
                items
                    .iter()
                    .map(|i| text_of(i).ok_or_else(bad))
                    .collect::<Result<Vec<String>, String>>()?,
            ),
            _ => return Err(bad()),
        },
        _ => return Err(bad()),
    };
    OwnedValue::try_from(v).map_err(|e| e.to_string())
}

// --- The body ---------------------------------------------------------------

impl Custom {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let id = cx.state().spec;
        let Some(spec) = spec_of(id) else {
            // A declaration replaced meanwhile: nothing to run.
            cx.ready();
            while cx.recv().await.is_some() {}
            return Ok(());
        };
        let r = match &spec.source {
            Source::Dbus { system, name, path } => {
                let path = path
                    .clone()
                    .unwrap_or_else(|| strand_introspect::default_path(name));
                run_dbus(&mut cx, &spec, *system, name, &path).await
            }
            Source::File { path } => run_file(&mut cx, &spec, path).await,
            Source::Listen { command } => run_listen(&mut cx, &spec, command).await,
            Source::Poll { target, every } => run_poll(&mut cx, &spec, target, *every).await,
        };
        r.map_err(|e| ServiceError(format!("`{}`: {}", spec.name, e.0)))
    }
}

/// A write this source cannot take (only D-Bus properties are written;
/// the checker refuses `rw` elsewhere): reported as it is, so the
/// optimistic value goes.
fn refuse(cx: &mut Cx<Custom>, w: &Write) -> bool {
    cx.report(w, |_| {})
}

/// The property each field reads: its interface and signature.
type Bound = Vec<Option<(String, String, String)>>;

fn bind(spec: &Spec, name: &str, props: &[strand_introspect::Property]) -> Bound {
    spec.fields
        .iter()
        .map(|f| {
            let prop = f.key.first()?;
            let found = props
                .iter()
                .filter(|p| p.name == *prop)
                .min_by_key(|p| p.interface != name);
            match found {
                Some(p) => Some((p.interface.clone(), p.name.clone(), p.signature.clone())),
                None => {
                    log::warn!("custom service: `{name}` has no property `{prop}`");
                    None
                }
            }
        })
        .collect()
}

/// The strings each field's property has held, so an enum variant is
/// written back as the daemon spells it.
struct Seen(Vec<Vec<Arc<str>>>);

/// How many spellings a field remembers.
const SEEN_MAX: usize = 64;

impl Seen {
    fn new(fields: usize) -> Seen {
        Seen(vec![Vec::new(); fields])
    }

    /// Remember `d` (read from field `i`'s property), and hand it back.
    fn note(&mut self, i: usize, d: Data) -> Data {
        if let (Data::Text(t), Some(seen)) = (&d, self.0.get_mut(i))
            && !seen.contains(t)
            && t.len() <= 256
        {
            if seen.len() == SEEN_MAX {
                seen.remove(0);
            }
            seen.push(t.clone());
        }
        d
    }

    /// The ways to write `d` to field `i`, in the order to try them. An
    /// enum variant is the string read before that names it (any case,
    /// `-` for `_`), else its name, else its name with `-` for `_`.
    fn spellings(&self, i: usize, d: &Data) -> Vec<Data> {
        let Data::Enum { variant, .. } = d else {
            return vec![d.clone()];
        };
        let norm = |s: &str| s.to_ascii_lowercase().replace('-', "_");
        let want = norm(variant);
        if let Some(t) = self
            .0
            .get(i)
            .and_then(|seen| seen.iter().rev().find(|t| norm(t) == want))
        {
            return vec![Data::Text(t.clone())];
        }
        let mut out = vec![Data::text(&**variant)];
        let hyphens = variant.replace('_', "-");
        if hyphens != **variant {
            out.push(Data::text(hyphens));
        }
        out
    }
}

/// The values written to each field lately, oldest first, until the
/// daemon's PropertiesChanged for the latest comes back: a signal for an
/// earlier write that arrives after a later write was reported (a slider
/// dragged, a quick double click) is that write's late echo, not news,
/// and must not show the old value until the latest echo arrives.
struct Echoes(Vec<Vec<(Data, std::time::Instant)>>);

/// How long a written value waits for its echo (a daemon that does not
/// signal a change it was asked for leaves nothing behind for long).
const ECHO_WAIT: Duration = Duration::from_secs(2);

/// How many writes to one field wait for their echoes.
const ECHO_MAX: usize = 8;

impl Echoes {
    fn new(fields: usize) -> Echoes {
        Echoes(vec![Vec::new(); fields])
    }

    /// `d` was written to field `i` (as the daemon will signal it).
    fn wrote(&mut self, i: usize, d: Data) {
        if let Some(ring) = self.0.get_mut(i) {
            if ring.len() == ECHO_MAX {
                ring.remove(0);
            }
            ring.push((d, std::time::Instant::now()));
        }
    }

    /// The daemon signalled `d` for field `i`: whether to show it. An
    /// echo of a write older than the latest is not shown (the writes up
    /// to it are settled); the latest's echo, or anything else, is.
    fn shows(&mut self, i: usize, d: &Data) -> bool {
        let Some(ring) = self.0.get_mut(i) else {
            return true;
        };
        ring.retain(|(_, at)| at.elapsed() < ECHO_WAIT);
        match ring.iter().rposition(|(w, _)| w == d) {
            Some(p) if p + 1 < ring.len() => {
                ring.drain(..=p);
                false
            }
            _ => {
                ring.clear();
                true
            }
        }
    }
}

async fn run_dbus(
    cx: &mut Cx<Custom>,
    spec: &Spec,
    system: bool,
    name: &str,
    path: &str,
) -> Result<(), ServiceError> {
    let conn = if system {
        cx.system().await
    } else {
        cx.session().await
    };
    let conn = match conn {
        Ok(c) => c,
        Err(e) => {
            return dbus::idle_without_bus(cx, if system { "system" } else { "session" }, e).await;
        }
    };
    let mut daemon = Daemon::new(&conn, name).await?;
    daemon
        .subscribe(
            "props",
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(dbus::PROPERTIES)?
                .member("PropertiesChanged")?
                .path(path.to_string())?
                .build(),
        )
        .await?;
    // The strings each field's property has held: an enum variant is
    // written as the one it was read from (`power_saver` as
    // `power-saver`).
    let mut seen = Seen::new(spec.fields.len());
    let mut echoes = Echoes::new(spec.fields.len());
    loop {
        // (Re)read everything from the current owner.
        let mut bound: Bound = vec![None; spec.fields.len()];
        let mut state = cx.state().clone();
        for v in &mut state.values {
            v.value = Data::Null;
        }
        if daemon.owner().is_some() {
            let props = dbus::timed_for(dbus::READ_TIMEOUT, async {
                strand_introspect::properties_on(&conn, name, path)
                    .await
                    .map_err(zbus::Error::Failure)
            })
            .await
            .map_err(|e| ServiceError(format!("{name} did not answer: {e}")))?;
            bound = bind(spec, name, &props);
            let mut ifaces: Vec<&String> = bound.iter().flatten().map(|(i, _, _)| i).collect();
            ifaces.sort();
            ifaces.dedup();
            for iface in ifaces {
                let all = dbus::get_all(&conn, name, path, iface)
                    .await
                    .map_err(|e| ServiceError(format!("{name} did not answer: {e}")))?;
                for (i, b) in bound.iter().enumerate() {
                    if let Some((bi, prop, _)) = b
                        && bi == iface
                        && let Some(v) = all.get(prop)
                    {
                        state.set(i, seen.note(i, dbus_data(v)));
                    }
                }
            }
        }
        if !cx.update(|s| *s = state) {
            return Ok(());
        }
        cx.ready();
        loop {
            tokio::select! {
                ev = daemon.next() => match ev {
                    None => return Err(ServiceError("the bus connection ended".into())),
                    Some(DaemonEvent::Owner) => break,
                    Some(DaemonEvent::Signal(m)) => {
                        let Some(c) = dbus::properties_changed(&m) else { continue };
                        if c.path != path {
                            continue;
                        }
                        let iface = c.iface.clone();
                        let mut props = dbus::Props::new();
                        dbus::apply_changed(&conn, name, &mut props, c).await;
                        let mut state = cx.state().clone();
                        let mut any = false;
                        for (i, b) in bound.iter().enumerate() {
                            if let Some((bi, prop, _)) = b
                                && *bi == iface
                                && let Some(v) = props.get(prop)
                            {
                                let d = dbus_data(v);
                                if !echoes.shows(i, &d) {
                                    continue;
                                }
                                state.set(i, seen.note(i, d));
                                any = true;
                            }
                        }
                        if any && !cx.update(|s| *s = state) {
                            return Ok(());
                        }
                    }
                },
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Write(w)) => {
                        let i = w.key.as_ref().and_then(|k| match k { Data::Int(n) => usize::try_from(*n).ok(), _ => None });
                        let target = i.and_then(|i| bound.get(i).cloned().flatten().map(|b| (i, b)));
                        let writable = i.and_then(|i| spec.fields.get(i)).is_some_and(|f| f.rw);
                        let Some((i, (iface, prop, sig))) = target.filter(|_| writable) else {
                            if !refuse(cx, &w) {
                                return Ok(());
                            }
                            continue;
                        };
                        let value = match &w.field_value {
                            Data::Record { .. } => w.field_value.field("value").cloned().unwrap_or(Data::Null),
                            _ => w.value.clone(),
                        };
                        // Each way to spell it, until one is taken.
                        let mut set = Err(String::new());
                        for value in seen.spellings(i, &value) {
                            set = match to_dbus(&value, &sig) {
                                Ok(v) => {
                                    // As the daemon will signal it back.
                                    let echo = dbus_data(&v);
                                    dbus::set(&conn, name, path, &iface, &prop, ZValue::from(v)).await.map(|()| (value, echo)).map_err(|e| e.to_string())
                                }
                                Err(e) => Err(e),
                            };
                            if set.is_ok() {
                                break;
                            }
                        }
                        // What the property holds now: the written value,
                        // or (refused) a fresh read.
                        let now = match set {
                            Ok((value, echo)) => {
                                echoes.wrote(i, echo);
                                seen.note(i, value)
                            }
                            Err(e) => {
                                log::warn!("{}.{}: {name} refused the write: {e}", spec.name, spec.fields[i].name);
                                match dbus::timed(dbus::get(&conn, name, path, &iface, &prop)).await {
                                    Ok(v) => seen.note(i, dbus_data(&v)),
                                    Err(_) => cx.state().values.get(i).map(|v| v.value.clone()).unwrap_or_default(),
                                }
                            }
                        };
                        if !cx.report(&w, |s| s.set(i, now)) {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// How long reading a `from file` (or polled) file may take: a hung
/// network or FUSE mount fails the run instead of holding the services
/// thread.
const READ_WAIT: Duration = Duration::from_secs(5);

/// Read `path` as a document (`None`: it is not there). Opened without
/// blocking, and only a regular file is read (sysfs and procfs
/// attributes are): a FIFO or a device would block the reader.
fn read_doc(path: &Path) -> Result<Option<Document>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    match f.metadata() {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(format!("{}: not a regular file", path.display())),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let mut buf = Vec::new();
    f.take(MAX_DOCUMENT as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(Document::parse(&String::from_utf8_lossy(&buf))))
}

/// [`read_doc`] off the services thread, at most [`READ_WAIT`].
async fn read_doc_bounded(path: &Path) -> Result<Option<Document>, ServiceError> {
    let p = path.to_path_buf();
    match tokio::time::timeout(READ_WAIT, tokio::task::spawn_blocking(move || read_doc(&p))).await {
        Ok(Ok(r)) => r.map_err(ServiceError),
        Ok(Err(e)) => Err(ServiceError(format!("{}: {e}", path.display()))),
        Err(_) => Err(ServiceError(format!(
            "{}: not read within {READ_WAIT:?}",
            path.display()
        ))),
    }
}

/// An inotify watch on a file's directory (its replacement, creation and
/// removal) and on the file itself (writes in place, sysfs
/// notifications).
struct FileWatch {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    path: PathBuf,
    /// The directory's watch: only its events naming the file count.
    dir_wd: i32,
    name: Vec<u8>,
}

fn watch_file(path: &Path) -> std::io::Result<FileWatch> {
    use rustix::fs::inotify::{self, WatchFlags};
    use std::os::unix::ffi::OsStrExt;
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir_wd = inotify::add_watch(
        &fd,
        dir,
        WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_TO
            | WatchFlags::MOVED_FROM
            | WatchFlags::CREATE
            | WatchFlags::DELETE,
    )?;
    let w = FileWatch {
        fd: tokio::io::unix::AsyncFd::new(fd)?,
        path: path.to_path_buf(),
        dir_wd,
        name: path
            .file_name()
            .map(|n| n.as_bytes().to_vec())
            .unwrap_or_default(),
    };
    w.watch_inode();
    Ok(w)
}

impl FileWatch {
    /// Watch the file now at the path (again after it was replaced or
    /// created; the same inode keeps its watch).
    fn watch_inode(&self) {
        use rustix::fs::inotify::{self, WatchFlags};
        let _ = inotify::add_watch(
            self.fd.get_ref(),
            &self.path,
            WatchFlags::MODIFY | WatchFlags::CLOSE_WRITE,
        );
    }

    /// Reads every queued event: `Some(true)` when one concerns the file
    /// (not another file of its directory), `None` when the watch failed.
    fn drain(&self) -> Option<bool> {
        let mut buf = [0u8; 4096];
        let mut any = false;
        loop {
            match rustix::io::read(self.fd.get_ref(), &mut buf) {
                Ok(0) => break,
                Ok(n) => any |= concerns(&buf[..n], self.dir_wd, &self.name),
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return None,
            }
        }
        if any {
            self.watch_inode();
        }
        Some(any)
    }
}

/// Whether raw inotify events `buf` concern the file `name` of the
/// directory watched as `dir_wd`: any event of the file's own watch, a
/// directory event naming it, or a queue overflow (anything may have
/// changed).
fn concerns(buf: &[u8], dir_wd: i32, name: &[u8]) -> bool {
    const HEADER: usize = 16;
    const Q_OVERFLOW: u32 = 0x4000;
    let mut at = 0;
    while at + HEADER <= buf.len() {
        let word = |i: usize| {
            let b = &buf[at + i..at + i + 4];
            [b[0], b[1], b[2], b[3]]
        };
        let wd = i32::from_ne_bytes(word(0));
        let mask = u32::from_ne_bytes(word(4));
        let len = u32::from_ne_bytes(word(12)) as usize;
        let end = (at + HEADER).saturating_add(len).min(buf.len());
        let raw = &buf[at + HEADER..end];
        let named = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        if mask & Q_OVERFLOW != 0 || wd != dir_wd || named == name {
            return true;
        }
        at = end;
    }
    false
}

async fn run_file(cx: &mut Cx<Custom>, spec: &Spec, path: &Path) -> Result<(), ServiceError> {
    // Watch first, then read: no change is lost in between.
    let watch = watch_file(path)
        .map_err(|e| ServiceError(format!("cannot watch {}: {e}", path.display())))?;
    let mut last: Option<Document> = None;
    let mut first = true;
    loop {
        // Read (first, or after a change to the file itself).
        let doc = read_doc_bounded(path).await?;
        if first || doc != last {
            let mut state = cx.state().clone();
            match &doc {
                Some(d) => apply_doc(spec, d, &mut state),
                // Gone: every field null until it is back.
                None => state.values.iter_mut().for_each(|v| v.value = Data::Null),
            }
            last = doc;
            if !cx.update(|s| *s = state) {
                return Ok(());
            }
        }
        if first {
            cx.ready();
            first = false;
        }
        // Until something happens to the file.
        loop {
            tokio::select! {
                r = watch.fd.readable() => {
                    let changed = match r {
                        Ok(mut g) => {
                            g.clear_ready();
                            watch.drain()
                        }
                        Err(_) => None,
                    };
                    match changed {
                        None => return Err(ServiceError("the file watch failed".into())),
                        Some(true) => break,
                        // Another file of the directory.
                        Some(false) => {}
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Write(w)) => if !refuse(cx, &w) { return Ok(()) },
                    Some(_) => {}
                },
            }
        }
    }
}

/// How long a stopped command's process group has to end after
/// `SIGTERM` before it is sent `SIGKILL`.
pub const KILL_GRACE: Duration = Duration::from_millis(500);

/// A command running in a process group of its own: dropped (the
/// service stopped or restarted, a poll timed out or finished), the
/// whole group is ended, so nothing it started outlives it.
struct Proc {
    child: tokio::process::Child,
    pgid: Option<i32>,
}

impl Proc {
    fn spawn(argv: &[String]) -> Result<(Proc, tokio::process::ChildStdout), ServiceError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| ServiceError("no command".into()))?;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
            .map_err(|e| ServiceError(format!("cannot run `{program}`: {e}")))?;
        let pgid = child.id().and_then(|p| i32::try_from(p).ok());
        let stdout = child.stdout.take();
        let proc = Proc { child, pgid };
        // Without its output, dropping `proc` ends the group.
        let stdout = stdout.ok_or_else(|| ServiceError("no output".into()))?;
        Ok((proc, stdout))
    }

    /// How it ended, waiting a moment for it (it closed its output).
    async fn status(&mut self) -> String {
        match tokio::time::timeout(Duration::from_millis(200), self.child.wait()).await {
            Ok(Ok(s)) => s.to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "its output closed; killed".into(),
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let Some(pgid) = self.pgid.filter(|p| *p > 1) else {
            return;
        };
        // SAFETY: kill(2) on our own child's process group; no memory is
        // involved. ESRCH (the group is gone) is the common answer.
        let alive = unsafe { libc::kill(-pgid, libc::SIGTERM) } == 0;
        if !alive {
            return;
        }
        let kill = move || {
            std::thread::sleep(KILL_GRACE);
            // SAFETY: as above.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        };
        if std::thread::Builder::new()
            .name("strand-kill".into())
            .spawn(kill)
            .is_err()
        {
            // SAFETY: as above.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

/// A command's output as lines of at most [`MAX_DOCUMENT`] bytes (a
/// longer one is skipped whole, up to its newline), decoded leniently
/// (bytes that are not UTF-8 become U+FFFD). Cancel safe: everything
/// read is kept in `self`.
struct Lines<R> {
    reader: tokio::io::BufReader<R>,
    buf: Vec<u8>,
    skipping: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> Lines<R> {
    fn new(r: R) -> Lines<R> {
        Lines {
            reader: tokio::io::BufReader::new(r),
            buf: Vec::new(),
            skipping: false,
        }
    }

    /// The next line (`None`: the output ended).
    async fn next(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                let last = std::mem::take(&mut self.buf);
                let skipped = std::mem::replace(&mut self.skipping, false);
                return Ok((!last.is_empty() && !skipped).then(|| decode(last)));
            }
            let newline = available.iter().position(|b| *b == b'\n');
            let chunk = &available[..newline.unwrap_or(available.len())];
            if !self.skipping {
                if self.buf.len() + chunk.len() > MAX_DOCUMENT {
                    self.skipping = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
            }
            let used = newline.map_or(available.len(), |i| i + 1);
            self.reader.consume(used);
            if newline.is_some() {
                if std::mem::replace(&mut self.skipping, false) {
                    log::warn!("a line longer than {MAX_DOCUMENT} bytes was skipped");
                    continue;
                }
                return Ok(Some(decode(std::mem::take(&mut self.buf))));
            }
        }
    }
}

fn decode(mut line: Vec<u8>) -> String {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    match String::from_utf8(line) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

async fn run_listen(cx: &mut Cx<Custom>, spec: &Spec, argv: &[String]) -> Result<(), ServiceError> {
    let (mut proc, stdout) = Proc::spawn(argv)?;
    let mut lines = Lines::new(stdout);
    cx.ready();
    loop {
        tokio::select! {
            line = lines.next() => match line {
                Ok(Some(l)) => {
                    if l.trim().is_empty() {
                        continue;
                    }
                    let doc = Document::parse(&l);
                    let mut state = cx.state().clone();
                    apply_doc(spec, &doc, &mut state);
                    if !cx.update(|s| *s = state) {
                        return Ok(());
                    }
                }
                // Ended (or unreadable): its group is ended with `proc`,
                // and the failure retried with backoff.
                Ok(None) => {
                    let status = proc.status().await;
                    return Err(ServiceError(format!("`{}` ended ({status})", argv[0])));
                }
                Err(e) => {
                    return Err(ServiceError(format!("reading `{}`: {e}", argv[0])));
                }
            },
            m = cx.recv() => match m {
                None => return Ok(()),
                Some(Msg::Write(w)) => if !refuse(cx, &w) { return Ok(()) },
                Some(_) => {}
            },
        }
    }
}

/// Run `argv` once and parse what it printed (`None`: it failed, logged).
/// At most [`MAX_DOCUMENT`] bytes are read; the command's group ends
/// with it.
async fn poll_command(argv: &[String]) -> Option<Document> {
    use tokio::io::AsyncReadExt;
    let (mut proc, stdout) = match Proc::spawn(argv) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("{}", e.0);
            return None;
        }
    };
    let run = async {
        let mut out = Vec::new();
        stdout
            .take(MAX_DOCUMENT as u64)
            .read_to_end(&mut out)
            .await
            .ok()?;
        let status = proc.child.wait().await.ok()?;
        if !status.success() {
            log::warn!("`{}` failed ({status})", argv[0]);
            return None;
        }
        Some(Document::parse(&String::from_utf8_lossy(&out)))
    };
    match tokio::time::timeout(POLL_TIMEOUT, run).await {
        Ok(d) => d,
        Err(_) => {
            log::warn!("`{}` took more than {POLL_TIMEOUT:?}; killed", argv[0]);
            None
        }
    }
}

async fn run_poll(
    cx: &mut Cx<Custom>,
    spec: &Spec,
    target: &PollTarget,
    every: Duration,
) -> Result<(), ServiceError> {
    let mut first = true;
    loop {
        if cx.visible() {
            let doc = match target {
                PollTarget::Command(argv) => poll_command(argv).await,
                PollTarget::File(path) => read_doc_bounded(path).await?,
            };
            if let Some(doc) = doc {
                let mut state = cx.state().clone();
                apply_doc(spec, &doc, &mut state);
                if !cx.update(|s| *s = state) {
                    return Ok(());
                }
            }
            if first {
                cx.ready();
                first = false;
            }
        } else if first {
            cx.ready();
            first = false;
        }
        // Until the next poll, or until shown again.
        let sleep = tokio::time::sleep(every);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep, if cx.visible() => break,
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(true)) => break,
                    Some(Msg::Write(w)) => if !refuse(cx, &w) { return Ok(()) },
                    Some(_) => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> Vec<String> {
        k.split('.').map(str::to_string).collect()
    }

    #[test]
    fn documents_are_json_lines_or_text() {
        let j = Document::parse(
            r#"{"cpu": {"temp": 41.5, "cores": [1, 2]}, "name": "x", "nested": {"package": 7}}"#,
        );
        assert_eq!(j.get(&key("cpu.temp")), Some(Data::Float(41.5)));
        assert_eq!(j.get(&key("cpu.cores.1")), Some(Data::Int(2)));
        assert_eq!(j.get(&key("name")), Some(Data::text("x")));
        assert_eq!(j.get(&key("package")), Some(Data::Int(7)), "searched below");
        assert_eq!(j.get(&key("missing")), None);
        let n = Document::parse("45000\n");
        assert_eq!(n.get(&key("anything")), Some(Data::Int(45000)));
        let p = Document::parse("NAME=\"Arch Linux\"\nID=arch\n\n");
        assert_eq!(p.get(&key("NAME")), Some(Data::text("Arch Linux")));
        assert_eq!(p.get(&key("id")), Some(Data::text("arch")), "any case");
        let m = Document::parse("MemTotal:       16303392 kB\nMemFree: 1 kB");
        assert_eq!(m.get(&key("MemTotal")), Some(Data::text("16303392 kB")));
        let t = Document::parse("  performance \n");
        assert_eq!(t.get(&key("profile")), Some(Data::text("performance")));
        let words = Document::parse("hello world\nsecond line");
        assert!(matches!(words, Document::Text(_)));
    }

    #[test]
    fn dbus_values_convert_both_ways() {
        assert_eq!(dbus_data(&ZValue::U32(5)), Data::Int(5));
        assert_eq!(dbus_data(&ZValue::from("balanced")), Data::text("balanced"));
        assert_eq!(
            dbus_data(&ZValue::new(ZValue::Bool(true))),
            Data::Bool(true),
            "variants unwrapped"
        );
        let v = to_dbus(&Data::text("power-saver"), "s").unwrap();
        assert_eq!(String::try_from(v).unwrap(), "power-saver");
        let v = to_dbus(&Data::Float(3.0), "u").unwrap();
        assert_eq!(u32::try_from(v).unwrap(), 3);
        assert!(to_dbus(&Data::Int(-1), "u").is_err());
        assert!(to_dbus(&Data::text("x"), "a{sv}").is_err());
        let e = Data::Enum {
            ty: "P".into(),
            variant: "performance".into(),
        };
        assert_eq!(
            String::try_from(to_dbus(&e, "s").unwrap()).unwrap(),
            "performance"
        );
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Lines that are not UTF-8 are read leniently and reading goes on;
    /// a line longer than a document is skipped up to its newline.
    #[test]
    fn lines_are_lenient_and_bounded() {
        let mut input = b"a=1\na=\xff\r\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_DOCUMENT + 10));
        input.extend(b"\na=3\nlast");
        let got = block_on(async {
            let mut lines = Lines::new(&input[..]);
            let mut got = Vec::new();
            while let Some(l) = lines.next().await.unwrap() {
                got.push(l);
            }
            got
        });
        assert_eq!(got, ["a=1", "a=\u{fffd}", "a=3", "last"]);
    }

    /// An enum variant is written as the property spelled it, else by
    /// its name, then with `-` for `_`.
    /// Two quick writes A then B: A's late echo is not shown (B was
    /// reported already), B's is; then a change from elsewhere is news,
    /// even one back to A.
    #[test]
    fn late_echoes_of_earlier_writes_are_not_shown() {
        let (a, b) = (Data::text("power-saver"), Data::text("performance"));
        let mut e = Echoes::new(2);
        e.wrote(0, a.clone());
        e.wrote(0, b.clone());
        assert!(!e.shows(0, &a), "A's late echo");
        assert!(e.shows(0, &b), "B's echo");
        assert!(e.shows(0, &a), "A from elsewhere, nothing pending");
        // A write whose echo is the latest is shown; another field is
        // untouched.
        e.wrote(0, a.clone());
        assert!(e.shows(1, &b));
        assert!(e.shows(0, &a));
        // An echo that never comes is forgotten.
        e.wrote(0, a.clone());
        e.wrote(0, b.clone());
        for w in &mut e.0[0] {
            w.1 -= ECHO_WAIT;
        }
        assert!(e.shows(0, &a), "no write waits any more");
    }

    /// A FIFO (or any file that is not regular) is refused at once, not
    /// waited on; a missing file is no document.
    #[test]
    fn only_regular_files_are_read() {
        let dir = std::env::temp_dir().join(format!("strand-custom-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = std::time::Instant::now();
        let e = read_doc(&fifo).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(read_doc(&dir.join("none")), Ok(None));
        std::fs::write(dir.join("doc"), "{\"a\": 1}").unwrap();
        assert!(read_doc(&dir.join("doc")).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Directory events re-read the file only when they name it; the
    /// file's own watch and an overflow always do.
    #[test]
    fn directory_events_count_only_for_the_file() {
        fn event(wd: i32, mask: u32, name: &str) -> Vec<u8> {
            let mut b = Vec::new();
            let len = if name.is_empty() {
                0
            } else {
                (name.len() + 1).next_multiple_of(16)
            };
            b.extend(wd.to_ne_bytes());
            b.extend(mask.to_ne_bytes());
            b.extend(0u32.to_ne_bytes());
            b.extend((len as u32).to_ne_bytes());
            let mut n = name.as_bytes().to_vec();
            n.resize(len, 0);
            b.extend(n);
            b
        }
        let (dir, file) = (1, 2);
        let other = [event(dir, 0x8, "other.json"), event(dir, 0x100, "x")].concat();
        assert!(!concerns(&other, dir, b"state.json"));
        let named = [other.clone(), event(dir, 0x80, "state.json")].concat();
        assert!(concerns(&named, dir, b"state.json"));
        assert!(concerns(&event(file, 0x2, ""), dir, b"state.json"));
        assert!(concerns(&event(-1, 0x4000, ""), dir, b"state.json"));
        // A prefix of the name is another file.
        assert!(!concerns(
            &event(dir, 0x8, "state.json.tmp"),
            dir,
            b"state.json"
        ));
    }

    #[test]
    fn enum_writes_use_the_spelling_read() {
        let e = |v: &'static str| Data::Enum {
            ty: "Profile".into(),
            variant: v.into(),
        };
        let mut seen = Seen::new(2);
        assert_eq!(
            seen.spellings(0, &e("power_saver")),
            [Data::text("power_saver"), Data::text("power-saver")]
        );
        assert_eq!(seen.spellings(0, &e("balanced")), [Data::text("balanced")]);
        seen.note(0, Data::text("Power-Saver"));
        assert_eq!(
            seen.spellings(0, &e("power_saver")),
            [Data::text("Power-Saver")]
        );
        assert_eq!(seen.spellings(1, &e("power_saver")).len(), 2, "per field");
        assert_eq!(seen.spellings(0, &Data::Int(3)), [Data::Int(3)]);
        for n in 0..SEEN_MAX * 2 {
            seen.note(0, Data::text(format!("v{n}")));
        }
        assert_eq!(seen.0[0].len(), SEEN_MAX, "bounded");
    }

    fn gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            // A zombie waiting for its reaper has ended.
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    fn wait_gone(pid: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if gone(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A command's whole process group ends with it: what it started
    /// does not outlive it.
    #[test]
    fn a_dropped_command_ends_its_whole_group() {
        let argv: Vec<String> = ["sh", "-c", "sleep 30 & echo $!; trap '' TERM; wait"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let grandchild = block_on(async {
            let (proc, stdout) = Proc::spawn(&argv).unwrap();
            let mut lines = Lines::new(stdout);
            let pid = lines.next().await.unwrap().unwrap();
            assert!(!gone(&pid));
            drop(proc);
            pid
        });
        assert!(wait_gone(&grandchild), "the grandchild {grandchild} ended");
        // A poll that times out ends its group too (here: one that
        // finished, leaving something behind).
        let dir = std::env::temp_dir().join(format!("strand-pgid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pid");
        let argv: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            format!("sleep 30 >/dev/null & echo $! > {}; echo 1", file.display()),
        ];
        let doc = block_on(poll_command(&argv));
        assert!(doc.is_some());
        let pid = std::fs::read_to_string(&file).unwrap();
        assert!(wait_gone(pid.trim()), "the poll's leftover {pid} ended");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

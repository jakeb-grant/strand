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
//!   prints is a document. It ending is a failure (retried with backoff).
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
                        state.set(i, dbus_data(v));
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
                                state.set(i, dbus_data(v));
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
                        let set = match to_dbus(&value, &sig) {
                            Ok(v) => dbus::set(&conn, name, path, &iface, &prop, ZValue::from(v)).await.map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        };
                        // What the property holds now: the written value,
                        // or (refused) a fresh read.
                        let now = match set {
                            Ok(()) => value,
                            Err(e) => {
                                log::warn!("{}.{}: {name} refused the write: {e}", spec.name, spec.fields[i].name);
                                match dbus::timed(dbus::get(&conn, name, path, &iface, &prop)).await {
                                    Ok(v) => dbus_data(&v),
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

/// Read `path` as a document (`None`: it is not there).
fn read_doc(path: &Path) -> Result<Option<Document>, String> {
    use std::io::Read;
    match std::fs::File::open(path) {
        Ok(f) => {
            let mut buf = Vec::new();
            f.take(MAX_DOCUMENT as u64)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            Ok(Some(Document::parse(&String::from_utf8_lossy(&buf))))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// An inotify watch on `path`'s directory (its replacement, creation and
/// removal) and on the file itself (writes in place, sysfs
/// notifications).
fn watch_file(path: &Path) -> std::io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use rustix::fs::inotify::{self, WatchFlags};
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    inotify::add_watch(
        &fd,
        dir,
        WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_TO
            | WatchFlags::MOVED_FROM
            | WatchFlags::CREATE
            | WatchFlags::DELETE,
    )?;
    let _ = inotify::add_watch(&fd, path, WatchFlags::MODIFY | WatchFlags::CLOSE_WRITE);
    tokio::io::unix::AsyncFd::new(fd)
}

fn drain(fd: &std::os::fd::OwnedFd) -> bool {
    let mut buf = [0u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(rustix::io::Errno::AGAIN) => return true,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

async fn run_file(cx: &mut Cx<Custom>, spec: &Spec, path: &Path) -> Result<(), ServiceError> {
    // Watch first, then read: no change is lost in between.
    let watch = watch_file(path)
        .map_err(|e| ServiceError(format!("cannot watch {}: {e}", path.display())))?;
    let mut last: Option<Document> = None;
    let read = |cx: &mut Cx<Custom>, last: &mut Option<Document>| -> Result<bool, ServiceError> {
        let doc = read_doc(path).map_err(ServiceError)?;
        if doc == *last {
            return Ok(true);
        }
        let mut state = cx.state().clone();
        match &doc {
            Some(d) => apply_doc(spec, d, &mut state),
            // Gone: every field null until it is back.
            None => state.values.iter_mut().for_each(|v| v.value = Data::Null),
        }
        *last = doc;
        Ok(cx.update(|s| *s = state))
    };
    if !read(cx, &mut last)? {
        return Ok(());
    }
    cx.ready();
    loop {
        tokio::select! {
            r = watch.readable() => {
                let ok = r.map(|mut g| g.clear_ready()).is_ok() && drain(watch.get_ref());
                if !ok {
                    return Err(ServiceError("the file watch failed".into()));
                }
                if !read(cx, &mut last)? {
                    return Ok(());
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

fn command(argv: &[String]) -> Result<tokio::process::Command, ServiceError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| ServiceError("no command".into()))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd)
}

async fn run_listen(cx: &mut Cx<Custom>, spec: &Spec, argv: &[String]) -> Result<(), ServiceError> {
    let mut child = command(argv)?
        .spawn()
        .map_err(|e| ServiceError(format!("cannot run `{}`: {e}", argv[0])))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ServiceError("no output".into()))?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    cx.ready();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if l.len() > MAX_DOCUMENT || l.trim().is_empty() {
                        continue;
                    }
                    let doc = Document::parse(&l);
                    let mut state = cx.state().clone();
                    apply_doc(spec, &doc, &mut state);
                    if !cx.update(|s| *s = state) {
                        return Ok(());
                    }
                }
                Ok(None) | Err(_) => {
                    let status = child.wait().await.map(|s| s.to_string()).unwrap_or_default();
                    return Err(ServiceError(format!("`{}` ended ({status})", argv[0])));
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
async fn poll_command(argv: &[String]) -> Option<Document> {
    let cmd = command(argv).ok();
    let run = async move {
        let out = cmd?.output().await.ok()?;
        if !out.status.success() {
            log::warn!("`{}` failed ({})", argv[0], out.status);
            return None;
        }
        let text =
            String::from_utf8_lossy(&out.stdout[..out.stdout.len().min(MAX_DOCUMENT)]).into_owned();
        Some(Document::parse(&text))
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
                PollTarget::File(path) => read_doc(path).map_err(ServiceError)?,
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
}

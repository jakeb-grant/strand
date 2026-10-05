//! Settings files: `state prefs from "prefs.toml" { accent: color =
//! #7aa2f7; compact: bool = false }`, typed two-way settings that
//! non-programmers can edit ("Settings files" in the design).
//!
//! The compiler supplies the schema: one [`FieldSpec`] per field, with its
//! name, its default and the codec between the VM's value type `V` and a
//! TOML item. [`Runtime::settings_file`] reads the file and returns one
//! [`Signal`] per field plus a [`Settings`] handle. The rules:
//!
//! * **Each field is checked on its own.** A value that does not decode
//!   keeps that field's last good value (the default at boot) and reports
//!   [`SettingsIssue::BadValue`]; the other fields still apply.
//! * **A TOML syntax error keeps every last good value**
//!   ([`SettingsIssue::Syntax`]). **A deleted key springs back to its
//!   default.**
//! * **Writes go through `toml_edit`.** A UI write (or `strand set
//!   prefs.compact true`) is an ordinary write to the field's signal; after
//!   [`PERSIST_DEBOUNCE`] of quiet it is applied to what the file holds,
//!   field by field, keeping comments, spacing and order. The edits run on
//!   the persist IO thread (never on the logic tick), merged per file.
//! * **Writes follow symlinks** and replace the target via a temp file in
//!   the target's directory plus rename. **A read-only target** (`/nix/
//!   store`: no write permission, `EACCES`, `EROFS`) gets an overlay in
//!   `$XDG_STATE_HOME/strand/settings/` instead, with a
//!   [`SettingsIssue::ReadOnly`] notice (once per file).
//! * **Who wins:** runtime overlay > file > default. The overlay holds
//!   redirected writes and explicit [`Settings::set_overlay`] values, and
//!   survives restarts. A UI write to a field that has an overlay value
//!   updates the overlay (writing the file would be shadowed). When the
//!   file changes under a field the overlay shadows, the reload reports
//!   [`SettingsIssue::Shadowed`]: `accent: file changed but runtime overlay
//!   wins [clear]` ([`Settings::clear_overlay`] is `[clear]`).
//!
//! The watcher calls [`Settings::reload`] when the file changes. A reload
//! sees edits still queued for the IO thread (so it never undoes a write
//! that has not reached the disk) and leaves a field the user wrote since
//! the last write-out alone (that write is saved next).

use std::cell::RefCell;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use toml_edit;
use toml_edit::{DocumentMut, Item};

use crate::error::Error;
use crate::persist::{
    FailSink, PERSIST_DEBOUNCE, PersistError, PersistStore, create_private_dir, escape_name,
    io_error, temp_next_to,
};
use crate::runtime::{Diagnostic, Runtime};
use crate::signal::Signal;

/// One field edit: set to an item, or remove the key (`None`).
pub(crate) type Edit = (Arc<str>, Option<Item>);

/// Where settings overlays live, and the IO thread their writes use. Cheap
/// to clone.
#[derive(Clone)]
pub struct SettingsStore {
    io: PersistStore,
    dir: PathBuf,
}

impl fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsStore")
            .field("dir", &self.dir)
            .finish()
    }
}

impl SettingsStore {
    /// A store keeping overlays in `dir`, with its own IO thread.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            io: PersistStore::new(dir.clone()),
            dir,
        }
    }

    /// A store keeping overlays in `dir` and writing through `io`'s thread
    /// ([`PersistStore::settings`] is the usual way).
    pub fn sharing(io: PersistStore, dir: impl Into<PathBuf>) -> Self {
        Self {
            io,
            dir: dir.into(),
        }
    }

    /// `$XDG_STATE_HOME/strand/settings` (see [`PersistStore::from_env`]),
    /// with its own IO thread.
    pub fn from_env() -> Result<Self, PersistError> {
        let persist = PersistStore::from_env()?;
        let dir = persist
            .dir()
            .parent()
            .map_or_else(|| persist.dir().join("settings"), |p| p.join("settings"));
        Ok(Self::new(dir))
    }

    /// The overlay directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The overlay file for the settings file declared at `file` (named by
    /// the declared path, not the link target, so a `home-manager switch`
    /// that swaps the link keeps the overlay).
    pub fn overlay_of(&self, file: &Path) -> PathBuf {
        let name = escape_name(&file.to_string_lossy());
        self.dir.join(format!("{name}.toml"))
    }

    /// Wait until queued writes reached the disk, at most `timeout`.
    pub fn sync(&self, timeout: std::time::Duration) -> bool {
        self.io.sync(timeout)
    }
}

type Decode<V> = Box<dyn Fn(&Item) -> Result<V, String>>;
type Encode<V> = Box<dyn Fn(&V) -> Item>;

/// One typed field of a settings file, from the compiler's schema.
pub struct FieldSpec<V> {
    name: Arc<str>,
    default: V,
    decode: Decode<V>,
    encode: Encode<V>,
}

impl<V> FieldSpec<V> {
    /// A field `name` with its declared `default`. `decode` checks a TOML
    /// item against the field's type (`Err` is the message shown with the
    /// file and field); `encode` writes a value back.
    pub fn new(
        name: impl Into<Arc<str>>,
        default: V,
        decode: impl Fn(&Item) -> Result<V, String> + 'static,
        encode: impl Fn(&V) -> Item + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            default,
            decode: Box::new(decode),
            encode: Box::new(encode),
        }
    }
}

impl<V: fmt::Debug> fmt::Debug for FieldSpec<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldSpec")
            .field("name", &self.name)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

/// A settings report for the overlay and `strand watch`
/// ([`Diagnostic::Settings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsNotice {
    /// The settings file as declared.
    pub file: Arc<str>,
    /// The field, when it is about one.
    pub field: Option<Arc<str>>,
    /// What happened.
    pub issue: SettingsIssue,
}

/// What a [`SettingsNotice`] is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsIssue {
    /// The file is not valid TOML: every field keeps its last good value.
    Syntax(Arc<str>),
    /// The file could not be read: every field keeps its last good value.
    Unreadable(Arc<str>),
    /// The field's value does not have its type: it keeps its last good
    /// value (an overlay value is dropped).
    BadValue(Arc<str>),
    /// The file changed this field, but the runtime overlay wins.
    Shadowed,
    /// The file is read-only: writes go to `overlay` (a notice).
    ReadOnly {
        /// The overlay file.
        overlay: PathBuf,
    },
    /// A write could not be saved (the value stays live). A file with a
    /// syntax error is not overwritten.
    WriteFailed(Arc<str>),
}

impl fmt::Display for SettingsNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = self.field.as_deref().unwrap_or("");
        match &self.issue {
            SettingsIssue::Syntax(m) => {
                write!(f, "{}: {m}; keeping every last good value", self.file)
            }
            SettingsIssue::Unreadable(m) => {
                write!(f, "{}: {m}; keeping every last good value", self.file)
            }
            SettingsIssue::BadValue(m) => {
                write!(
                    f,
                    "{}: {field}: {m}; keeping its last good value",
                    self.file
                )
            }
            SettingsIssue::Shadowed => {
                write!(f, "{field}: file changed but runtime overlay wins [clear]")
            }
            SettingsIssue::ReadOnly { overlay } => write!(
                f,
                "{} is read-only; changes are kept in {}",
                self.file,
                overlay.display()
            ),
            SettingsIssue::WriteFailed(m) => write!(f, "{}: not saved: {m}", self.file),
        }
    }
}

/// Settings edits queued for the IO thread.
#[derive(Clone)]
pub(crate) struct SettingsJob {
    pub(crate) edits: Vec<Edit>,
    layer: Layer,
    file: Arc<str>,
    sink: Arc<FailSink>,
}

#[derive(Clone)]
enum Layer {
    /// The settings file; falls back to `overlay` when it is read-only.
    File {
        overlay: PathBuf,
        read_only: Arc<AtomicBool>,
        noticed: Arc<AtomicBool>,
    },
    /// The overlay file.
    Overlay,
}

impl SettingsJob {
    /// Take `later`'s edits too (a later edit of a field replaces an
    /// earlier one).
    pub(crate) fn merge(&mut self, later: SettingsJob) {
        for (name, item) in later.edits {
            self.edits.retain(|(n, _)| *n != name);
            self.edits.push((name, item));
        }
    }

    fn report(&self, issue: SettingsIssue) {
        self.sink.push(Diagnostic::Settings(SettingsNotice {
            file: self.file.clone(),
            field: None,
            issue,
        }));
    }
}

/// Run a settings job on the IO thread; it reports its own outcome.
pub(crate) fn perform(key: &Path, job: &SettingsJob) {
    match &job.layer {
        Layer::Overlay => {
            if let Err(e) = edit_toml(key, &job.edits, true) {
                job.report(SettingsIssue::WriteFailed(e.message()));
            }
        }
        Layer::File {
            overlay,
            read_only,
            noticed,
        } => match edit_toml(key, &job.edits, false) {
            Ok(()) => {}
            Err(EditError::ReadOnly) => {
                read_only.store(true, Ordering::Release);
                match edit_toml(overlay, &job.edits, true) {
                    Ok(()) => {
                        if !noticed.swap(true, Ordering::AcqRel) {
                            job.report(SettingsIssue::ReadOnly {
                                overlay: overlay.clone(),
                            });
                        }
                    }
                    Err(e) => job.report(SettingsIssue::WriteFailed(e.message())),
                }
            }
            Err(e) => job.report(SettingsIssue::WriteFailed(e.message())),
        },
    }
}

enum EditError {
    ReadOnly,
    Syntax(String),
    Io(PersistError),
}

impl EditError {
    fn message(&self) -> Arc<str> {
        match self {
            Self::ReadOnly => Arc::from("read-only"),
            Self::Syntax(m) => Arc::from(format!("not overwritten, it has a syntax error: {m}")),
            Self::Io(e) => Arc::from(e.to_string()),
        }
    }
}

fn classify(path: &Path, e: &std::io::Error) -> EditError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => EditError::ReadOnly,
        _ => EditError::Io(io_error(path, e)),
    }
}

/// Follow `path` through symlinks (at most 40) to the file a write must
/// replace; a dangling link resolves to its target.
fn resolve_links(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&p) {
            Ok(t) if t.is_absolute() => p = t,
            Ok(t) => p = p.parent().map_or_else(|| t.clone(), |d| d.join(&t)),
            Err(_) => break,
        }
    }
    p
}

/// No write permission on the target or its directory: replacing it would
/// fail (or, as root, write where the owner said not to: `/nix/store` files
/// are mode 0444 in 0555 directories).
fn probe_read_only(target: &Path) -> bool {
    let ro = |p: &Path| fs::metadata(p).is_ok_and(|m| m.permissions().readonly());
    ro(target) || target.parent().is_some_and(ro)
}

/// Apply `edits` to `doc` in place: a replaced value keeps its decor
/// (spacing and trailing comment), keys keep their order and comments; a
/// new key is appended.
fn apply_edits(doc: &mut DocumentMut, edits: &[Edit]) {
    for (name, item) in edits {
        match item {
            None => {
                doc.remove(name);
            }
            Some(new) => match doc.get_mut(name) {
                Some(old) => {
                    let decor = old.as_value().map(|v| v.decor().clone());
                    match (decor, new.clone()) {
                        (Some(decor), Item::Value(mut v)) => {
                            *v.decor_mut() = decor;
                            *old = Item::Value(v);
                        }
                        (_, new) => *old = new,
                    }
                }
                None => {
                    doc.insert(name, new.clone());
                }
            },
        }
    }
}

/// Read-modify-write `path` with `edits` through `toml_edit`: follows
/// symlinks, writes a temp file in the target's directory (with the
/// target's permissions), `fsync`, rename, directory `fsync`. An overlay
/// lives in a private directory and is removed once it holds nothing.
fn edit_toml(path: &Path, edits: &[Edit], overlay: bool) -> Result<(), EditError> {
    let target = resolve_links(path);
    if !overlay && probe_read_only(&target) {
        return Err(EditError::ReadOnly);
    }
    let text = match fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(classify(&target, &e)),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| EditError::Syntax(e.to_string()))?;
    apply_edits(&mut doc, edits);
    let dir = target
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if overlay {
        create_private_dir(&dir).map_err(EditError::Io)?;
        if doc.is_empty() {
            return match fs::remove_file(&target) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(classify(&target, &e)),
            };
        }
    }
    let temp = temp_next_to(&dir, &target);
    let written = (|| {
        let mut f = fs::File::create(&temp)?;
        if let Ok(m) = fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        f.write_all(doc.to_string().as_bytes())?;
        f.sync_all()?;
        fs::rename(&temp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(classify(&target, &e));
    }
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Read a TOML file; a missing one is empty.
fn read_doc(path: &Path) -> Result<DocumentMut, SettingsIssue> {
    match fs::read_to_string(path) {
        Ok(t) => t
            .parse::<DocumentMut>()
            .map_err(|e| SettingsIssue::Syntax(Arc::from(e.to_string().trim_end()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(SettingsIssue::Unreadable(Arc::from(e.to_string()))),
    }
}

struct Field<V> {
    name: Arc<str>,
    default: V,
    decode: Decode<V>,
    encode: Encode<V>,
    signal: Signal<V>,
    /// The file layer: its last good value (`None`: no key, the default).
    file: RefCell<Option<V>>,
    /// What the file (plus queued edits) held at the last read, good or
    /// not changed: a change of it under an overlay value is reported.
    seen: RefCell<Option<V>>,
    /// The runtime overlay layer.
    overlay: RefCell<Option<V>>,
    /// The layers' value as last given to the signal: a signal holding
    /// something else was written (by the UI) and is saved next.
    shown: RefCell<V>,
    /// The signal's latest value (for a write-out without the runtime).
    live: RefCell<V>,
}

impl<V: Clone> Field<V> {
    /// Runtime overlay > file > default.
    fn effective(&self) -> V {
        self.overlay
            .borrow()
            .clone()
            .or_else(|| self.file.borrow().clone())
            .unwrap_or_else(|| self.default.clone())
    }
}

struct Inner<V> {
    store: SettingsStore,
    path: PathBuf,
    display: Arc<str>,
    overlay: PathBuf,
    read_only: Arc<AtomicBool>,
    noticed: Arc<AtomicBool>,
    sink: Arc<FailSink>,
    fields: Vec<Field<V>>,
}

/// A settings file: [`Runtime::settings_file`]. Cheap to clone.
pub struct Settings<V> {
    inner: Rc<Inner<V>>,
}

impl<V> Clone for Settings<V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<V> fmt::Debug for Settings<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("path", &self.inner.path)
            .field(
                "fields",
                &self
                    .inner
                    .fields
                    .iter()
                    .map(|f| &f.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl<V: Clone + PartialEq + 'static> Inner<V> {
    fn field(&self, name: &str) -> Result<&Field<V>, Error> {
        self.fields
            .iter()
            .find(|f| &*f.name == name)
            .ok_or_else(|| Error::failed(format!("{}: no settings field `{name}`", self.display)))
    }

    fn notice(&self, rt: &Runtime, field: Option<&Arc<str>>, issue: SettingsIssue) {
        rt.diagnose(Diagnostic::Settings(SettingsNotice {
            file: self.display.clone(),
            field: field.cloned(),
            issue,
        }));
    }

    fn job(&self, edits: Vec<Edit>, layer: Layer) -> SettingsJob {
        SettingsJob {
            edits,
            layer,
            file: self.display.clone(),
            sink: self.sink.clone(),
        }
    }

    fn file_layer(&self) -> Layer {
        Layer::File {
            overlay: self.overlay.clone(),
            read_only: self.read_only.clone(),
            noticed: self.noticed.clone(),
        }
    }

    /// Read the file and the overlay (with the edits still queued for
    /// them) into the layers. `boot`: no last good values yet, and nothing
    /// to compare a change against.
    fn read_layers(&self, rt: &Runtime, boot: bool) {
        let file = read_doc(&self.path).map(|mut d| {
            apply_edits(&mut d, &self.store.io.queued_settings(&self.path));
            d
        });
        let overlay = read_doc(&self.overlay).map(|mut d| {
            apply_edits(&mut d, &self.store.io.queued_settings(&self.overlay));
            d
        });
        if let Err(issue) = &file {
            self.notice(rt, None, issue.clone());
        }
        if let Err(issue) = &overlay {
            rt.diagnose(Diagnostic::Settings(SettingsNotice {
                file: Arc::from(self.overlay.to_string_lossy()),
                field: None,
                issue: issue.clone(),
            }));
        }
        for f in &self.fields {
            if let Ok(doc) = &file {
                let read = match doc.get(&f.name) {
                    // A deleted key springs back to its default.
                    None => Some(None),
                    Some(item) => match (f.decode)(item) {
                        Ok(v) => Some(Some(v)),
                        Err(m) => {
                            self.notice(rt, Some(&f.name), SettingsIssue::BadValue(Arc::from(m)));
                            None
                        }
                    },
                };
                if let Some(v) = read {
                    let changed = *f.seen.borrow() != v;
                    if changed && !boot && f.overlay.borrow().is_some() {
                        self.notice(rt, Some(&f.name), SettingsIssue::Shadowed);
                    }
                    f.seen.replace(v.clone());
                    f.file.replace(v);
                }
            }
            if let Ok(doc) = &overlay {
                let v = match doc.get(&f.name) {
                    None => None,
                    Some(item) => match (f.decode)(item) {
                        Ok(v) => Some(v),
                        Err(m) => {
                            rt.diagnose(Diagnostic::Settings(SettingsNotice {
                                file: Arc::from(self.overlay.to_string_lossy()),
                                field: Some(f.name.clone()),
                                issue: SettingsIssue::BadValue(Arc::from(m)),
                            }));
                            None
                        }
                    },
                };
                f.overlay.replace(v);
            }
        }
    }

    /// Give the signal its layers' value, unless the user wrote it since
    /// (that write is saved next).
    fn show(&self, rt: &Runtime, f: &Field<V>, force: bool) {
        let eff = f.effective();
        let before = f.shown.replace(eff.clone());
        let Ok(cur) = f.signal.get_untracked(rt) else {
            return;
        };
        if cur == eff || !force && cur != before {
            return;
        }
        if rt.check_write_allowed(f.signal.id()).is_ok() {
            rt.note_write(f.signal.id());
            let _ = f.signal.set_raw(rt, eff);
        }
    }

    /// Save what the user wrote since the last write-out: to the file, or
    /// to the overlay when the file is read-only or the field has an
    /// overlay value.
    fn write_out(&self, rt: Option<&Runtime>) {
        let read_only = self.read_only.load(Ordering::Acquire);
        let mut to_file = Vec::new();
        let mut to_overlay = Vec::new();
        let mut redirected = false;
        for f in &self.fields {
            let v = rt
                .and_then(|rt| f.signal.get_untracked(rt).ok())
                .unwrap_or_else(|| f.live.borrow().clone());
            if v == *f.shown.borrow() {
                continue;
            }
            let item = (f.encode)(&v);
            let has_overlay = f.overlay.borrow().is_some();
            if read_only || has_overlay {
                redirected |= !has_overlay;
                f.overlay.replace(Some(v.clone()));
                to_overlay.push((f.name.clone(), Some(item)));
            } else {
                f.file.replace(Some(v.clone()));
                to_file.push((f.name.clone(), Some(item)));
            }
            f.live.replace(v.clone());
            f.shown.replace(v);
        }
        if !to_file.is_empty() {
            let job = self.job(to_file, self.file_layer());
            self.store.io.enqueue_settings(self.path.clone(), job);
        }
        if !to_overlay.is_empty() {
            let job = self.job(to_overlay, Layer::Overlay);
            self.store.io.enqueue_settings(self.overlay.clone(), job);
        }
        if redirected && !self.noticed.swap(true, Ordering::AcqRel) {
            self.sink.push(Diagnostic::Settings(SettingsNotice {
                file: self.display.clone(),
                field: None,
                issue: SettingsIssue::ReadOnly {
                    overlay: self.overlay.clone(),
                },
            }));
        }
    }
}

impl<V> Drop for Inner<V> {
    /// A runtime dropped without `shutdown` (the cleanup that writes out
    /// never ran) still saves what the user wrote.
    fn drop(&mut self) {
        drop_write_out(self);
    }
}

/// [`Inner::write_out`] for `Drop`, from the last tracked values (no
/// runtime to read the signals, no `V: PartialEq` bound: compares
/// encodings).
fn drop_write_out<V>(inner: &Inner<V>) {
    let read_only = inner.read_only.load(Ordering::Acquire);
    let mut to_file = Vec::new();
    let mut to_overlay = Vec::new();
    for f in &inner.fields {
        let item = (f.encode)(&f.live.borrow());
        if item.to_string() == (f.encode)(&f.shown.borrow()).to_string() {
            continue;
        }
        if read_only || f.overlay.borrow().is_some() {
            to_overlay.push((f.name.clone(), Some(item)));
        } else {
            to_file.push((f.name.clone(), Some(item)));
        }
    }
    let job = |edits, layer| SettingsJob {
        edits,
        layer,
        file: inner.display.clone(),
        sink: inner.sink.clone(),
    };
    if !to_file.is_empty() {
        let layer = Layer::File {
            overlay: inner.overlay.clone(),
            read_only: inner.read_only.clone(),
            noticed: inner.noticed.clone(),
        };
        inner
            .store
            .io
            .enqueue_settings(inner.path.clone(), job(to_file, layer));
    }
    if !to_overlay.is_empty() {
        inner
            .store
            .io
            .enqueue_settings(inner.overlay.clone(), job(to_overlay, Layer::Overlay));
    }
}

impl<V: Clone + PartialEq + 'static> Settings<V> {
    /// The settings file as declared.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Where its overlay is kept.
    pub fn overlay_path(&self) -> &Path {
        &self.inner.overlay
    }

    /// Writes go to the overlay because the file is read-only.
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only.load(Ordering::Acquire)
    }

    /// The field's signal (`prefs.compact`): read it like any state, write
    /// it for a UI write or `strand set prefs.compact true` (saved after
    /// [`PERSIST_DEBOUNCE`] of quiet).
    pub fn signal(&self, field: &str) -> Option<Signal<V>> {
        self.inner.field(field).ok().map(|f| f.signal)
    }

    /// Every field's name and signal, in declaration order.
    pub fn signals(&self) -> Vec<(Arc<str>, Signal<V>)> {
        self.inner
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.signal))
            .collect()
    }

    /// A UI write or `strand set`: [`Signal::set`] on the field.
    pub fn set(&self, rt: &Runtime, field: &str, value: V) -> Result<(), Error> {
        self.inner.field(field)?.signal.set(rt, value)
    }

    /// The field's runtime overlay value, if any (the inspector's
    /// provenance).
    pub fn overlay(&self, field: &str) -> Option<V> {
        self.inner
            .field(field)
            .ok()
            .and_then(|f| f.overlay.borrow().clone())
    }

    /// The file changed (the watcher): re-read it and the overlay. Each
    /// field is checked on its own; a syntax error keeps every last good
    /// value; a deleted key springs back to its default; a file change to
    /// a field the overlay shadows is reported.
    pub fn reload(&self, rt: &Runtime) {
        self.inner.read_layers(rt, false);
        for f in &self.inner.fields {
            self.inner.show(rt, f, false);
        }
    }

    /// Set a runtime overlay value: it wins over the file until cleared,
    /// and is kept across restarts.
    pub fn set_overlay(&self, rt: &Runtime, field: &str, value: V) -> Result<(), Error> {
        let f = self.inner.field(field)?;
        let item = (f.encode)(&value);
        f.overlay.replace(Some(value));
        let job = self
            .inner
            .job(vec![(f.name.clone(), Some(item))], Layer::Overlay);
        self.inner
            .store
            .io
            .enqueue_settings(self.inner.overlay.clone(), job);
        self.inner.show(rt, f, true);
        Ok(())
    }

    /// The overlay's `[clear]`: drop the field's overlay value, so the
    /// file (or the default) applies again.
    pub fn clear_overlay(&self, rt: &Runtime, field: &str) -> Result<(), Error> {
        let f = self.inner.field(field)?;
        if f.overlay.replace(None).is_none() {
            return Ok(());
        }
        let job = self.inner.job(vec![(f.name.clone(), None)], Layer::Overlay);
        self.inner
            .store
            .io
            .enqueue_settings(self.inner.overlay.clone(), job);
        self.inner.show(rt, f, true);
        Ok(())
    }

    /// Queue what the user wrote now instead of after the debounce.
    pub fn write_out(&self, rt: &Runtime) {
        self.inner.write_out(Some(rt));
    }
}

impl Runtime {
    /// `state prefs from "prefs.toml" { … }`: a two-way settings file at
    /// `path` (the compiler resolves it against the config directory) with
    /// the typed `fields` of its schema. Returns the handle; each field is
    /// an ordinary [`Signal`] ([`Settings::signal`]) owned by the current
    /// owner. See the module docs for the rules. Reports from reading come
    /// as [`Diagnostic::Settings`] now, from writing in a later tick.
    pub fn settings_file<V>(
        &self,
        store: &SettingsStore,
        path: impl AsRef<Path>,
        fields: Vec<FieldSpec<V>>,
    ) -> Settings<V>
    where
        V: Clone + PartialEq + 'static,
    {
        let path = path.as_ref().to_path_buf();
        let display: Arc<str> = Arc::from(path.to_string_lossy());
        let fields = fields
            .into_iter()
            .map(|s| {
                let signal = self.signal(s.default.clone());
                self.set_name(signal.id(), format!("{display}:{}", s.name));
                Field {
                    signal,
                    file: RefCell::new(None),
                    seen: RefCell::new(None),
                    overlay: RefCell::new(None),
                    shown: RefCell::new(s.default.clone()),
                    live: RefCell::new(s.default.clone()),
                    name: s.name,
                    default: s.default,
                    decode: s.decode,
                    encode: s.encode,
                }
            })
            .collect();
        let inner = Rc::new(Inner {
            overlay: store.overlay_of(&path),
            read_only: Arc::new(AtomicBool::new(probe_read_only(&resolve_links(&path)))),
            noticed: Arc::new(AtomicBool::new(false)),
            sink: self.inner.persist_failures.clone(),
            store: store.clone(),
            path,
            display,
            fields,
        });
        {
            let mut stores = self.inner.persist_stores.borrow_mut();
            if !stores.iter().any(|s| s.same(&store.io)) {
                stores.push(store.io.clone());
            }
        }
        inner.read_layers(self, true);
        for f in &inner.fields {
            let v = f.effective();
            f.shown.replace(v.clone());
            f.live.replace(v.clone());
            // Its starting value, not a write: nothing observes it yet.
            f.signal.init_value(self, v);
        }
        // The tracking effect keeps the handle alive with the field cells.
        let tracked = inner.clone();
        let saver: Weak<Inner<V>> = Rc::downgrade(&inner);
        self.on_change_after(
            move |rt| {
                let inner = &tracked;
                let mut values = Vec::with_capacity(inner.fields.len());
                for f in &inner.fields {
                    let v = f.signal.get(rt)?;
                    f.live.replace(v.clone());
                    values.push(v);
                }
                Ok(values)
            },
            PERSIST_DEBOUNCE,
            move |rt| {
                if let Some(inner) = saver.upgrade() {
                    inner.write_out(Some(rt));
                }
                Ok(())
            },
        );
        let flusher = Rc::downgrade(&inner);
        let rt = self.downgrade();
        self.on_cleanup(move || {
            if let Some(inner) = flusher.upgrade() {
                let rt = rt.upgrade();
                inner.write_out(rt.as_ref());
            }
        });
        Settings { inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(doc: &str, edits: &[Edit]) -> String {
        let mut d: DocumentMut = doc.parse().unwrap();
        apply_edits(&mut d, edits);
        d.to_string()
    }

    #[test]
    fn edits_keep_comments_spacing_and_order() {
        let doc =
            "# My settings\naccent   =  \"#7aa2f7\"   # blue\n\n# denser bar\ncompact = false\n";
        let out = edit(
            doc,
            &[
                (Arc::from("compact"), Some(toml_edit::value(true))),
                (Arc::from("accent"), Some(toml_edit::value("#ff0000"))),
                (Arc::from("gap"), Some(toml_edit::value(4))),
            ],
        );
        assert_eq!(
            out,
            "# My settings\naccent   =  \"#ff0000\"   # blue\n\n# denser bar\ncompact = true\ngap = 4\n"
        );
        assert_eq!(
            edit(&out, &[(Arc::from("gap"), None)]),
            "# My settings\naccent   =  \"#ff0000\"   # blue\n\n# denser bar\ncompact = true\n"
        );
    }

    #[test]
    fn a_later_edit_of_a_field_replaces_a_queued_one() {
        let sink = Arc::new(FailSink::new(Arc::new(crate::task::ReadyQueue::default())));
        let job = |edits: Vec<Edit>| SettingsJob {
            edits,
            layer: Layer::Overlay,
            file: Arc::from("x"),
            sink: sink.clone(),
        };
        let mut a = job(vec![
            (Arc::from("a"), Some(toml_edit::value(1))),
            (Arc::from("b"), Some(toml_edit::value(1))),
        ]);
        a.merge(job(vec![(Arc::from("a"), None)]));
        let names: Vec<_> = a
            .edits
            .iter()
            .map(|(n, i)| (n.to_string(), i.is_some()))
            .collect();
        assert_eq!(
            names,
            vec![("b".to_string(), true), ("a".to_string(), false)]
        );
    }
}

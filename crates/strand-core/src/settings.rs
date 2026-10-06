//! Settings files: `state prefs from "prefs.toml" { accent: color =
//! #7aa2f7; compact: bool = false }`, typed two-way settings that
//! non-programmers can edit ("Settings files" in the design).
//!
//! The compiler supplies the schema: one [`FieldSpec`] per field, with its
//! name, its type name, its default and the codec between the VM's value
//! type `V` and a TOML item. [`Runtime::settings_file`] reads the file and
//! returns one [`Signal`] per field plus a [`Settings`] handle. The rules:
//!
//! * **Each field is checked on its own.** A value that does not decode
//!   keeps that field's last good value and reports
//!   [`SettingsIssue::BadValue`]; the other fields still apply.
//! * **A TOML syntax error keeps every last good value**
//!   ([`SettingsIssue::Syntax`]). **A deleted key springs back to its
//!   default.**
//! * **Last good values survive a restart.** Every value read from the
//!   file (or written to it) is also kept in a last-good snapshot,
//!   `$XDG_STATE_HOME/strand/settings/last-good/<declared path>.toml`, so a
//!   file that is broken at boot still starts from its last good values,
//!   not the declared defaults.
//! * **Writes go through `toml_edit`.** A UI write (or `strand set
//!   prefs.compact true`) is an ordinary write to the field's signal; after
//!   [`PERSIST_DEBOUNCE`] of quiet it is applied to what the file holds,
//!   field by field, keeping comments, spacing and order. The edits run on
//!   the persist IO thread (never on the logic tick), merged per file.
//! * **Writes follow symlinks** and replace the target via a temp file
//!   (`.<name>.tmp.<pid>.<n>`, which strand-watch ignores) in the target's
//!   directory plus rename; a missing directory is created. **A read-only
//!   target** (`/nix/store`: no write permission, `EACCES`, `EROFS`) gets
//!   an overlay in `$XDG_STATE_HOME/strand/settings/` instead, with a
//!   [`SettingsIssue::ReadOnly`] notice (once per file while it stays
//!   read-only; every reload probes again, so a link swapped to a writable
//!   file takes writes again).
//! * **Who wins:** runtime overlay > file > default ([`Settings::layer`]).
//!   The overlay holds redirected writes and explicit
//!   [`Settings::set_overlay`] values, and survives restarts. A UI write to
//!   a field that has an overlay value updates the overlay (writing the
//!   file would be shadowed). When the file changes under a field the
//!   overlay shadows, the reload reports [`SettingsIssue::Shadowed`]:
//!   `accent: file changed but runtime overlay wins [clear]`
//!   ([`Settings::clear_overlay`] is `[clear]`). The overlay is Strand's
//!   own file: one with a syntax error is moved aside to
//!   `.<name>.corrupt` ([`SettingsIssue::CorruptOverlay`]).
//! * **One file, several handles** (a component declaring the file mounted
//!   once per monitor): each has its own signals, and a write through one
//!   is adopted by the others in the same tick, without a reload.
//!
//! The logic thread calls [`Settings::reload`] when the watcher reports a
//! changed hash (the watcher never parses; Strand's own writes reach it as
//! pre-registered hashes through [`SettingsStore::on_written`]). A thread
//! allowed to parse can instead read with [`SettingsSources`] and hand the
//! result to [`Settings::reload_with`], so the logic thread only decodes. A reload
//! never undoes a write of Strand's own that may not be in what it read
//! (still queued, in flight, or written after the read began), and leaves a
//! field the user wrote since the last write-out alone (that write is
//! saved next). Live reload of the declaration is [`Settings::redeclare`].

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub use toml_edit;
use toml_edit::{DocumentMut, Item};

use crate::error::Error;
use crate::persist::{
    FailSink, Observers, PERSIST_DEBOUNCE, PersistError, PersistStore, create_private_dir,
    escape_name, io_error, quarantine, quarantine_path, sweep_temps, temp_next_to,
};
use crate::runtime::{Diagnostic, NodeId, Runtime};
use crate::signal::Signal;

/// One field edit: set to an item, or remove the key (`None`).
pub(crate) type Edit = (Arc<str>, Option<Item>);

/// Sequence numbers of settings jobs (process-wide, so jobs of every
/// handle on one file are ordered).
static JOB_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_seq() -> u64 {
    JOB_SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

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

    /// [`PersistStore::on_written`] for the IO thread this store writes
    /// through. It is the same single observer slot as that
    /// `PersistStore`'s (the one this store was made from with
    /// [`PersistStore::settings`] or [`SettingsStore::sharing`]): it sees
    /// every file that IO thread writes or removes (settings files,
    /// overlays, last-good snapshots *and* persisted cells), and setting it
    /// here replaces an observer set on the `PersistStore`, and the
    /// reverse. Register one observer per IO thread.
    pub fn on_written(&self, f: impl Fn(&crate::persist::OwnWrite<'_>) + Send + Sync + 'static) {
        self.io.on_written(f);
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

    /// The last-good snapshot of the settings file declared at `file`: the
    /// values a broken file falls back to at boot.
    pub fn last_good_of(&self, file: &Path) -> PathBuf {
        let name = escape_name(&file.to_string_lossy());
        self.dir.join("last-good").join(format!("{name}.toml"))
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
    ty: Arc<str>,
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
            ty: Arc::from(""),
            default,
            decode: Box::new(decode),
            encode: Box::new(encode),
        }
    }

    /// The field's declared type, as written (`color`, `bool`): a
    /// [`Settings::redeclare`] that changes it resets the field.
    #[must_use]
    pub fn with_type(mut self, ty: impl Into<Arc<str>>) -> Self {
        self.ty = ty.into();
        self
    }
}

impl<V: fmt::Debug> fmt::Debug for FieldSpec<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldSpec")
            .field("name", &self.name)
            .field("ty", &self.ty)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

/// A settings report for the overlay and `strand watch`
/// ([`Diagnostic::Settings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsNotice {
    /// The settings file as declared (the overlay file for overlay
    /// issues).
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
    /// The overlay is not valid TOML: it was moved aside to `moved_to` and
    /// the overlay starts empty.
    CorruptOverlay {
        /// Where the broken overlay was moved (`.<name>.corrupt`).
        moved_to: PathBuf,
        /// The parse error.
        error: Arc<str>,
    },
    /// Live reload changed the field's declared type: it was reset (read
    /// again under the new type, or its default).
    TypeChanged {
        /// The old type.
        from: Arc<str>,
        /// The new type.
        to: Arc<str>,
    },
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
            SettingsIssue::CorruptOverlay { moved_to, error } => write!(
                f,
                "{}: not valid TOML ({error}); moved aside to {}, starting empty",
                self.file,
                moved_to.display()
            ),
            SettingsIssue::TypeChanged { from, to } => write!(
                f,
                "{}: {field}: type changed from {from} to {to}; reset",
                self.file
            ),
        }
    }
}

/// Which layer a field's value comes from ([`Settings::layer`], the
/// inspector's provenance).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The runtime overlay (a redirected write, or [`Settings::set_overlay`]).
    Overlay,
    /// The settings file (or its last good value).
    File,
    /// The declared default.
    Default,
}

/// Settings edits queued for the IO thread.
#[derive(Clone)]
pub(crate) struct SettingsJob {
    pub(crate) edits: Vec<Edit>,
    /// Its sequence number (the highest of the jobs merged into it).
    pub(crate) seq: u64,
    target: Target,
    file: Arc<str>,
    sink: Arc<FailSink>,
    /// A corrupt overlay this moves aside was already reported (by a read).
    corrupt_noticed: bool,
}

#[derive(Clone)]
enum Target {
    /// The settings file; falls back to `overlay` when it is read-only.
    File {
        overlay: PathBuf,
        read_only: Arc<AtomicBool>,
        noticed: Arc<AtomicBool>,
    },
    /// The overlay file.
    Overlay,
    /// The last-good snapshot (Strand's cache: failures are silent).
    Snapshot,
}

impl SettingsJob {
    /// Take `later`'s edits too (a later edit of a field replaces an
    /// earlier one).
    pub(crate) fn merge(&mut self, later: SettingsJob) {
        for (name, item) in later.edits {
            self.edits.retain(|(n, _)| *n != name);
            self.edits.push((name, item));
        }
        self.seq = self.seq.max(later.seq);
        self.corrupt_noticed |= later.corrupt_noticed;
    }

    fn report(&self, issue: SettingsIssue) {
        self.sink.push(Diagnostic::Settings(SettingsNotice {
            file: self.file.clone(),
            field: None,
            issue,
        }));
    }

    /// Report how an overlay edit went.
    fn overlay_done(&self, overlay: &Path, r: Result<Option<Quarantined>, EditError>) {
        match r {
            Ok(Some(q)) if !self.corrupt_noticed => {
                self.sink.push(Diagnostic::Settings(SettingsNotice {
                    file: Arc::from(overlay.to_string_lossy()),
                    field: None,
                    issue: SettingsIssue::CorruptOverlay {
                        moved_to: q.moved_to,
                        error: Arc::from(q.error),
                    },
                }));
            }
            Ok(_) => {}
            Err(e) => self.report(SettingsIssue::WriteFailed(e.message())),
        }
    }
}

/// Run a settings job on the IO thread; it reports its own outcome.
pub(crate) fn perform(key: &Path, job: &SettingsJob, observe: &Observers<'_>) {
    let edit_toml = |path: &Path, edits: &[Edit], mode| edit_toml(path, edits, mode, observe);
    match &job.target {
        Target::Snapshot => {
            let _ = edit_toml(key, &job.edits, Mode::Snapshot);
        }
        Target::Overlay => job.overlay_done(key, edit_toml(key, &job.edits, Mode::Overlay)),
        Target::File {
            overlay,
            read_only,
            noticed,
        } => match edit_toml(key, &job.edits, Mode::File) {
            Ok(_) => {}
            Err(EditError::ReadOnly) => {
                read_only.store(true, Ordering::Release);
                let r = edit_toml(overlay, &job.edits, Mode::Overlay);
                let saved = r.is_ok();
                job.overlay_done(overlay, r);
                if saved && !noticed.swap(true, Ordering::AcqRel) {
                    job.report(SettingsIssue::ReadOnly {
                        overlay: overlay.clone(),
                    });
                }
            }
            Err(e) => job.report(SettingsIssue::WriteFailed(e.message())),
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The user's file: never overwritten when it has a syntax error; a
    /// missing directory is created (with the user's umask).
    File,
    /// Strand's overlay: one with a syntax error is moved aside.
    Overlay,
    /// Strand's last-good snapshot: one with a syntax error is replaced.
    Snapshot,
}

/// A corrupt overlay moved aside by [`edit_toml`].
struct Quarantined {
    moved_to: PathBuf,
    error: String,
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

fn classify(path: &Path, e: &io::Error) -> EditError {
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
                Some(old) if old.is_value() => {
                    let decor = old.as_value().map(|v| v.decor().clone());
                    match (decor, new.clone()) {
                        (Some(decor), Item::Value(mut v)) => {
                            *v.decor_mut() = decor;
                            *old = Item::Value(v);
                        }
                        (_, new) => *old = new,
                    }
                }
                // A table (or array of tables) the user wrote under that
                // name: replaced as a new key, with the key's default
                // spacing (its own decor belongs to a `[header]`).
                Some(_) => {
                    doc.remove(name);
                    doc.insert(name, new.clone());
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
/// target's permissions), `fsync`, rename, directory `fsync`. Strand's own
/// files (overlay, snapshot) live in a private directory and are removed
/// once they hold nothing.
fn edit_toml(
    path: &Path,
    edits: &[Edit],
    mode: Mode,
    observe: &Observers<'_>,
) -> Result<Option<Quarantined>, EditError> {
    let target = resolve_links(path);
    if mode == Mode::File && probe_read_only(&target) {
        return Err(EditError::ReadOnly);
    }
    let text = match fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(classify(&target, &e)),
    };
    let mut quarantined = None;
    let mut doc: DocumentMut = match text.parse() {
        Ok(d) => d,
        Err(e) => {
            let e: toml_edit::TomlError = e;
            match mode {
                Mode::File => return Err(EditError::Syntax(e.to_string())),
                Mode::Overlay => {
                    observe.report(path, &target, None);
                    quarantine(&target);
                    quarantined = Some(Quarantined {
                        moved_to: quarantine_path(&target),
                        error: e.to_string().trim_end().to_string(),
                    });
                    DocumentMut::new()
                }
                Mode::Snapshot => DocumentMut::new(),
            }
        }
    };
    apply_edits(&mut doc, edits);
    let dir = target
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if mode == Mode::File {
        fs::create_dir_all(&dir).map_err(|e| classify(&dir, &e))?;
    } else {
        create_private_dir(&dir).map_err(EditError::Io)?;
        if doc.is_empty() {
            // Not `exists()`, which follows a link.
            if fs::symlink_metadata(&target).is_ok() {
                observe.report(path, &target, None);
            }
            return match fs::remove_file(&target) {
                Ok(()) => Ok(quarantined),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(quarantined),
                Err(e) => Err(classify(&target, &e)),
            };
        }
    }
    let temp = temp_next_to(&dir, &target);
    let content = doc.to_string();
    let written = (|| {
        let mut f = fs::File::create(&temp)?;
        if let Ok(m) = fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        observe.report(path, &target, Some(content.as_bytes()));
        fs::rename(&temp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(classify(&target, &e));
    }
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(quarantined)
}

/// Parse a TOML file's text; a missing file is empty.
fn parse_doc(text: io::Result<String>) -> Result<DocumentMut, SettingsIssue> {
    match text {
        Ok(t) => t
            .parse::<DocumentMut>()
            .map_err(|e| SettingsIssue::Syntax(Arc::from(e.to_string().trim_end()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(SettingsIssue::Unreadable(Arc::from(e.to_string()))),
    }
}

/// What a reload reads, from any thread: [`Settings::sources`]. Cheap to
/// clone; `Send`.
#[derive(Clone)]
pub struct SettingsSources {
    path: PathBuf,
    overlay: PathBuf,
    io: PersistStore,
}

impl fmt::Debug for SettingsSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsSources")
            .field("path", &self.path)
            .field("overlay", &self.overlay)
            .finish()
    }
}

/// Strand's own writes as they stood before a read: [`SettingsSources::mark`].
pub struct ReadMark {
    file: (u64, Vec<Edit>),
    overlay: (u64, Vec<Edit>),
}

impl fmt::Debug for ReadMark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadMark")
            .field("file_landed", &self.file.0)
            .field("overlay_landed", &self.overlay.0)
            .finish_non_exhaustive()
    }
}

/// A settings file and its overlay as read (and parsed) by
/// [`SettingsSources::read`]: hand it to [`Settings::reload_with`]
/// (cloned for each handle on one file).
#[derive(Clone)]
pub struct SettingsRead {
    file: Result<DocumentMut, SettingsIssue>,
    overlay: Result<DocumentMut, SettingsIssue>,
    /// The overlay had a syntax error (it reads as empty).
    overlay_corrupt: Option<Arc<str>>,
    read_only: bool,
    file_landed: u64,
    overlay_landed: u64,
}

impl fmt::Debug for SettingsRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsRead")
            .field("file_ok", &self.file.is_ok())
            .field("overlay_ok", &self.overlay.is_ok())
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

impl SettingsSources {
    /// The settings file as declared.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Note Strand's own writes to the file and overlay (done, queued or
    /// in flight). Take it *before* reading the file's bytes.
    pub fn mark(&self) -> ReadMark {
        ReadMark {
            file: self.io.settings_mark(&self.path),
            overlay: self.io.settings_mark(&self.overlay),
        }
    }

    /// Read and parse the file and its overlay, and probe whether the
    /// file is writable: blocking IO, for the watcher's thread.
    pub fn read(&self) -> SettingsRead {
        let mark = self.mark();
        let text = fs::read_to_string(&self.path);
        self.read_from(mark, text)
    }

    /// [`SettingsSources::read`] with the file's text already read (the
    /// watcher read it to hash it) *after* `mark` was taken. Reads the
    /// overlay and probes the file.
    pub fn read_from(&self, mark: ReadMark, file: io::Result<String>) -> SettingsRead {
        let (file_landed, file_edits) = mark.file;
        let (overlay_landed, overlay_edits) = mark.overlay;
        let file = parse_doc(file).map(|mut d| {
            apply_edits(&mut d, &file_edits);
            d
        });
        let mut overlay_corrupt = None;
        let overlay = match parse_doc(fs::read_to_string(&self.overlay)) {
            Err(SettingsIssue::Syntax(m)) => {
                overlay_corrupt = Some(m);
                Ok(DocumentMut::new())
            }
            r => r,
        }
        .map(|mut d| {
            apply_edits(&mut d, &overlay_edits);
            d
        });
        SettingsRead {
            file,
            overlay,
            overlay_corrupt,
            read_only: probe_read_only(&resolve_links(&self.path)),
            file_landed,
            overlay_landed,
        }
    }
}

/// The parts of a field that [`Settings::redeclare`] replaces.
struct Spec<V> {
    ty: Arc<str>,
    default: V,
    decode: Decode<V>,
    encode: Encode<V>,
}

struct Field<V> {
    name: Arc<str>,
    spec: RefCell<Spec<V>>,
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
    /// What the last-good snapshot holds (or is about to).
    snapped: RefCell<Option<V>>,
    /// The sequence number of the last edit of this field sent to the
    /// file (and to the overlay): a read that may predate it leaves the
    /// layer alone.
    file_seq: Cell<u64>,
    overlay_seq: Cell<u64>,
    /// Not read yet (boot, or added or reset by a redeclare): no last good
    /// value but the snapshot's, nothing to compare a change against.
    fresh: Cell<bool>,
}

impl<V: Clone> Field<V> {
    /// Runtime overlay > file > default.
    fn effective(&self) -> V {
        self.overlay
            .borrow()
            .clone()
            .or_else(|| self.file.borrow().clone())
            .unwrap_or_else(|| self.spec.borrow().default.clone())
    }

    fn layer(&self) -> Layer {
        if self.overlay.borrow().is_some() {
            Layer::Overlay
        } else if self.file.borrow().is_some() {
            Layer::File
        } else {
            Layer::Default
        }
    }

    fn decode(&self, item: &Item) -> Result<V, String> {
        (self.spec.borrow().decode)(item)
    }

    fn encode(&self, v: &V) -> Item {
        (self.spec.borrow().encode)(v)
    }
}

/// A live handle in the runtime's registry ([`Runtime::settings_file`]).
pub(crate) struct Registered {
    overlay: PathBuf,
    handle: Weak<dyn Sibling>,
}

/// Another handle on the same settings file.
trait Sibling {
    /// A sibling queued `edits` (sequence number `seq`) for the file, or
    /// the overlay: take them as that layer's values.
    fn adopt(&self, rt: &Runtime, overlay: bool, edits: &[Edit], seq: u64);
}

struct Inner<V> {
    store: SettingsStore,
    path: PathBuf,
    display: Arc<str>,
    overlay: PathBuf,
    snapshot: PathBuf,
    /// The owner of the field cells (fields a redeclare adds join it).
    owner: Option<NodeId>,
    read_only: Arc<AtomicBool>,
    /// The last probe said read-only.
    probed: Cell<bool>,
    noticed: Arc<AtomicBool>,
    sink: Arc<FailSink>,
    /// Bumped by a redeclare so the saver tracks the new field list.
    schema: Signal<u64>,
    fields: RefCell<Vec<Rc<Field<V>>>>,
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
                    .borrow()
                    .iter()
                    .map(|f| f.name.clone())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl<V: Clone + PartialEq + 'static> Inner<V> {
    fn fields(&self) -> Vec<Rc<Field<V>>> {
        self.fields.borrow().clone()
    }

    fn find(&self, name: &str) -> Option<Rc<Field<V>>> {
        self.fields
            .borrow()
            .iter()
            .find(|f| &*f.name == name)
            .cloned()
    }

    fn field(&self, name: &str) -> Result<Rc<Field<V>>, Error> {
        self.find(name)
            .ok_or_else(|| Error::failed(format!("{}: no settings field `{name}`", self.display)))
    }

    fn notice(&self, rt: &Runtime, field: Option<&Arc<str>>, issue: SettingsIssue) {
        rt.diagnose(Diagnostic::Settings(SettingsNotice {
            file: self.display.clone(),
            field: field.cloned(),
            issue,
        }));
    }

    fn overlay_notice(&self, rt: &Runtime, field: Option<&Arc<str>>, issue: SettingsIssue) {
        rt.diagnose(Diagnostic::Settings(SettingsNotice {
            file: Arc::from(self.overlay.to_string_lossy()),
            field: field.cloned(),
            issue,
        }));
    }

    fn sources(&self) -> SettingsSources {
        SettingsSources {
            path: self.path.clone(),
            overlay: self.overlay.clone(),
            io: self.store.io.clone(),
        }
    }

    fn file_target(&self) -> Target {
        Target::File {
            overlay: self.overlay.clone(),
            read_only: self.read_only.clone(),
            noticed: self.noticed.clone(),
        }
    }

    /// Queue `edits` for `key`; returns the job's sequence number.
    fn enqueue(&self, key: &Path, edits: Vec<Edit>, target: Target, corrupt_noticed: bool) -> u64 {
        let seq = next_seq();
        let job = SettingsJob {
            edits,
            seq,
            target,
            file: self.display.clone(),
            sink: self.sink.clone(),
            corrupt_noticed,
        };
        self.store.io.enqueue_settings(key.to_path_buf(), job);
        seq
    }

    /// The last-good snapshot as it is about to stand.
    fn read_snapshot(&self) -> DocumentMut {
        let (_, queued) = self.store.io.settings_mark(&self.snapshot);
        let mut doc = parse_doc(fs::read_to_string(&self.snapshot)).unwrap_or_default();
        apply_edits(&mut doc, &queued);
        doc
    }

    /// Bring the last-good snapshot up to the file layer.
    fn sync_snapshot(&self) {
        let mut edits = Vec::new();
        for f in self.fields() {
            let file = f.file.borrow().clone();
            if *f.snapped.borrow() != file {
                edits.push((f.name.clone(), file.as_ref().map(|v| f.encode(v))));
                f.snapped.replace(file);
            }
        }
        if !edits.is_empty() {
            self.enqueue(&self.snapshot, edits, Target::Snapshot, false);
        }
    }

    /// Take what was read into the layers. `only_fresh`: just the fields
    /// not read yet (a redeclare added or reset them).
    fn apply(&self, rt: &Runtime, read: SettingsRead, only_fresh: bool) {
        // Probe again on every read: a link swapped from /nix/store to a
        // writable file takes writes again (and a later swap back gives
        // its notice again). A file the IO thread found read-only while
        // the probe still says writable stays redirected.
        let was = self.probed.replace(read.read_only);
        if read.read_only {
            self.read_only.store(true, Ordering::Release);
        } else if was {
            self.read_only.store(false, Ordering::Release);
            self.noticed.store(false, Ordering::Release);
        }
        if !only_fresh {
            if let Err(issue) = &read.file {
                self.notice(rt, None, issue.clone());
            }
            if let Err(issue) = &read.overlay {
                self.overlay_notice(rt, None, issue.clone());
            }
            if let Some(error) = &read.overlay_corrupt {
                // Strand's own file: move it aside (on the IO thread) and
                // go on from an empty overlay.
                self.overlay_notice(
                    rt,
                    None,
                    SettingsIssue::CorruptOverlay {
                        moved_to: quarantine_path(&self.overlay),
                        error: error.clone(),
                    },
                );
                self.enqueue(&self.overlay, Vec::new(), Target::Overlay, true);
            }
        }
        let mut snapshot = None;
        for f in self.fields() {
            let fresh = f.fresh.get();
            if only_fresh && !fresh {
                continue;
            }
            if fresh {
                // The last good value until the file says otherwise.
                let snap = snapshot.get_or_insert_with(|| self.read_snapshot());
                let v = snap.get(&f.name).and_then(|i| f.decode(i).ok());
                f.snapped.replace(v.clone());
                f.seen.replace(v.clone());
                f.file.replace(v);
            }
            // A read that may predate the field's last edit leaves it: the
            // edit is what the file holds (or is about to).
            if f.file_seq.get() <= read.file_landed
                && let Ok(doc) = &read.file
            {
                let read = match doc.get(&f.name) {
                    // A deleted key springs back to its default.
                    None => Some(None),
                    Some(item) => match f.decode(item) {
                        Ok(v) => Some(Some(v)),
                        Err(m) => {
                            self.notice(rt, Some(&f.name), SettingsIssue::BadValue(Arc::from(m)));
                            None
                        }
                    },
                };
                if let Some(v) = read {
                    let changed = *f.seen.borrow() != v;
                    if changed && !fresh && f.overlay.borrow().is_some() {
                        self.notice(rt, Some(&f.name), SettingsIssue::Shadowed);
                    }
                    f.seen.replace(v.clone());
                    f.file.replace(v);
                }
            }
            if f.overlay_seq.get() <= read.overlay_landed
                && let Ok(doc) = &read.overlay
            {
                let v = match doc.get(&f.name) {
                    None => None,
                    Some(item) => match f.decode(item) {
                        Ok(v) => Some(v),
                        Err(m) => {
                            self.overlay_notice(
                                rt,
                                Some(&f.name),
                                SettingsIssue::BadValue(Arc::from(m)),
                            );
                            None
                        }
                    },
                };
                f.overlay.replace(v);
            }
            f.fresh.set(false);
        }
        self.sync_snapshot();
    }

    /// Give the signal its layers' value, unless the user wrote it since
    /// (that write is saved next). `reload`: a live reload's redeclare (an
    /// adopted default, a type reset), a reload write that `on change`
    /// handlers take as their baseline ([`Signal::set_reloaded`]).
    fn show(&self, rt: &Runtime, f: &Field<V>, force: bool, reload: bool) {
        let eff = f.effective();
        let Ok(cur) = f.signal.get_untracked(rt) else {
            return;
        };
        let before = f.shown.borrow().clone();
        if cur == eff || !force && cur != before {
            f.shown.replace(eff);
            return;
        }
        // `shown` moves only once the signal holds the value: a signal
        // left stale is not mistaken for a user write.
        if rt.check_write_allowed(f.signal.id()).is_ok() {
            rt.note_write(f.signal.id());
            let set = if reload {
                f.signal.set_reloaded(rt, eff.clone())
            } else {
                f.signal.set_raw(rt, eff.clone())
            };
            if set.is_ok() {
                f.shown.replace(eff);
            }
        }
    }

    /// The other live handles on this file.
    fn siblings(&self, rt: &Runtime) -> Vec<Rc<dyn Sibling>> {
        let me = std::ptr::from_ref(self).cast::<()>();
        rt.inner
            .settings_files
            .borrow()
            .iter()
            .filter(|r| r.overlay == self.overlay)
            .filter_map(|r| r.handle.upgrade())
            .filter(|h| Rc::as_ptr(h).cast::<()>() != me)
            .collect()
    }

    fn tell_siblings(&self, rt: &Runtime, overlay: bool, edits: &[Edit], seq: u64) {
        for s in self.siblings(rt) {
            s.adopt(rt, overlay, edits, seq);
        }
    }

    /// Save what the user wrote since the last write-out: to the file, or
    /// to the overlay when the file is read-only or the field has an
    /// overlay value.
    fn write_out(&self, rt: Option<&Runtime>) {
        let read_only = self.read_only.load(Ordering::Acquire);
        let mut to_file = Vec::new();
        let mut to_overlay = Vec::new();
        let mut file_fields = Vec::new();
        let mut overlay_fields = Vec::new();
        let mut redirected = false;
        for f in self.fields() {
            let v = rt
                .and_then(|rt| f.signal.get_untracked(rt).ok())
                .unwrap_or_else(|| f.live.borrow().clone());
            if v == *f.shown.borrow() {
                continue;
            }
            let item = f.encode(&v);
            let has_overlay = f.overlay.borrow().is_some();
            if read_only || has_overlay {
                redirected |= !has_overlay;
                f.overlay.replace(Some(v.clone()));
                to_overlay.push((f.name.clone(), Some(item)));
                overlay_fields.push(f.clone());
            } else {
                f.file.replace(Some(v.clone()));
                // What the next read sees: not a file change to report.
                f.seen.replace(Some(v.clone()));
                to_file.push((f.name.clone(), Some(item)));
                file_fields.push(f.clone());
            }
            f.live.replace(v.clone());
            f.shown.replace(v);
        }
        if !to_file.is_empty() {
            let seq = self.enqueue(&self.path, to_file.clone(), self.file_target(), false);
            for f in &file_fields {
                f.file_seq.set(seq);
            }
            self.sync_snapshot();
            if let Some(rt) = rt {
                self.tell_siblings(rt, false, &to_file, seq);
            }
        }
        if !to_overlay.is_empty() {
            let seq = self.enqueue(&self.overlay, to_overlay.clone(), Target::Overlay, false);
            for f in &overlay_fields {
                f.overlay_seq.set(seq);
            }
            if let Some(rt) = rt {
                self.tell_siblings(rt, true, &to_overlay, seq);
            }
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

    /// A field for `spec`, its cell owned by the handle's owner.
    fn new_field(&self, rt: &Runtime, s: FieldSpec<V>) -> Field<V> {
        let make = |rt: &Runtime| rt.signal(s.default.clone());
        let signal = match self.owner {
            Some(o) if rt.current_owner() != Some(o) => {
                rt.with_owner(o, make).unwrap_or_else(|_| make(rt))
            }
            _ => make(rt),
        };
        rt.set_name(signal.id(), format!("{}:{}", self.display, s.name));
        Field {
            signal,
            file: RefCell::new(None),
            seen: RefCell::new(None),
            overlay: RefCell::new(None),
            shown: RefCell::new(s.default.clone()),
            live: RefCell::new(s.default.clone()),
            snapped: RefCell::new(None),
            file_seq: Cell::new(0),
            overlay_seq: Cell::new(0),
            fresh: Cell::new(true),
            name: s.name,
            spec: RefCell::new(Spec {
                ty: s.ty,
                default: s.default,
                decode: s.decode,
                encode: s.encode,
            }),
        }
    }
}

impl<V: Clone + PartialEq + 'static> Sibling for Inner<V> {
    fn adopt(&self, rt: &Runtime, overlay: bool, edits: &[Edit], seq: u64) {
        for (name, item) in edits {
            let Some(f) = self.find(name) else {
                continue;
            };
            let v = match item {
                None => None,
                Some(item) => match f.decode(item) {
                    Ok(v) => Some(v),
                    // Declared with another type here: not ours to take.
                    Err(_) => continue,
                },
            };
            if overlay {
                f.overlay.replace(v);
                f.overlay_seq.set(seq);
            } else {
                f.seen.replace(v.clone());
                // The writer updated the shared snapshot already.
                f.snapped.replace(v.clone());
                f.file.replace(v);
                f.file_seq.set(seq);
            }
            self.show(rt, &f, false, false);
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
    for f in inner.fields.borrow().iter() {
        let spec = f.spec.borrow();
        let item = (spec.encode)(&f.live.borrow());
        if item.to_string() == (spec.encode)(&f.shown.borrow()).to_string() {
            continue;
        }
        if read_only || f.overlay.borrow().is_some() {
            to_overlay.push((f.name.clone(), Some(item)));
        } else {
            to_file.push((f.name.clone(), Some(item)));
        }
    }
    let job = |edits, target| SettingsJob {
        edits,
        seq: next_seq(),
        target,
        file: inner.display.clone(),
        sink: inner.sink.clone(),
        corrupt_noticed: false,
    };
    if !to_file.is_empty() {
        let target = Target::File {
            overlay: inner.overlay.clone(),
            read_only: inner.read_only.clone(),
            noticed: inner.noticed.clone(),
        };
        inner.store.io.enqueue_settings(
            inner.snapshot.clone(),
            job(to_file.clone(), Target::Snapshot),
        );
        inner
            .store
            .io
            .enqueue_settings(inner.path.clone(), job(to_file, target));
    }
    if !to_overlay.is_empty() {
        inner
            .store
            .io
            .enqueue_settings(inner.overlay.clone(), job(to_overlay, Target::Overlay));
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

    /// Where its last good values are kept.
    pub fn last_good_path(&self) -> &Path {
        &self.inner.snapshot
    }

    /// Writes go to the overlay because the file is read-only.
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only.load(Ordering::Acquire)
    }

    /// The field's signal (`prefs.compact`): read it like any state, write
    /// it for a UI write or `strand set prefs.compact true` (saved after
    /// [`PERSIST_DEBOUNCE`] of quiet).
    pub fn signal(&self, field: &str) -> Option<Signal<V>> {
        self.inner.find(field).map(|f| f.signal)
    }

    /// Every field's name and signal, in declaration order.
    pub fn signals(&self) -> Vec<(Arc<str>, Signal<V>)> {
        self.inner
            .fields
            .borrow()
            .iter()
            .map(|f| (f.name.clone(), f.signal))
            .collect()
    }

    /// A UI write or `strand set`: [`Signal::set`] on the field.
    pub fn set(&self, rt: &Runtime, field: &str, value: V) -> Result<(), Error> {
        self.inner.field(field)?.signal.set(rt, value)
    }

    /// The field's runtime overlay value, if any.
    pub fn overlay(&self, field: &str) -> Option<V> {
        self.inner
            .find(field)
            .and_then(|f| f.overlay.borrow().clone())
    }

    /// Which layer the field's value comes from (the inspector's
    /// provenance); `None` for an unknown field. A UI write not saved yet
    /// counts for the layer it will be saved to once it is.
    pub fn layer(&self, field: &str) -> Option<Layer> {
        self.inner.find(field).map(|f| f.layer())
    }

    /// What a reload reads, for the watcher's thread.
    pub fn sources(&self) -> SettingsSources {
        self.inner.sources()
    }

    /// The file changed (the watcher): re-read it and the overlay (on this
    /// thread; [`Settings::reload_with`] takes a read made elsewhere). Each
    /// field is checked on its own; a syntax error keeps every last good
    /// value; a deleted key springs back to its default; a file change to
    /// a field the overlay shadows is reported.
    pub fn reload(&self, rt: &Runtime) {
        self.reload_with(rt, self.inner.sources().read());
    }

    /// [`Settings::reload`] with what [`SettingsSources::read`] read on
    /// another thread: the logic thread only decodes.
    pub fn reload_with(&self, rt: &Runtime, read: SettingsRead) {
        self.inner.apply(rt, read, false);
        for f in self.inner.fields() {
            self.inner.show(rt, &f, false, false);
        }
    }

    /// Set a runtime overlay value: it wins over the file until cleared,
    /// and is kept across restarts.
    pub fn set_overlay(&self, rt: &Runtime, field: &str, value: V) -> Result<(), Error> {
        let f = self.inner.field(field)?;
        let edits = vec![(f.name.clone(), Some(f.encode(&value)))];
        f.overlay.replace(Some(value));
        let seq = self
            .inner
            .enqueue(&self.inner.overlay, edits.clone(), Target::Overlay, false);
        f.overlay_seq.set(seq);
        self.inner.show(rt, &f, true, false);
        self.inner.tell_siblings(rt, true, &edits, seq);
        Ok(())
    }

    /// The overlay's `[clear]`: drop the field's overlay value, so the
    /// file (or the default) applies again.
    pub fn clear_overlay(&self, rt: &Runtime, field: &str) -> Result<(), Error> {
        let f = self.inner.field(field)?;
        if f.overlay.replace(None).is_none() {
            return Ok(());
        }
        let edits = vec![(f.name.clone(), None)];
        let seq = self
            .inner
            .enqueue(&self.inner.overlay, edits.clone(), Target::Overlay, false);
        f.overlay_seq.set(seq);
        self.inner.show(rt, &f, true, false);
        self.inner.tell_siblings(rt, true, &edits, seq);
        Ok(())
    }

    /// Queue what the user wrote now instead of after the debounce.
    pub fn write_out(&self, rt: &Runtime) {
        self.inner.write_out(Some(rt));
    }

    /// Live reload changed the declaration: the reconciler calls this
    /// instead of creating a new handle. Fields are matched by name and
    /// keep their signal (so dependents and a live value not saved yet are
    /// kept). A changed default is adopted where neither the file nor the
    /// overlay sets the field and the user has not written it; a changed
    /// type ([`FieldSpec::with_type`]) resets that field, read again under
    /// the new type, with [`SettingsIssue::TypeChanged`]; an added field is
    /// read from the file (or its last good value); a removed field's cell
    /// is disposed (its key stays in the file).
    pub fn redeclare(&self, rt: &Runtime, fields: Vec<FieldSpec<V>>) {
        let inner = &self.inner;
        let old = inner.fields();
        let mut next: Vec<Rc<Field<V>>> = Vec::with_capacity(fields.len());
        let mut reset = Vec::new();
        for s in fields {
            let Some(f) = old.iter().find(|f| f.name == s.name) else {
                next.push(Rc::new(inner.new_field(rt, s)));
                continue;
            };
            let old_ty = f.spec.borrow().ty.clone();
            if old_ty != s.ty {
                inner.notice(
                    rt,
                    Some(&f.name),
                    SettingsIssue::TypeChanged {
                        from: old_ty,
                        to: s.ty.clone(),
                    },
                );
                f.file.replace(None);
                f.seen.replace(None);
                f.overlay.replace(None);
                f.snapped.replace(None);
                f.file_seq.set(0);
                f.overlay_seq.set(0);
                f.fresh.set(true);
                reset.push(f.name.clone());
            }
            *f.spec.borrow_mut() = Spec {
                ty: s.ty,
                default: s.default,
                decode: s.decode,
                encode: s.encode,
            };
            next.push(f.clone());
        }
        for f in &old {
            if !next.iter().any(|n| Rc::ptr_eq(n, f)) {
                f.signal.dispose(rt);
            }
        }
        let any_fresh = next.iter().any(|f| f.fresh.get());
        *inner.fields.borrow_mut() = next;
        if any_fresh {
            inner.apply(rt, inner.sources().read(), true);
        }
        for f in inner.fields() {
            inner.show(rt, &f, reset.contains(&f.name), true);
        }
        // The saver tracks the new field list.
        let n = inner.schema.get_untracked(rt).unwrap_or(0);
        let _ = inner.schema.set(rt, n.wrapping_add(1));
    }
}

impl Runtime {
    /// `state prefs from "prefs.toml" { … }`: a two-way settings file at
    /// `path` (the compiler resolves it against the config directory) with
    /// the typed `fields` of its schema. Returns the handle; each field is
    /// an ordinary [`Signal`] ([`Settings::signal`]) owned by the current
    /// owner. See the module docs for the rules. Reports from reading come
    /// as [`Diagnostic::Settings`] now, from writing in a later tick.
    ///
    /// Boot reads the file, its overlay and its last-good snapshot on this
    /// thread (small files, once), and sweeps temp files a crash left next
    /// to the file.
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
        let target = resolve_links(&path);
        if let (Some(dir), Some(name)) = (target.parent(), target.file_name()) {
            sweep_temps(dir, Some(&name.to_string_lossy()));
        }
        let inner = Rc::new(Inner {
            overlay: store.overlay_of(&path),
            snapshot: store.last_good_of(&path),
            owner: self.current_owner(),
            read_only: Arc::new(AtomicBool::new(false)),
            probed: Cell::new(false),
            noticed: Arc::new(AtomicBool::new(false)),
            sink: self.inner.persist_failures.clone(),
            schema: self.signal(0u64),
            store: store.clone(),
            display: Arc::from(path.to_string_lossy()),
            path,
            fields: RefCell::new(Vec::new()),
        });
        let fields = fields
            .into_iter()
            .map(|s| Rc::new(inner.new_field(self, s)))
            .collect();
        *inner.fields.borrow_mut() = fields;
        {
            let mut stores = self.inner.persist_stores.borrow_mut();
            if !stores.iter().any(|s| s.same(&store.io)) {
                stores.push(store.io.clone());
            }
        }
        inner.apply(self, inner.sources().read(), false);
        for f in inner.fields() {
            let v = f.effective();
            f.shown.replace(v.clone());
            f.live.replace(v.clone());
            // Its starting value, not a write: nothing observes it yet.
            f.signal.init_value(self, v);
        }
        {
            let mut reg = self.inner.settings_files.borrow_mut();
            reg.retain(|r| r.handle.strong_count() > 0);
            let handle: Rc<dyn Sibling> = inner.clone();
            reg.push(Registered {
                overlay: inner.overlay.clone(),
                handle: Rc::downgrade(&handle),
            });
        }
        // The tracking effect keeps the handle alive with the field cells.
        let tracked = inner.clone();
        let saver: Weak<Inner<V>> = Rc::downgrade(&inner);
        // A field written before the first tracking run (the mount tick)
        // differs from what was shown: that run arms the debounce itself,
        // since `on change` never fires for the first value.
        let debounce: Rc<Cell<Option<crate::timer::Timer>>> = Rc::new(Cell::new(None));
        let arm = debounce.clone();
        let first = Cell::new(true);
        let d = self.on_change_after(
            move |rt| {
                let inner = &tracked;
                inner.schema.get(rt)?;
                let fields = inner.fields();
                let mut values = Vec::with_capacity(fields.len());
                let mut written = false;
                for f in &fields {
                    let v = f.signal.get(rt)?;
                    written |= *f.shown.borrow() != v;
                    f.live.replace(v.clone());
                    values.push(v);
                }
                if first.replace(false)
                    && written
                    && let Some(t) = arm.get()
                {
                    t.restart(rt)?;
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
        debounce.set(Some(d.timer));
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
    fn a_table_replaced_by_a_value_gets_default_spacing() {
        let out = edit(
            "b = 5\n[a]\nx = 1\n",
            &[(Arc::from("a"), Some(toml_edit::value(9)))],
        );
        assert_eq!(out, "b = 5\na = 9\n");
    }

    #[test]
    fn reads_can_be_made_on_another_thread() {
        fn send<T: Send>() {}
        send::<SettingsSources>();
        send::<ReadMark>();
        send::<SettingsRead>();
    }

    #[test]
    fn a_later_edit_of_a_field_replaces_a_queued_one() {
        let sink = Arc::new(FailSink::new(Arc::new(crate::task::ReadyQueue::default())));
        let job = |edits: Vec<Edit>| SettingsJob {
            edits,
            seq: next_seq(),
            target: Target::Overlay,
            file: Arc::from("x"),
            sink: sink.clone(),
            corrupt_noticed: false,
        };
        let mut a = job(vec![
            (Arc::from("a"), Some(toml_edit::value(1))),
            (Arc::from("b"), Some(toml_edit::value(1))),
        ]);
        let later = job(vec![(Arc::from("a"), None)]);
        let later_seq = later.seq;
        assert!(later_seq > a.seq);
        a.merge(later);
        let names: Vec<_> = a
            .edits
            .iter()
            .map(|(n, i)| (n.to_string(), i.is_some()))
            .collect();
        assert_eq!(
            names,
            vec![("b".to_string(), true), ("a".to_string(), false)]
        );
        assert_eq!(
            a.seq, later_seq,
            "a merged job is as late as its latest part"
        );
    }
}

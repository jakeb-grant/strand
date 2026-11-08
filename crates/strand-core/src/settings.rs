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

    /// `settings` in strand's state directory (`$STRAND_STATE_DIR`, else
    /// `$XDG_STATE_HOME/strand`: see [`PersistStore::from_env`]), with its
    /// own IO thread.
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
type Carry<V> = Box<dyn Fn(&V) -> Option<V>>;

/// One typed field of a settings file, from the compiler's schema.
pub struct FieldSpec<V> {
    name: Arc<str>,
    ty: Arc<str>,
    default: V,
    decode: Decode<V>,
    encode: Encode<V>,
    carry: Option<Carry<V>>,
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
            carry: None,
        }
    }

    /// The field's declared type, as written (`color`, `bool`): a
    /// [`Settings::redeclare`] that changes it resets the field.
    #[must_use]
    pub fn with_type(mut self, ty: impl Into<Arc<str>>) -> Self {
        self.ty = ty.into();
        self
    }

    /// (dictionary) How a value the handle holds becomes a value of the
    /// redeclared program, for a [`Settings::redeclare`] that keeps the
    /// field's type (the compiler's `translate`: an enum by its variant's
    /// name, a record by field names, a dictionary's entries whose key is
    /// gone dropped); `None` when it no longer fits. Each layer and the
    /// field's cell are carried through it; a field one of them no longer
    /// fits is read again from its files. Without it, values are kept as
    /// they are.
    #[must_use]
    pub fn with_carry(mut self, carry: impl Fn(&V) -> Option<V> + 'static) -> Self {
        self.carry = Some(Box::new(carry));
        self
    }
}

impl<V: fmt::Debug> fmt::Debug for FieldSpec<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FieldSpec")
            .field("name", &self.name)
            .field("ty", &self.ty)
            .field("default", &self.default)
            .field("carry", &self.carry.is_some())
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
    /// syntax error is not overwritten, nor (settings-tables) a field
    /// whose value in the file is bad (the notice names the field).
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
            SettingsIssue::WriteFailed(m) => match &self.field {
                // (settings-tables) A field's write held back.
                Some(field) => write!(f, "{}: {field}: {m}", self.file),
                None => write!(f, "{}: not saved: {m}", self.file),
            },
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
/// new key is appended. (dictionary) A table written over a table (a
/// `[section]`, or an inline `{ … }` the user wrote) is merged key by
/// key ([`merge_item`]), so its comments, spacing and order stay too.
/// (settings-tables) So is a record written over a `[section]`, and a
/// list of records written over an array of tables, `[[name]]`, is
/// merged by value ([`merge_tables`]).
fn apply_edits(doc: &mut DocumentMut, edits: &[Edit]) {
    for (name, item) in edits {
        match item {
            None => {
                doc.remove(name);
            }
            Some(new) => match doc.get_mut(name) {
                Some(old) => {
                    // Anything else the user wrote under that name (a
                    // table where a value goes now): replaced as a new
                    // key, with the key's default spacing (its own decor
                    // belongs to a `[header]`).
                    if !merge_item(old, new) {
                        doc.remove(name);
                        doc.insert(name, new.clone());
                    }
                }
                None => {
                    doc.insert(name, new.clone());
                }
            },
        }
    }
}

/// Write `new` over `old` in place, keeping what the user wrote around
/// it: a value keeps its decor (spacing and trailing comment); a table
/// written over a table, or over an inline table, is merged key by key
/// ([`merge_table`], [`merge_value`]). (settings-tables) What reads the
/// same as `new` ([`same_item`]: `1` for `1.0`, a section for an inline
/// table, keys in any order) is left as the file has it, with its text
/// and comments; a record written over a `[section]` stays a section,
/// merged key by key, and a list of records written over `[[name]]`
/// stays an array of tables, merged by value ([`merge_tables`]). `false`
/// when `old` cannot take `new` in place (a table where a value goes, or
/// a value where a section was) and is to be replaced as a new key.
fn merge_item(old: &mut Item, new: &Item) -> bool {
    if same_item(old, new) {
        return true;
    }
    match (old, new) {
        (Item::Value(o), Item::Value(n)) => {
            merge_value(o, n);
            true
        }
        (Item::Table(o), Item::Table(n)) => {
            merge_table(o, n);
            true
        }
        // A section's entries into the inline table the user wrote: it
        // stays inline.
        (Item::Value(toml_edit::Value::InlineTable(o)), Item::Table(n)) => {
            let n = n.clone().into_inline_table();
            merge_inline(o, &n);
            true
        }
        // (settings-tables) A record into the section the user wrote: it
        // stays a section.
        (Item::Table(o), Item::Value(toml_edit::Value::InlineTable(n))) => {
            merge_table(o, &n.clone().into_table());
            true
        }
        // A list of records into the array of tables the user wrote. An
        // empty list is written `name = []`: no `[[name]]` at all would
        // read as the field's default.
        (Item::ArrayOfTables(o), Item::Value(toml_edit::Value::Array(n)))
            if !n.is_empty() && n.iter().all(toml_edit::Value::is_inline_table) =>
        {
            merge_tables(o, n);
            true
        }
        _ => false,
    }
}

/// [`merge_item`] for a section: a key `new` no longer has is removed
/// (with its comment), one both have is written in place, a new one is
/// appended at the section's end ([`fresh_in`]).
fn merge_table(old: &mut toml_edit::Table, new: &toml_edit::Table) {
    let gone: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k))
        .collect();
    for k in gone {
        old.remove(&k);
    }
    for (k, n) in new.iter() {
        match old.get_mut(k) {
            Some(o) => {
                if !merge_item(o, n) {
                    old.remove(k);
                    let n = fresh_in(old, n);
                    old.insert(k, n);
                }
            }
            None => {
                let n = fresh_in(old, n);
                old.insert(k, n);
            }
        }
    }
}

/// [`merge_item`] for two values: two inline tables merge key by key
/// ([`merge_inline`]), two arrays by value ([`merge_array`]); anything
/// else is replaced, keeping `old`'s decor. One that reads the same is
/// left alone.
fn merge_value(old: &mut toml_edit::Value, new: &toml_edit::Value) {
    if same_value(old, new) {
        return;
    }
    match (&mut *old, new) {
        (toml_edit::Value::InlineTable(o), toml_edit::Value::InlineTable(n)) => merge_inline(o, n),
        (toml_edit::Value::Array(o), toml_edit::Value::Array(n)) => merge_array(o, n),
        _ => {
            let decor = old.decor().clone();
            *old = new.clone();
            *old.decor_mut() = decor;
        }
    }
}

/// [`merge_table`] for an inline table.
fn merge_inline(old: &mut toml_edit::InlineTable, new: &toml_edit::InlineTable) {
    let gone: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k))
        .collect();
    for k in gone {
        old.remove(&k);
    }
    for (k, n) in new.iter() {
        match old.get_mut(k) {
            Some(o) => merge_value(o, n),
            None => {
                old.insert(k, n.clone());
            }
        }
    }
}

/// (settings-tables) The old element a new one keeps, by value: the
/// first unused one that reads the same, else (for a record) the first
/// unused one whose every key the new one has with the same value (an
/// element that left fields to their defaults). Never by position, so an
/// element's comments never go to another.
fn pick_old<'a>(
    olds: impl Iterator<Item = (usize, ElementRef<'a>)> + Clone,
    taken: &[bool],
    new: &toml_edit::Value,
) -> Option<usize> {
    let free = olds.filter(|(i, _)| !taken[*i]);
    free.clone()
        .find(|(_, o)| o.same(new))
        .or_else(|| {
            let n = new.as_inline_table()?;
            free.clone()
                .find(|(_, o)| o.table().is_some_and(|o| covers(n, o)))
        })
        .map(|(i, _)| i)
}

/// An element of an array or of an array of tables.
#[derive(Clone, Copy)]
enum ElementRef<'a> {
    Value(&'a toml_edit::Value),
    Table(&'a toml_edit::Table),
}

impl<'a> ElementRef<'a> {
    fn same(self, new: &toml_edit::Value) -> bool {
        match self {
            Self::Value(v) => same_value(v, new),
            Self::Table(t) => new.as_inline_table().is_some_and(|n| same_tables(t, n)),
        }
    }

    fn table(self) -> Option<&'a dyn toml_edit::TableLike> {
        match self {
            Self::Value(v) => v.as_inline_table().map(|t| t as &dyn toml_edit::TableLike),
            Self::Table(t) => Some(t as &dyn toml_edit::TableLike),
        }
    }
}

/// (settings-tables) A list of records into the array of tables the
/// user wrote, by value ([`pick_old`]): a record that takes an old table
/// keeps it, its comments and text (merged key by key), in the new
/// order; any other is a new table shaped like the first old one
/// ([`shaped`]); an old table no record took is removed with its
/// comments.
fn merge_tables(old: &mut toml_edit::ArrayOfTables, new: &toml_edit::Array) {
    let olds: Vec<toml_edit::Table> = old.iter().cloned().collect();
    let like = olds.first().cloned().unwrap_or_default();
    let mut taken = vec![false; olds.len()];
    let mut out = toml_edit::ArrayOfTables::new();
    for n in new.iter() {
        let Some(nt) = n.as_inline_table() else {
            continue;
        };
        let refs = olds.iter().map(ElementRef::Table).enumerate();
        match pick_old(refs, &taken, n) {
            Some(i) => {
                taken[i] = true;
                let mut t = olds[i].clone();
                merge_table(&mut t, &nt.clone().into_table());
                out.push(t);
            }
            None => out.push(shaped(nt, &like)),
        }
    }
    keep_order(&mut out);
    *old = out;
}

/// (settings-tables) Tables print in the order of their place in the
/// file, whatever the order of the array that holds them, so a table
/// moved before one it followed (and the tables under it) takes the
/// place of the last one before it: they print after it, in order.
fn keep_order(tables: &mut toml_edit::ArrayOfTables) {
    let mut last: Option<isize> = None;
    for t in tables.iter_mut() {
        if let (Some(p), Some(l)) = (t.position(), last)
            && p < l
        {
            place_at(t, l);
        }
        last = last.max(deepest(t));
    }
}

/// The last place in the file of `t` or a table under it.
fn deepest(t: &toml_edit::Table) -> Option<isize> {
    let mut at = t.position();
    for (_, i) in t.iter() {
        match i {
            Item::Table(c) => at = at.max(deepest(c)),
            Item::ArrayOfTables(a) => {
                for c in a.iter() {
                    at = at.max(deepest(c));
                }
            }
            _ => {}
        }
    }
    at
}

fn place_at(t: &mut toml_edit::Table, at: isize) {
    if t.position().is_some() {
        t.set_position(Some(at));
    }
    for (_, i) in t.iter_mut() {
        match i {
            Item::Table(c) => place_at(c, at),
            Item::ArrayOfTables(a) => {
                for c in a.iter_mut() {
                    place_at(c, at);
                }
            }
            _ => {}
        }
    }
}

/// (settings-tables) Two arrays merged by value ([`pick_old`]): an
/// element that takes an old one keeps its text and the comments on its
/// lines (the comment after an element's comma is its own, though the
/// parser files it with the next element), in the new order; any other
/// is written fresh, on a line of its own in an array the user spread
/// over lines. An array on one line has no comments, so its spacing
/// stays by position.
fn merge_array(old: &mut toml_edit::Array, new: &toml_edit::Array) {
    let raw = |r: Option<&toml_edit::RawString>| {
        r.and_then(toml_edit::RawString::as_str)
            .unwrap_or("")
            .to_string()
    };
    let olds: Vec<toml_edit::Value> = old.iter().cloned().collect();
    let prefixes: Vec<String> = olds.iter().map(|v| raw(v.decor().prefix())).collect();
    let suffixes: Vec<String> = olds.iter().map(|v| raw(v.decor().suffix())).collect();
    let trailing = raw(Some(old.trailing()));
    let lines = !olds.is_empty()
        && prefixes
            .iter()
            .chain(std::iter::once(&trailing))
            .any(|p| p.contains('\n'));
    // The part of a prefix up to its first line break is the end of the
    // line before it (the comment after the previous element, or after
    // `[`); the rest is this element's own lead.
    let split = |p: &str| match p.find('\n') {
        Some(at) => (p[..=at].to_string(), p[at + 1..].to_string()),
        None => (String::new(), p.to_string()),
    };
    let mut taken = vec![false; olds.len()];
    let mut out = toml_edit::Array::new();
    let (opener, indent) = split(prefixes.first().map_or("", String::as_str));
    let mut carry = opener;
    for (k, n) in new.iter().enumerate() {
        let refs = olds.iter().map(ElementRef::Value).enumerate();
        let pick = pick_old(refs, &taken, n);
        let mut v = match pick {
            Some(i) => {
                taken[i] = true;
                let mut v = olds[i].clone();
                merge_value(&mut v, n);
                v
            }
            None => {
                let mut v = n.clone();
                v.decor_mut().clear();
                v
            }
        };
        if lines {
            let (lead, tail) = match pick {
                Some(i) => {
                    let next = prefixes.get(i + 1).unwrap_or(&trailing);
                    (split(&prefixes[i]).1, split(next).0)
                }
                None => (indent.clone(), "\n".to_string()),
            };
            v.decor_mut().set_prefix(std::mem::take(&mut carry) + &lead);
            if pick.is_none() {
                v.decor_mut().set_suffix("");
            }
            carry = tail;
        } else {
            let default = if k == 0 { "" } else { " " };
            v.decor_mut()
                .set_prefix(prefixes.get(k).map_or(default, String::as_str));
            v.decor_mut()
                .set_suffix(suffixes.get(k).map_or("", String::as_str));
        }
        out.push_formatted(v);
    }
    if lines {
        out.set_trailing(carry + &split(&trailing).1);
    } else {
        out.set_trailing(&trailing);
    }
    out.set_trailing_comma(old.trailing_comma());
    *out.decor_mut() = old.decor().clone();
    *old = out;
}

/// (settings-tables) A value written fresh into `table`: a record where
/// the user wrote records as sections takes their shape ([`shaped`]),
/// anything else is written as it is.
fn fresh_in(table: &toml_edit::Table, new: &Item) -> Item {
    match (new, table.iter().find_map(|(_, i)| i.as_table())) {
        (Item::Value(toml_edit::Value::InlineTable(n)), Some(like)) => Item::Table(shaped(n, like)),
        _ => new.clone(),
    }
}

/// `new` as a section shaped like `like`: a field `like` has as a section
/// is a section, a list of records it has as `[[…]]` an array of tables,
/// anything else a value.
fn shaped(new: &toml_edit::InlineTable, like: &toml_edit::Table) -> toml_edit::Table {
    let mut out = toml_edit::Table::new();
    for (k, v) in new.iter() {
        let item = match (v, like.get(k)) {
            (toml_edit::Value::InlineTable(t), Some(Item::Table(l))) => Item::Table(shaped(t, l)),
            (toml_edit::Value::Array(a), Some(Item::ArrayOfTables(l)))
                if !a.is_empty() && a.iter().all(toml_edit::Value::is_inline_table) =>
            {
                let first = l.get(0).cloned().unwrap_or_default();
                let mut tables = toml_edit::ArrayOfTables::new();
                for t in a.iter().filter_map(toml_edit::Value::as_inline_table) {
                    tables.push(shaped(t, &first));
                }
                Item::ArrayOfTables(tables)
            }
            _ => {
                let mut v = v.clone();
                v.decor_mut().clear();
                Item::Value(v)
            }
        };
        out.insert(k, item);
    }
    out
}

/// (settings-tables) Whether two items read as the same value whatever
/// their text: numbers by value (`1` is `1.0`), texts by content, a
/// section as the inline table of its keys and an array of tables as the
/// array of its tables, keys in any order.
fn same_item(a: &Item, b: &Item) -> bool {
    if let (Some(x), Some(y)) = (a.as_table_like(), b.as_table_like()) {
        return same_tables(x, y);
    }
    let elements = |i: &Item| -> Option<Vec<Item>> {
        match i {
            Item::ArrayOfTables(a) => Some(a.iter().cloned().map(Item::Table).collect()),
            Item::Value(toml_edit::Value::Array(a)) => {
                Some(a.iter().cloned().map(Item::Value).collect())
            }
            _ => None,
        }
    };
    match (a, b) {
        (Item::Value(x), Item::Value(y)) => same_value(x, y),
        _ => match (elements(a), elements(b)) {
            (Some(x), Some(y)) => {
                x.len() == y.len() && x.iter().zip(&y).all(|(p, q)| same_item(p, q))
            }
            _ => false,
        },
    }
}

/// [`same_item`] for two values.
fn same_value(a: &toml_edit::Value, b: &toml_edit::Value) -> bool {
    use toml_edit::Value as V;
    match (a, b) {
        (V::String(x), V::String(y)) => x.value() == y.value(),
        (V::Integer(x), V::Integer(y)) => x.value() == y.value(),
        (V::Float(x), V::Float(y)) => x.value() == y.value(),
        (V::Integer(i), V::Float(f)) | (V::Float(f), V::Integer(i)) => {
            *i.value() as f64 == *f.value()
        }
        (V::Boolean(x), V::Boolean(y)) => x.value() == y.value(),
        (V::Datetime(x), V::Datetime(y)) => x.value() == y.value(),
        (V::Array(x), V::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same_value(p, q))
        }
        (V::InlineTable(x), V::InlineTable(y)) => same_tables(x, y),
        _ => false,
    }
}

fn same_tables(a: &dyn toml_edit::TableLike, b: &dyn toml_edit::TableLike) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, x)| b.get(k).is_some_and(|y| same_item(x, y)))
}

/// Whether `new` has every key `old` has, each with the same value.
fn covers(new: &toml_edit::InlineTable, old: &dyn toml_edit::TableLike) -> bool {
    old.iter()
        .all(|(k, x)| toml_edit::TableLike::get(new, k).is_some_and(|y| same_item(x, y)))
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
    /// (settings-tables) The file's value for it is bad (the last read
    /// did not decode): a write would replace what the user is fixing,
    /// a dictionary's other entries among it, so it is not saved there.
    bad: Cell<bool>,
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
                f.bad.set(read.is_none());
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

    /// (dictionary) A redeclare keeping `f`'s type: carry its layers and
    /// its cell into the new program through `carry`
    /// ([`FieldSpec::with_carry`]). `false`, with nothing changed, when
    /// one of them no longer fits (the field is then read again). The
    /// cell's new value is a reload write ([`Signal::set_reloaded`]), so
    /// `on change` takes it as its baseline.
    fn carry(&self, rt: &Runtime, f: &Field<V>, carry: &dyn Fn(&V) -> Option<V>) -> bool {
        let layer = |cell: &RefCell<Option<V>>| -> Option<Option<V>> {
            match &*cell.borrow() {
                Some(v) => carry(v).map(Some),
                None => Some(None),
            }
        };
        let Ok(cur) = f.signal.get_untracked(rt) else {
            return false;
        };
        let (
            Some(file),
            Some(seen),
            Some(overlay),
            Some(snapped),
            Some(shown),
            Some(live),
            Some(now),
        ) = (
            layer(&f.file),
            layer(&f.seen),
            layer(&f.overlay),
            layer(&f.snapped),
            carry(&f.shown.borrow()),
            carry(&f.live.borrow()),
            carry(&cur),
        )
        else {
            return false;
        };
        f.file.replace(file);
        f.seen.replace(seen);
        f.overlay.replace(overlay);
        f.snapped.replace(snapped);
        f.shown.replace(shown);
        f.live.replace(live);
        if now != cur && rt.check_write_allowed(f.signal.id()).is_ok() {
            rt.note_write(f.signal.id());
            let _ = f.signal.set_reloaded(rt, now);
        }
        true
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
            // (settings-tables) The file's value is bad: writing over it
            // would drop what the user wrote there (a dictionary's other
            // entries, merged key by key, would go). The value stays live
            // and is not saved; once the file reads again, it wins.
            if f.bad.get() && !read_only && !has_overlay {
                f.live.replace(v.clone());
                f.shown.replace(v);
                if let Some(rt) = rt {
                    self.notice(
                        rt,
                        Some(&f.name),
                        SettingsIssue::WriteFailed(Arc::from(
                            "not saved while its value in the file is bad; fix it there",
                        )),
                    );
                }
                continue;
            }
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
            bad: Cell::new(false),
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
                f.bad.set(false);
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
            let changed = old_ty != s.ty;
            if changed {
                inner.notice(
                    rt,
                    Some(&f.name),
                    SettingsIssue::TypeChanged {
                        from: old_ty,
                        to: s.ty.clone(),
                    },
                );
            }
            // (dictionary) The same type in the new program: each layer
            // and the cell carried into it; one that no longer fits reads
            // the field again from its files.
            let carried = changed
                || s.carry
                    .as_ref()
                    .is_none_or(|c| inner.carry(rt, f, c.as_ref()));
            if changed || !carried {
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

    fn section(entries: &[(&str, Item)]) -> Item {
        let mut t = toml_edit::Table::new();
        for (k, v) in entries {
            t.insert(k, v.clone());
        }
        Item::Table(t)
    }

    /// (dictionary) A table written over the section the file has merges
    /// key by key: a changed key keeps its place, spacing and comment, a
    /// key no longer there is removed (with its own comment), a new key
    /// goes at the section's end, and the rest of the file is untouched.
    #[test]
    fn a_table_is_written_key_by_key() {
        let doc = "# prefs\ngap = 4\n\n# per screen\n[wallpapers]\n# the left one\nDP-1   = \"~/a.jpg\"   # left\n# gone soon\nHDMI-A-1 = \"~/b.jpg\"\neDP-1 = \"~/c.jpg\" # laptop\n\n[other]\nx = 1\n";
        let new = section(&[
            ("DP-1", toml_edit::value("~/new.jpg")),
            ("eDP-1", toml_edit::value("~/c.jpg")),
            ("org.gnome.Nautilus", toml_edit::value("~/d.jpg")),
        ]);
        assert_eq!(
            edit(doc, &[(Arc::from("wallpapers"), Some(new))]),
            "# prefs\ngap = 4\n\n# per screen\n[wallpapers]\n# the left one\nDP-1   = \"~/new.jpg\"   # left\neDP-1 = \"~/c.jpg\" # laptop\n\"org.gnome.Nautilus\" = \"~/d.jpg\"\n\n[other]\nx = 1\n"
        );
        // An inline table the user wrote stays inline, merged the same way.
        let doc = "wallpapers = { DP-1 = \"~/a.jpg\", HDMI-A-1 = \"~/b.jpg\" } # mine\n";
        let new = section(&[
            ("DP-1", toml_edit::value("~/a.jpg")),
            ("eDP-1", toml_edit::value("~/c.jpg")),
        ]);
        assert_eq!(
            edit(doc, &[(Arc::from("wallpapers"), Some(new))]),
            "wallpapers = { DP-1 = \"~/a.jpg\", eDP-1 = \"~/c.jpg\" } # mine\n"
        );
        // A field the file does not have yet is a section of its own,
        // after the file's keys; an empty one is just its header.
        let new = section(&[("DP-1", toml_edit::value("~/a.jpg"))]);
        assert_eq!(
            edit(
                "gap = 4\n",
                &[
                    (Arc::from("wallpapers"), Some(new)),
                    (Arc::from("seen"), Some(section(&[]))),
                    (Arc::from("compact"), Some(toml_edit::value(true))),
                ]
            ),
            "gap = 4\ncompact = true\n\n[wallpapers]\nDP-1 = \"~/a.jpg\"\n\n[seen]\n"
        );
        // A bad value where the table goes is replaced by the section.
        let new = section(&[("DP-1", toml_edit::value("~/a.jpg"))]);
        assert_eq!(
            edit(
                "wallpapers = 3 # oops\ngap = 4\n",
                &[(Arc::from("wallpapers"), Some(new))]
            ),
            "gap = 4\n\n[wallpapers]\nDP-1 = \"~/a.jpg\"\n"
        );
    }

    /// (dictionary) A record written as an inline table keeps the
    /// spacing of the fields the user wrote, as a top-level value does.
    #[test]
    fn an_inline_table_keeps_its_spacing_field_by_field() {
        let mut t = toml_edit::InlineTable::new();
        t.insert("w", 3.into());
        t.insert("h", 4.into());
        assert_eq!(
            edit(
                "size = {  h = 2,   w = 1  } # cells\n",
                &[(Arc::from("size"), Some(toml_edit::value(t)))]
            ),
            "size = {  h = 4,   w = 3  } # cells\n"
        );
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

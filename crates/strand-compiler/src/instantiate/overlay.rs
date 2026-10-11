//! The token overlay layer and the instance's other IPC entry points
//! (S-ipc; docs/architecture.md, "The token overlay layer").
//!
//! **Token overlays** (decisions.md m5-owner Q10). `strand set '$accent'
//! '#ff8800'` writes a runtime overlay of the token `accent`: global (it
//! wins over every tier the theme's files define, as settings' runtime
//! overlay wins over their file), persisted in
//! `$XDG_STATE_HOME/strand/tokens.toml` beside settings' overlays, and
//! applied by the token table's memo, so a change takes the token swap
//! path (the palette roots spring, design.md "How a swap animates"). The
//! overlaid path's origin is [`OVERLAY_ORIGIN`] while the overlay holds;
//! the next build without it records the file's again.
//!
//! The file holds one key per token path (`accent = "#ff8800"`, `"space.2"
//! = "8px"`), each value the text `strand set` was given, read by the
//! token's type when the table is built, so a token whose type changes
//! (a colour made a length) does not take a stale value. Edits go through
//! `toml_edit` (a comment the user added stays), and are written off the
//! logic thread, atomically (temp file, `fsync`, rename), by the
//! overlay's own writer thread (decisions.md m5-ipc). A file that is not
//! valid TOML is moved aside to `.tokens.toml.corrupt`, as settings'
//! overlays are.
//!
//! **Who wins** (design.md, "Reactivity and state model"): runtime
//! overlay > file > default. A build that changes the file's value of an
//! overlaid token raises settings' notice, `accent: file changed but
//! runtime overlay wins [clear]` ([`SettingsIssue::Shadowed`], with the
//! overlay file as its file), whose `[clear]` reaches
//! [`Instance::clear_token_overlay`] through
//! [`Instance::clear_settings_overlay`]; `strand clear '$accent'` and IPC
//! `clear{path}` are its CLI twins.
//!
//! **Other entry points** S-ipc owns here: [`Instance::toggle`] (`strand
//! toggle`, booleans only), [`Instance::layer`] (`strand get --json`'s
//! layer of a settings field), [`Instance::clear_overlay`] (`strand
//! clear` on a settings field) and [`Instance::token`] (`strand get
//! '$accent'`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use strand_core::settings::toml_edit::{DocumentMut, value as toml_value};
use strand_core::{
    Diagnostic, Error, Runtime, Scope, SettingsIssue, SettingsLayer, SettingsNotice, Signal,
};
use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{Length, PropValue, TokenTable};

use super::{Ctx, Instance};
use crate::hir::DefKind;
use crate::ty::{Prim, Ty, TypeTable};
use crate::vm::Value;

/// The overlay's file name, in `$XDG_STATE_HOME/strand/`.
pub const TOKENS_FILE: &str = "tokens.toml";

/// How long dropping the last handle waits for a queued write.
const WRITE_WAIT: Duration = Duration::from_secs(5);

/// The token overlay of a [`super::Storage`]: cheap to clone, shared by
/// every build an instance commits (the storage is carried across
/// reloads). [`TokenOverlay::default`] keeps its values in memory only.
#[derive(Clone, Default)]
pub struct TokenOverlay(Arc<Shared>);

impl std::fmt::Debug for TokenOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenOverlay")
            .field("file", &self.0.file)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Shared {
    /// `tokens.toml`; `None`: in memory only.
    file: Option<PathBuf>,
    state: Mutex<State>,
    writer: Writer,
}

#[derive(Default)]
struct State {
    /// The file has been read (at the first build, and again when a
    /// `set` or `clear` finds it changed by hand: see [`TokenOverlay::writable`]).
    loaded: bool,
    /// What the file held when last read or last queued to the writer
    /// (`None`: no file): a `set` or `clear` that finds anything else
    /// there reads it again before editing, so a hand edit made while
    /// the shell runs is never written over.
    known: Option<String>,
    /// The file is there but could not be read (why): it is not written
    /// over, so `set` and `clear` are refused and read it again.
    unreadable: Option<String>,
    /// Entries of the file that are not a token's value (a table made by
    /// an unquoted dotted key, an array), for the next table's notices.
    skipped: Vec<String>,
    /// What the file holds, edited in place.
    doc: DocumentMut,
    /// Overlaid paths (no `$`) and their text.
    values: Keyed<String>,
    /// Each overlaid path's value without the overlay, as the last build
    /// of the table had it (`None`: no such token).
    base: Keyed<Option<PropValue>>,
    /// The program the last table was built from (its address): a new
    /// one is a committed build, whose changed file values are shadowed.
    program: usize,
    /// Read by the token table's memo, bumped by every change: the table
    /// is built again. A root node of the instance's runtime.
    generation: Option<Signal<u64>>,
    /// Overlay values the token's type does not read, already said.
    bad: Vec<(String, String)>,
    /// Notices for the next update (settings' kind, so the overlay shows
    /// them with `[clear]` and `strand watch` hears them).
    notices: Vec<Diagnostic>,
}

/// A few entries kept sorted by path: overlays are a handful, so a
/// vector beats a map's code for them.
struct Keyed<V>(Vec<(String, V)>);

impl<V> Default for Keyed<V> {
    fn default() -> Self {
        Keyed(Vec::new())
    }
}

impl<V> Keyed<V> {
    fn find(&self, path: &str) -> Result<usize, usize> {
        self.0.binary_search_by(|(k, _)| k.as_str().cmp(path))
    }

    fn get(&self, path: &str) -> Option<&V> {
        self.find(path).ok().map(|i| &self.0[i].1)
    }

    /// Set `path` to `v`; the value it had.
    fn insert(&mut self, path: String, v: V) -> Option<V> {
        match self.find(&path) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, v)),
            Err(i) => {
                self.0.insert(i, (path, v));
                None
            }
        }
    }

    fn remove(&mut self, path: &str) -> Option<V> {
        self.find(path).ok().map(|i| self.0.remove(i).1)
    }
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The file as notices name it.
    fn display(&self) -> Arc<str> {
        match &self.file {
            Some(f) => Arc::from(f.to_string_lossy()),
            None => Arc::from(TOKENS_FILE),
        }
    }

    fn notice(&self, field: Option<&str>, issue: SettingsIssue) -> Diagnostic {
        Diagnostic::Settings(SettingsNotice {
            file: self.display(),
            field: field.map(Arc::from),
            issue,
        })
    }
}

impl TokenOverlay {
    /// An overlay persisted in `file`.
    pub fn at(file: impl Into<PathBuf>) -> Self {
        TokenOverlay(Arc::new(Shared {
            file: Some(file.into()),
            ..Shared::default()
        }))
    }

    /// The overlay file (`None`: in memory only).
    pub fn file(&self) -> Option<&Path> {
        self.0.file.as_deref()
    }

    /// Overlaid token paths (no `$`) and their text, in order.
    pub fn values(&self) -> Vec<(String, String)> {
        let mut st = self.0.state();
        self.load(&mut st);
        st.values.0.clone()
    }

    /// Wait until the queued write reached the disk, at most `timeout`
    /// (tests; dropping the last handle waits too).
    pub fn sync(&self, timeout: Duration) -> bool {
        self.0.writer.wait(timeout)
    }

    /// Make the generation signal the table's memo reads, owned by the
    /// boot's root scope (`owner`), so disposing that scope (a reload's
    /// old root, a dropped instance) disposes it with the memo that read
    /// it; `set` and `clear` bump the newest. Called by the boot before it
    /// makes the memo, where no memo owns new nodes.
    pub(crate) fn prepare(&self, rt: &Runtime, owner: Option<Scope>) {
        let make = |rt: &Runtime| rt.signal(0u64);
        let signal = match owner {
            Some(o) => o.run(rt, make).unwrap_or_else(|_| make(rt)),
            None => make(rt),
        };
        self.0.state().generation = Some(signal);
    }

    /// The table's memo: read the generation (so a change builds it
    /// again), note the file's value of each overlaid token (a new build
    /// that changed one is shadowed: a notice), and put the overlay over
    /// it with [`OVERLAY_ORIGIN`]. A value the token's type does not read
    /// leaves the file's value, said once in `notices`.
    pub(crate) fn apply(
        &self,
        rt: &Runtime,
        program: usize,
        types: &TypeTable,
        t: &mut TokenTable,
        notices: &std::cell::RefCell<Vec<String>>,
    ) {
        let mut st = self.0.state();
        if let Some(g) = st.generation {
            // Errors only if disposed: the next boot makes it again.
            let _ = g.get(rt);
        }
        self.load(&mut st);
        let st = &mut *st;
        notices.borrow_mut().append(&mut st.skipped);
        let committed = st.program != 0 && st.program != program;
        st.program = program;
        // The file's values first: a derived token reads the overlaid
        // ones once they are in.
        let files: Vec<(String, String, Option<PropValue>)> = st
            .values
            .0
            .iter()
            .map(|(p, text)| (p.clone(), text.clone(), t.lookup(p)))
            .collect();
        for (path, text, file) in files {
            let before = st.base.insert(path.clone(), file.clone());
            if committed && before.is_some_and(|b| b != file) {
                st.notices
                    .push(self.0.notice(Some(&path), SettingsIssue::Shadowed));
            }
            let Some(file) = file else {
                // No such token in this build (yet): kept for one that
                // has it.
                continue;
            };
            match read(types, &file, &text) {
                Ok(v) => {
                    t.insert(path.clone(), v);
                    t.set_origin(path, OVERLAY_ORIGIN);
                }
                Err(why) => {
                    let said = (path.clone(), text.clone());
                    if !st.bad.contains(&said) {
                        st.bad.push(said);
                        notices.borrow_mut().push(format!(
                            "${path}: runtime overlay {why}; the theme's value applies \
                             (strand clear '${path}')"
                        ));
                    }
                }
            }
        }
    }

    /// `diagnostics` with the overlay's notices after them (a shadowed
    /// save, a file that could not be read or written).
    pub(crate) fn with_notices(&self, mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
        let mut st = self.0.state();
        diagnostics.append(&mut st.notices);
        if let Some(why) = self.0.writer.take_failure() {
            diagnostics.push(
                self.0
                    .notice(None, SettingsIssue::WriteFailed(Arc::from(why))),
            );
        }
        diagnostics
    }

    /// Set `path`'s overlay to `text` (already checked) and build the
    /// table again; refused while the file cannot be read.
    fn set(&self, rt: &Runtime, path: &str, text: &str) -> Result<(), Error> {
        let mut st = self.0.state();
        self.writable(&mut st)?;
        st.values.insert(path.to_string(), text.to_string());
        // In place where the key is there, so a comment above it stays.
        match st.doc.get_mut(path) {
            Some(item) => *item = toml_value(text),
            None => {
                st.doc.insert(path, toml_value(text));
            }
        }
        self.changed(rt, &mut st);
        Ok(())
    }

    /// Drop `path`'s overlay; whether it had one. Refused while the file
    /// cannot be read.
    /// An entry of the file that is not a token's value (an array, a
    /// table: said at load, never in `values`) is cleared too, as its
    /// notice advises.
    fn clear(&self, rt: &Runtime, path: &str) -> Result<bool, Error> {
        let mut st = self.0.state();
        let reread = self.writable(&mut st)?;
        let had = st.values.remove(path).is_some();
        let entry = st.doc.as_table_mut().remove(path).is_some();
        if !had && !entry {
            // A file read again for the first time holds overlays the
            // table does not have yet: build it again.
            if reread && let Some(g) = st.generation {
                let _ = g.update(rt, |n| *n += 1);
            }
            return Ok(false);
        }
        st.base.remove(path);
        self.changed(rt, &mut st);
        Ok(true)
    }

    /// Whether `path` has an overlay, or an entry in the file that is
    /// not one (which `clear` drops).
    fn has(&self, path: &str) -> bool {
        let mut st = self.0.state();
        self.load(&mut st);
        st.values.get(path).is_some() || st.doc.contains_key(path)
    }

    /// Loaded, current and safe to write: a file that could not be read,
    /// or that holds other than what was last read or written (a hand
    /// edit while the shell runs), is read again, as settings' overlay
    /// edits read the file they change; while it still cannot be read,
    /// the change is refused rather than writing the overlays it holds
    /// away. Whether it was read again just now (its overlays are new to
    /// the table).
    fn writable(&self, st: &mut State) -> Result<bool, Error> {
        let reread = st.unreadable.is_some() || (st.loaded && self.edited(st));
        if reread {
            st.loaded = false;
        }
        self.load(st);
        match &st.unreadable {
            None => Ok(reread),
            Some(why) => Err(Error::failed(format!(
                "{} could not be read ({why}); it is not written over, so the token \
                 overlay is unchanged",
                self.0.display()
            ))),
        }
    }

    /// Whether the file holds other than [`State::known`]. Not while a
    /// write is queued or running (the file is ours until it lands); a
    /// small file read on a `set` or `clear`, never on a frame.
    #[cold]
    #[inline(never)]
    fn edited(&self, st: &State) -> bool {
        let Some(file) = &self.0.file else {
            return false;
        };
        if !self.0.writer.idle() {
            return false;
        }
        match std::fs::read_to_string(file) {
            Ok(t) => st.known.as_deref() != Some(t.as_str()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => st.known.is_some(),
            Err(_) => true,
        }
    }

    fn changed(&self, rt: &Runtime, st: &mut State) {
        if let Some(g) = st.generation {
            let _ = g.update(rt, |n| *n += 1);
        }
        if let Some(file) = &self.0.file {
            let text = st.doc.to_string();
            st.known = Some(text.clone());
            self.0.writer.save(file.clone(), text);
        }
    }

    /// Read the file, once (again after it could not be read: see
    /// [`Self::writable`]).
    fn load(&self, st: &mut State) {
        if !st.loaded {
            self.read_file(st);
        }
    }

    /// [`Self::load`]'s read: once per boot (and per hand edit), so kept
    /// out of the memo. What an earlier read held is replaced: an overlay
    /// removed by hand is gone.
    #[cold]
    #[inline(never)]
    fn read_file(&self, st: &mut State) {
        st.loaded = true;
        let Some(file) = &self.0.file else {
            return;
        };
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                st.unreadable = None;
                st.known = None;
                st.doc = DocumentMut::new();
                st.values.0.clear();
                st.base.0.clear();
                return;
            }
            Err(e) => {
                let why = e.to_string();
                // Said once, not at every retry.
                if st.unreadable.is_none() {
                    st.notices.push(
                        self.0
                            .notice(None, SettingsIssue::Unreadable(Arc::from(why.as_str()))),
                    );
                }
                st.unreadable = Some(why);
                return;
            }
        };
        st.unreadable = None;
        st.values.0.clear();
        let doc: DocumentMut = match text.parse() {
            Ok(d) => d,
            Err(e) => {
                st.known = None;
                st.doc = DocumentMut::new();
                st.base.0.clear();
                let moved_to = aside(file);
                st.notices.push(self.0.notice(
                    None,
                    SettingsIssue::CorruptOverlay {
                        moved_to,
                        error: Arc::from(e.to_string().trim_end()),
                    },
                ));
                return;
            }
        };
        for (k, item) in doc.iter() {
            let text = item.as_value().and_then(|v| {
                match (v.as_str(), v.as_integer(), v.as_float(), v.as_bool()) {
                    (Some(s), ..) => Some(s.to_string()),
                    (_, Some(i), ..) => Some(i.to_string()),
                    (_, _, Some(f), _) => Some(f.to_string()),
                    (.., Some(b)) => Some(b.to_string()),
                    _ => None,
                }
            });
            match text {
                Some(text) => {
                    st.values.insert(k.to_string(), text);
                }
                None => st.skipped.push(skipped(&self.0.display(), k, item)),
            }
        }
        // A path no longer overlaid has no base: overlaid again later,
        // its first build is not a shadowed save.
        let values = &st.values;
        st.base.0.retain(|(p, _)| values.get(p).is_some());
        st.known = Some(text);
        st.doc = doc;
    }
}

/// The notice for an entry of the file that is not a token's value: a
/// table (an unquoted dotted key, `radius.lg = "4px"`, makes one) is
/// told to quote its path.
fn skipped(file: &str, key: &str, item: &strand_core::settings::toml_edit::Item) -> String {
    let mut path = key.to_string();
    let mut at = item;
    while let Some((k, next)) = at.as_table_like().and_then(|t| t.iter().next()) {
        path.push('.');
        path.push_str(k);
        at = next;
    }
    if path != key {
        format!(
            "{file}: `{key}` is a table, not a token's value, and is ignored; quote a \
             token's path as its key (\"{path}\" = ...)"
        )
    } else {
        format!(
            "{file}: `{key}` is not text, a number or a bool, and is ignored (strand clear \
             '${key}')"
        )
    }
}

/// Move a broken overlay file aside (`.tokens.toml.corrupt`), as
/// settings' overlays are; where it went.
fn aside(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map_or_else(|| TOKENS_FILE.into(), |n| n.to_string_lossy().into_owned());
    let to = file.with_file_name(format!(".{name}.corrupt"));
    let _ = std::fs::rename(file, &to);
    to
}

/// `text` as a value of the token whose file value is `file`: read as
/// `strand set` reads a state of that type (`#rrggbb`, `8` or `8px`,
/// `40%`, `90deg`, `150ms` or `0.2s`, text as given, `true`/`false`).
fn read(types: &TypeTable, file: &PropValue, text: &str) -> Result<PropValue, String> {
    let prim = match file {
        PropValue::Color(_) => Prim::Color,
        PropValue::Number(_) => Prim::Length,
        PropValue::Length(Length::Percent(_)) => Prim::Percent,
        PropValue::Angle(_) => Prim::Angle,
        PropValue::Duration(_) => Prim::Duration,
        PropValue::Text(_) => Prim::Text,
        PropValue::Bool(_) => Prim::Bool,
        _ => {
            return Err(
                "is not one of the kinds an overlay takes (a colour, a number or \
                 length, a percentage, an angle, a duration, text or a bool)"
                    .into(),
            );
        }
    };
    let ty = Ty::Prim(prim);
    super::parse_text(types, &ty, text.trim())
        .map(|v| super::convert::prop_value(types, &ty, &v))
        .filter(|v| !matches!(v, PropValue::Unset))
        .ok_or_else(|| format!("`{text}` is not a {}", types.show(&ty)))
}

/// The overlay's writer: one thread at a time writes the newest text
/// queued, so the logic thread never waits for the disk.
#[derive(Default)]
struct Writer(Arc<(Mutex<Slot>, Condvar)>);

#[derive(Default)]
struct Slot {
    /// The newest text not written yet.
    next: Option<(PathBuf, String)>,
    /// A thread is writing.
    running: bool,
    /// The last write's failure, not said yet.
    failed: Option<String>,
}

impl Writer {
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&self, file: PathBuf, text: String) {
        let mut s = self.slot();
        s.next = Some((file, text));
        if s.running {
            return;
        }
        s.running = true;
        let shared = Arc::clone(&self.0);
        let spawned = std::thread::Builder::new()
            .name("strand-tokens".into())
            .spawn(move || {
                let (lock, cv) = &*shared;
                loop {
                    let job = {
                        let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        match s.next.take() {
                            Some(j) => j,
                            None => {
                                s.running = false;
                                cv.notify_all();
                                return;
                            }
                        }
                    };
                    let r = write_atomic(&job.0, &job.1);
                    let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    s.failed = r.err().map(|e| format!("{}: {e}", job.0.display()));
                }
            });
        if let Err(e) = spawned {
            s.running = false;
            s.next = None;
            s.failed = Some(format!("no writer thread: {e}"));
        }
    }

    /// Nothing queued or being written.
    fn idle(&self) -> bool {
        let s = self.slot();
        !s.running && s.next.is_none()
    }

    fn take_failure(&self) -> Option<String> {
        self.slot().failed.take()
    }

    /// Wait until nothing is queued or being written, at most `timeout`.
    fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.slot();
        while s.running || s.next.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            s = self
                .0
                .1
                .wait_timeout(s, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // The overlay's last handle (at exit): a write just queued still
        // lands.
        self.wait(WRITE_WAIT);
    }
}

/// Distinct temp names within one process.
static TEMP: AtomicU64 = AtomicU64::new(0);

/// Write `text` to `file` atomically: a temp file beside it (`.<name>
/// .tmp.<pid>.<n>`), synced, renamed over it, the directory synced. A
/// symlinked file is written through to its target.
fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf())
        }
        _ => file.to_path_buf(),
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map_or_else(|| TOKENS_FILE.into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let r = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `no token `$path``, with the closest name the table has.
fn no_token(table: &TokenTable, path: &str) -> Error {
    // A `Vec<&str>`: the instantiation of `suggest` the binary has.
    let names: Vec<&str> = (table.tokens.keys().chain(table.derived.keys()))
        .map(String::as_str)
        .collect();
    let hint = crate::diagnostic::suggest(path, names)
        .map(|s| format!("; did you mean `${s}`?"))
        .unwrap_or_default();
    Error::failed(format!("no token `${path}`{hint}"))
}

impl Ctx {
    /// The table's last step: the token overlay over it.
    pub(super) fn token_overlay(
        &self,
        rt: &Runtime,
        prog: &Arc<crate::lower::Program>,
        t: &mut TokenTable,
    ) {
        let program = Arc::as_ptr(prog) as usize;
        self.storage
            .tokens
            .apply(rt, program, &prog.types, t, &self.notices);
    }
}

impl Instance {
    /// The token table as the scene has it now, the overlay included.
    fn token_table_now(&self) -> Option<TokenTable> {
        self.tokens.and_then(|t| t.get_untracked(&self.rt).ok())
    }

    /// `strand set '$accent' '#ff8800'`: overlay the token at `path` (no
    /// `$`) with `text`, read by the token's type as `strand set` reads a
    /// state's. It wins over every tier the theme's files define, is
    /// kept in `tokens.toml` and takes the token swap path. An unknown
    /// token is refused (with the closest name), and so is a value its
    /// type does not read.
    pub fn set_token_overlay(&self, path: &str, text: &str) -> Result<(), Error> {
        let table = self
            .token_table_now()
            .ok_or_else(|| Error::failed("no token table yet"))?;
        let Some(current) = table.lookup(path) else {
            return Err(no_token(&table, path));
        };
        read(&self.ctx.vm.prog.types, &current, text)
            .map_err(|why| Error::failed(format!("`${path}`: {why}")))?;
        self.ctx.storage.tokens.set(&self.rt, path, text.trim())
    }

    /// `strand clear '$accent'` and the notice's `[clear]`: drop the
    /// token overlay at `path` (no `$`), so the theme's value applies
    /// again. Whether there was one: `false` for a token without one; an
    /// unknown token is refused with the closest name, as
    /// [`Self::set_token_overlay`] refuses it (an overlay of a token this
    /// build no longer defines is still cleared). Refused while
    /// `tokens.toml` cannot be read.
    pub fn clear_token_overlay(&self, path: &str) -> Result<bool, Error> {
        let tokens = &self.ctx.storage.tokens;
        if !tokens.has(path) {
            let table = self
                .token_table_now()
                .ok_or_else(|| Error::failed("no token table yet"))?;
            if table.lookup(path).is_none() {
                return Err(no_token(&table, path));
            }
        }
        tokens.clear(&self.rt, path)
    }

    /// The token at `path` (no `$`) as the scene has it, the overlay
    /// included, with where it was defined ([`TokenTable::origin`]:
    /// [`OVERLAY_ORIGIN`] while overlaid); `None` for no such token.
    pub fn token(&self, path: &str) -> Option<(PropValue, Option<String>)> {
        let table = self.token_table_now()?;
        let v = table.lookup(path)?;
        Some((v, table.origin(path).map(str::to_string)))
    }

    /// A settings notice's `[clear]` on the token overlay's file: the
    /// token's overlay goes (`None`: `file` is another file).
    pub(super) fn clear_token_notice(&self, file: &str, field: &str) -> Option<bool> {
        let tokens = &self.ctx.storage.tokens;
        // The notice names an overlaid token: no table check. A file that
        // cannot be read clears nothing.
        (*tokens.0.display() == *file).then(|| tokens.clear(&self.rt, field).unwrap_or(false))
    }

    /// The settings file declaration and field a path names
    /// (`theme.prefs.accent`, `prefs.accent`).
    fn settings_field(&self, path: &str) -> Option<(crate::hir::DefId, String)> {
        let (d, rest) = self.export(path).ok()?;
        match (&self.ctx.vm.prog.def(d).kind, &rest[..]) {
            (DefKind::Settings, [field]) => Some((d, field.clone())),
            _ => None,
        }
    }

    /// Which layer a settings field's value comes from (runtime
    /// overlay, file or default: `strand get --json`); `None` for a path
    /// that is not a mounted settings field.
    pub fn layer(&self, path: &str) -> Option<SettingsLayer> {
        let (d, field) = self.settings_field(path)?;
        let slot = self.ctx.vm.root.settings(d)?;
        slot.handle.as_ref()?.layer(&field)
    }

    /// `strand clear <file.settings.field>`: drop the field's runtime
    /// overlay, so the file (or the default) applies. Whether it had
    /// one; an error for a path that is not a settings field.
    pub fn clear_overlay(&self, path: &str) -> Result<bool, Error> {
        let not = || {
            Error::failed(format!(
                "`{path}` is not a settings field or a token (`$accent`): only those have a \
                 runtime overlay"
            ))
        };
        let (d, field) = self.settings_field(path).ok_or_else(not)?;
        let slot = self.ctx.vm.root.settings(d).ok_or_else(not)?;
        let Some(h) = &slot.handle else {
            return Ok(false);
        };
        let had = h.overlay(&field).is_some();
        h.clear_overlay(&self.rt, &field)?;
        Ok(had)
    }

    /// `strand toggle <path>`: flip an exported boolean `state` or
    /// settings field (a null `bool?` becomes `true`). Its new value; any
    /// other type is refused, naming it.
    pub fn toggle(&self, path: &str) -> Result<bool, Error> {
        let (d, rest) = self.export(path)?;
        let prog = self.ctx.vm.prog.clone();
        let mut ty = prog.def(d).ty.clone();
        for f in &rest {
            ty = match ty.non_null() {
                Ty::Record(r) => prog
                    .types
                    .record(*r)
                    .fields
                    .iter()
                    .find(|x| x.name == *f)
                    .map(|x| x.ty.clone()),
                _ => None,
            }
            .ok_or_else(|| Error::failed(format!("`{path}` has no field `{f}`")))?;
        }
        if !matches!(ty.non_null(), Ty::Prim(Prim::Bool)) {
            return Err(Error::failed(format!(
                "`{path}` is a {}, not a bool: `toggle` flips booleans only",
                prog.types.show(&ty)
            )));
        }
        let on = !self.get(path)?.truthy();
        self.set(path, Value::Bool(on))?;
        Ok(on)
    }
}

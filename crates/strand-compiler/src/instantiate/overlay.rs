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

use std::collections::{BTreeMap, BTreeSet};
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
    /// The file has been read (once, at the first build).
    loaded: bool,
    /// What the file holds, edited in place.
    doc: DocumentMut,
    /// Overlaid paths (no `$`) and their text.
    values: BTreeMap<String, String>,
    /// Each overlaid path's value without the overlay, as the last build
    /// of the table had it (`None`: no such token).
    base: BTreeMap<String, Option<PropValue>>,
    /// The program the last table was built from (its address): a new
    /// one is a committed build, whose changed file values are shadowed.
    program: usize,
    /// Read by the token table's memo, bumped by every change: the table
    /// is built again. A root node of the instance's runtime.
    generation: Option<Signal<u64>>,
    /// Overlay values the token's type does not read, already said.
    bad: BTreeSet<(String, String)>,
    /// Notices for the next update (settings' kind, so the overlay shows
    /// them with `[clear]` and `strand watch` hears them).
    notices: Vec<Diagnostic>,
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
        st.values
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        let committed = st.program != 0 && st.program != program;
        st.program = program;
        // The file's values first: a derived token reads the overlaid
        // ones once they are in.
        let files: Vec<(String, String, Option<PropValue>)> = st
            .values
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
                    if st.bad.insert((path.clone(), text.clone())) {
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
    /// table again.
    fn set(&self, rt: &Runtime, path: &str, text: &str) {
        let mut st = self.0.state();
        self.load(&mut st);
        st.values.insert(path.to_string(), text.to_string());
        // In place where the key is there, so a comment above it stays.
        match st.doc.get_mut(path) {
            Some(item) => *item = toml_value(text),
            None => {
                st.doc.insert(path, toml_value(text));
            }
        }
        self.changed(rt, &mut st);
    }

    /// Drop `path`'s overlay; whether it had one.
    fn clear(&self, rt: &Runtime, path: &str) -> bool {
        let mut st = self.0.state();
        self.load(&mut st);
        if st.values.remove(path).is_none() {
            return false;
        }
        st.doc.as_table_mut().remove(path);
        st.base.remove(path);
        self.changed(rt, &mut st);
        true
    }

    fn changed(&self, rt: &Runtime, st: &mut State) {
        if let Some(g) = st.generation {
            let _ = g.update(rt, |n| *n += 1);
        }
        if let Some(file) = &self.0.file {
            self.0.writer.save(file.clone(), st.doc.to_string());
        }
    }

    /// Read the file, once.
    fn load(&self, st: &mut State) {
        if std::mem::replace(&mut st.loaded, true) {
            return;
        }
        let Some(file) = &self.0.file else {
            return;
        };
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                st.notices.push(
                    self.0
                        .notice(None, SettingsIssue::Unreadable(Arc::from(e.to_string()))),
                );
                return;
            }
        };
        let doc: DocumentMut = match text.parse() {
            Ok(d) => d,
            Err(e) => {
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
            let text = match item.as_value() {
                Some(v) => match (v.as_str(), v.as_integer(), v.as_float(), v.as_bool()) {
                    (Some(s), ..) => s.to_string(),
                    (_, Some(i), ..) => i.to_string(),
                    (_, _, Some(f), _) => f.to_string(),
                    (.., Some(b)) => b.to_string(),
                    _ => continue,
                },
                None => continue,
            };
            st.values.insert(k.to_string(), text);
        }
        st.doc = doc;
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
            let names = table.tokens.keys().chain(table.derived.keys());
            let hint = crate::diagnostic::suggest(path, names.map(String::as_str))
                .map(|s| format!("; did you mean `${s}`?"))
                .unwrap_or_default();
            return Err(Error::failed(format!("no token `${path}`{hint}")));
        };
        read(&self.ctx.vm.prog.types, &current, text)
            .map_err(|why| Error::failed(format!("`${path}`: {why}")))?;
        self.ctx.storage.tokens.set(&self.rt, path, text.trim());
        Ok(())
    }

    /// `strand clear '$accent'` and the notice's `[clear]`: drop the
    /// token overlay at `path` (no `$`), so the theme's value applies
    /// again. Whether there was one.
    pub fn clear_token_overlay(&self, path: &str) -> bool {
        self.ctx.storage.tokens.clear(&self.rt, path)
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
        (*self.ctx.storage.tokens.0.display() == *file).then(|| self.clear_token_overlay(field))
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

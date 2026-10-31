//! (M5, S-tools) The theming hub's work on the logic thread after each
//! tick's diff is sent (decisions.md m5-tools; architecture.md, "The
//! theming hub"): `file` outputs, and `export` requests waiting for their
//! other mode.
//!
//! The instance renders each changed output (`Instance::take_outputs`);
//! the bytes go to the file output writer thread, `strand-files`, so
//! logic never waits on a disk or a child process. That thread writes
//! atomically (temp file and rename, following symlinks, keeping the
//! mode), skips a write whose bytes the file already holds (boot's
//! catch-up), keeps a manifest of what Strand wrote in
//! `$XDG_STATE_HOME/strand/files` and refuses a target Strand did not
//! write, a `.strand` file, a read-only one and a link to a missing
//! file. A refused or failed write is tried again (after 1 s, doubling
//! to 10 s) until it lands or newer content replaces it, so deleting a
//! refused file, or `[replace]` on its notice, lets Strand write it. It
//! runs `run:` 200 ms after a file's last write, one run per file with
//! one queued, killing its process group after 10 s; a run's stderr is
//! read only while it runs (and a moment after), so a process it leaves
//! in the background never holds the writer. Failures come back in a
//! shared list (the loop is pinged awake) and become an overlay notice and a
//! `strand watch` notice; the last good file stays.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;
use strand_compiler::diagnostic::{Style, render, render_short};
use strand_compiler::instantiate::{OutputChange, OutputContent};
use strand_compiler::source::SourceMap;

use super::logic::OwnWrites;
use super::shell::Shell;
use crate::overlay;

/// `run:` follows a file's last write by this much.
const RUN_DEBOUNCE: Duration = Duration::from_millis(200);
/// A `run:` still going after this is killed with its process group.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of a failing run's stderr is kept.
const STDERR_TAIL: usize = 4096;
/// After a run ends, how long its stderr is still read: a process it
/// left in the background keeps the pipe open, and is not waited for.
const STDERR_GRACE: Duration = Duration::from_millis(100);
/// A refused or failed write is tried again after this, doubling.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two tries of a write.
const RETRY_MAX: Duration = Duration::from_secs(10);

/// What logic asks of the writer.
enum Job {
    Write {
        path: PathBuf,
        bytes: Vec<u8>,
        run: Option<Vec<String>>,
    },
    /// A run of `path`'s command ended (from its waiting thread).
    Ran {
        path: PathBuf,
        failed: Option<String>,
    },
    /// The user's `[replace]`: `path` is Strand's now, written at once.
    Replace(PathBuf),
    /// The paths the mounted `file`s write now: a write still being
    /// tried anywhere else is dropped (a removed `file`, a changed path,
    /// a failing render); a `run:` due for bytes already written still
    /// runs.
    Keep(Vec<PathBuf>),
    /// The shell is gone: the thread ends.
    Stop,
}

/// What the writer tells logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// `path` holds what Strand last rendered for it.
    Written(PathBuf),
    /// It was not written, or its `run:` failed: why.
    Failed(PathBuf, String),
    /// It was not written because Strand did not write the file there:
    /// why, with a `[replace]` on its notice.
    Unowned(PathBuf, String),
}

/// The writer's job queue and what it tells logic, shared by logic, the
/// writer and its runs' threads (one lock each, no channel: the writer
/// waits on the condvar, logic is pinged and drains the outcomes).
#[derive(Default)]
struct Shared {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    outcomes: Mutex<Vec<Outcome>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn push(&self, job: Job) {
        lock(&self.jobs).push_back(job);
        self.wake.notify_one();
    }

    /// The next job, waiting for one until `until` (`None`: as long as
    /// it takes); `None` once `until` passes.
    fn pop(&self, until: Option<Instant>) -> Option<Job> {
        let mut q = lock(&self.jobs);
        loop {
            if let Some(j) = q.pop_front() {
                return Some(j);
            }
            q = match until {
                None => self.wake.wait(q).unwrap_or_else(PoisonError::into_inner),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    match self.wake.wait_timeout(q, left) {
                        Ok((q, _)) => q,
                        Err(e) => e.into_inner().0,
                    }
                }
            };
        }
    }
}

/// The value kept for `path` in a writer's small per-path lists.
fn entry<'a, T>(v: &'a mut [(PathBuf, T)], path: &Path) -> Option<&'a mut T> {
    v.iter_mut().find(|(p, _)| p == path).map(|(_, t)| t)
}

/// Takes `path`'s value out of the list.
fn take<T>(v: &mut Vec<(PathBuf, T)>, path: &Path) -> Option<T> {
    let i = v.iter().position(|(p, _)| p == path)?;
    Some(v.swap_remove(i).1)
}

/// The writer thread, started with the first output, ended when this
/// is dropped (with the shell).
pub(super) struct Files {
    ping: calloop::ping::Ping,
    manifest: Option<PathBuf>,
    own_writes: Option<OwnWrites>,
    /// The writer's queue and outcomes, once it is started.
    shared: Option<Arc<Shared>>,
    /// The targets last sent as [`Job::Keep`].
    targets: Vec<PathBuf>,
    /// Each output's standing notice (until its next write lands), so a
    /// `strand watch` started after it still hears it.
    notices: Vec<(PathBuf, String)>,
}

impl Drop for Files {
    fn drop(&mut self) {
        if let Some(s) = &self.shared {
            s.push(Job::Stop);
        }
    }
}

impl Files {
    /// `manifest`: `$XDG_STATE_HOME/strand/files` (none: nothing is
    /// owned across restarts); `own_writes`: registers each write with
    /// the config watcher before it lands.
    pub(super) fn new(
        ping: calloop::ping::Ping,
        manifest: Option<PathBuf>,
        own_writes: Option<OwnWrites>,
    ) -> Files {
        Files {
            ping,
            manifest,
            own_writes,
            shared: None,
            targets: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// Tells the writer the paths `inst`'s outputs write now, when they
    /// changed since last told.
    fn keep(&mut self, inst: &strand_compiler::instantiate::Instance) {
        if self.shared.is_none() {
            return;
        }
        let (mut count, mut same) = (0, true);
        inst.output_targets(|p| {
            count += 1;
            same &= self.targets.iter().any(|t| t == p);
        });
        if same && count == self.targets.len() {
            return;
        }
        let mut now = Vec::with_capacity(count);
        inst.output_targets(|p| now.push(p.to_path_buf()));
        self.targets.clone_from(&now);
        self.send(Job::Keep(now));
    }

    fn send(&mut self, job: Job) -> bool {
        let shared = match &self.shared {
            Some(s) => s,
            None => {
                let shared = Arc::new(Shared::default());
                let mut w = Writer::new(self.manifest.clone(), shared.clone(), self.ping.clone());
                w.own_writes = self.own_writes.take();
                let spawned = std::thread::Builder::new()
                    .name("strand-files".into())
                    .spawn(move || w.run());
                if let Err(e) = spawned {
                    log::warn!("file outputs are not written: no writer thread: {e}");
                    return false;
                }
                self.shared.insert(shared)
            }
        };
        shared.push(job);
        true
    }

    /// A notice's `[replace]`: `path` becomes Strand's and is written.
    pub(super) fn replace(&mut self, path: PathBuf) {
        if self.shared.is_none() || !self.send(Job::Replace(path)) {
            log::warn!("file outputs: nothing to replace (no writer)");
        }
    }

    /// The outputs' standing notices, for a new `strand watch`.
    pub(super) fn notices(&self) -> impl Iterator<Item = &String> {
        self.notices.iter().map(|(_, n)| n)
    }

    fn outcomes(&self) -> Vec<Outcome> {
        self.shared
            .as_ref()
            .map(|s| std::mem::take(&mut *lock(&s.outcomes)))
            .unwrap_or_default()
    }
}

impl Shell {
    /// After the tick's diff is sent.
    pub(super) fn after_diff(&mut self) {
        self.retry_exports();
        for change in self.inst.take_outputs() {
            self.output(change);
        }
        self.files.keep(&self.inst);
        for o in self.files.outcomes() {
            match o {
                Outcome::Written(path) => self.output_ok(&path),
                Outcome::Failed(path, why) => self.output_notice(&path, &why, false),
                Outcome::Unowned(path, why) => self.output_notice(&path, &why, true),
            }
        }
    }

    fn output(&mut self, c: OutputChange) {
        match c.content {
            OutputContent::Write { bytes, run } => {
                let Some(path) = c.path else { return };
                if !self.files.send(Job::Write { path, bytes, run }) {
                    log::warn!("file outputs: the writer thread is gone");
                }
            }
            OutputContent::Failed { diags, template } => {
                let (short, long) = match &template {
                    Some((name, text)) => {
                        let (map, _) = SourceMap::single(name.clone(), text.clone());
                        (
                            render_short(&diags, &map),
                            render(&diags, &map, Style::Plain),
                        )
                    }
                    None => {
                        let map = &self.build.sources;
                        (render_short(&diags, map), render(&diags, map, Style::Plain))
                    }
                };
                log::warn!("{long}");
                let path = c.path.unwrap_or_default();
                self.output_notice(&path, short.trim_end(), false);
            }
        }
    }

    /// A write landed: its earlier notices go.
    fn output_ok(&mut self, path: &Path) {
        self.overlay.forget_service(&cell(path), &self.inst);
        let _ = take(&mut self.files.notices, path);
    }

    /// `path` was not written (or its `run:` failed): an overlay notice
    /// and a `strand watch` notice; `replace`: the notice offers
    /// `[replace]` (an unowned target).
    fn output_notice(&mut self, path: &Path, why: &str, replace: bool) {
        let text = format!("file {}: {why}", path.display());
        log::warn!("{text}");
        let _ = take(&mut self.files.notices, path);
        self.files.notices.push((path.to_path_buf(), text.clone()));
        self.overlay.forget_service(&cell(path), &self.inst);
        let rows = match replace {
            true => overlay::replace_lines(&cell(path), &text, path),
            false => overlay::service_lines(&cell(path), &text),
        };
        self.overlay.note(rows, Instant::now(), &self.inst);
        if self.server.is_some() || self.event_sink.is_some() {
            let ev = json!({"event": "notices", "kept_over_default": [], "notices": [text]});
            if let Some(s) = &mut self.server {
                s.broadcast(&ev);
            }
            if let Some(sink) = &self.event_sink {
                sink.send(&ev);
            }
        }
    }
}

/// The overlay's key for one output's notices.
fn cell(path: &Path) -> String {
    format!("files:{}", path.display())
}

/// A file's `run:`: when it is due, whether one is running, and whether
/// another is queued behind it.
#[derive(Default)]
struct RunState {
    argv: Vec<String>,
    due: Option<Instant>,
    running: bool,
    queued: bool,
}

/// A write that did not land, tried again at `at`.
struct Retry {
    bytes: Vec<u8>,
    run: Option<Vec<String>>,
    at: Instant,
    /// The wait before the next try after this one fails.
    wait: Duration,
    /// Why the last try failed (a notice is sent only when it changes).
    why: String,
}

/// The `strand-files` thread.
struct Writer {
    manifest_file: Option<PathBuf>,
    /// Path → FNV-1a hash of what Strand last wrote there.
    owned: BTreeMap<PathBuf, u64>,
    shared: Arc<Shared>,
    ping: calloop::ping::Ping,
    /// Per path (a handful of files: lists, not maps).
    runs: Vec<(PathBuf, RunState)>,
    retries: Vec<(PathBuf, Retry)>,
    /// How long `run:` may take ([`RUN_TIMEOUT`]; tests shorten it).
    timeout: Duration,
    /// Registers a write with the config watcher before it lands.
    own_writes: Option<OwnWrites>,
}

impl Writer {
    fn new(
        manifest_file: Option<PathBuf>,
        shared: Arc<Shared>,
        ping: calloop::ping::Ping,
    ) -> Writer {
        let owned = manifest_file
            .as_deref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| parse_manifest(&t))
            .unwrap_or_default();
        Writer {
            manifest_file,
            owned,
            shared,
            ping,
            runs: Vec::new(),
            retries: Vec::new(),
            timeout: RUN_TIMEOUT,
            own_writes: None,
        }
    }

    fn tell(&self, o: Outcome) {
        lock(&self.shared.outcomes).push(o);
        self.ping.ping();
    }

    fn run(mut self) {
        loop {
            let next = self
                .runs
                .iter()
                .filter_map(|(_, r)| r.due)
                .chain(self.retries.iter().map(|(_, r)| r.at))
                .min();
            match self.shared.pop(next) {
                Some(Job::Write { path, bytes, run }) => {
                    // Newer content replaces a write still being retried.
                    let last = take(&mut self.retries, &path).map(|r| r.why);
                    self.write(path, bytes, run, last);
                }
                Some(Job::Replace(path)) => {
                    // The file it names, a link followed: what is written.
                    match real(&path) {
                        Ok(target) if is_strand(&target) => {}
                        Ok(target) => self.own(&target, 0),
                        Err(why) => log::warn!("file {}: {why}", path.display()),
                    }
                    if let Some(r) = entry(&mut self.retries, &path) {
                        r.at = Instant::now();
                    }
                }
                Some(Job::Keep(paths)) => self.keep(&paths),
                Some(Job::Stop) => return,
                Some(Job::Ran { path, failed }) => {
                    if let Some(why) = failed {
                        self.tell(Outcome::Failed(path.clone(), why));
                    }
                    if let Some(r) = entry(&mut self.runs, &path) {
                        r.running = false;
                        if std::mem::take(&mut r.queued) {
                            r.due = Some(Instant::now());
                        }
                    }
                }
                None => {}
            }
            self.retry_due();
            self.start_due();
        }
    }

    /// Writes `bytes` to `path`, scheduling its `run:` when it changed
    /// the file; a write that fails is kept to be tried again. `last`:
    /// why the previous try of this path failed (no new notice for the
    /// same reason).
    fn write(
        &mut self,
        path: PathBuf,
        bytes: Vec<u8>,
        run: Option<Vec<String>>,
        last: Option<String>,
    ) -> Option<Duration> {
        match self.write_file(&path, &bytes) {
            Ok(changed) => {
                self.tell(Outcome::Written(path.clone()));
                if changed && let Some(argv) = run.filter(|a| !a.is_empty()) {
                    let due = Some(Instant::now() + RUN_DEBOUNCE);
                    match entry(&mut self.runs, &path) {
                        Some(r) => {
                            r.argv = argv;
                            r.due = due;
                        }
                        None => self.runs.push((
                            path,
                            RunState {
                                argv,
                                due,
                                ..RunState::default()
                            },
                        )),
                    }
                }
                None
            }
            Err(why) => {
                if last.as_deref() != Some(why.as_str()) {
                    self.tell(match why.starts_with(UNOWNED) {
                        true => Outcome::Unowned(path.clone(), why.clone()),
                        false => Outcome::Failed(path.clone(), why.clone()),
                    });
                }
                // A `.strand` target never becomes writable.
                if why.starts_with(STRAND_REFUSED) {
                    return None;
                }
                let _ = take(&mut self.retries, &path);
                self.retries.push((
                    path,
                    Retry {
                        bytes,
                        run,
                        at: Instant::now() + RETRY_FIRST,
                        wait: RETRY_FIRST,
                        why,
                    },
                ));
                Some(RETRY_FIRST)
            }
        }
    }

    /// Lets go of the unwritten bytes of paths no `file` writes now (a
    /// retry is never written behind the output's notice). A `run:` due
    /// or queued there still runs: it follows bytes already on disk, and
    /// the consumer would otherwise stay out of step with the file
    /// (review round 3, item 2). Only idle run state is dropped.
    fn keep(&mut self, paths: &[PathBuf]) {
        self.retries.retain(|(p, _)| paths.contains(p));
        self.runs
            .retain(|(p, r)| paths.contains(p) || r.due.is_some() || r.running || r.queued);
    }

    /// Tries again the writes whose time has come, waiting twice as
    /// long (at most [`RETRY_MAX`]) after another failure.
    fn retry_due(&mut self) {
        let now = Instant::now();
        let due: Vec<PathBuf> = self
            .retries
            .iter()
            .filter(|(_, r)| r.at <= now)
            .map(|(p, _)| p.clone())
            .collect();
        for path in due {
            let Some(r) = take(&mut self.retries, &path) else {
                continue;
            };
            let wait = (r.wait * 2).min(RETRY_MAX);
            if self
                .write(path.clone(), r.bytes, r.run, Some(r.why))
                .is_some()
                && let Some(again) = entry(&mut self.retries, &path)
            {
                again.wait = wait;
                again.at = Instant::now() + wait;
            }
        }
    }

    /// Writes `bytes` to `path` unless it holds them already: whether it
    /// wrote.
    ///
    /// Everything is decided on the file written, not the name given: a
    /// link is followed (the link kept), and its target must be Strand's
    /// and not a `.strand` file, so a path Strand once wrote that is now
    /// a link into the user's dotfiles is refused like any file Strand
    /// did not write. The manifest names the files written, links
    /// followed.
    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<bool, String> {
        let target = real(path)?;
        if is_strand(path) || is_strand(&target) {
            return Err(format!(
                "{STRAND_REFUSED}{}",
                match is_strand(path) {
                    true => String::new(),
                    false => format!(" (it is a link to {})", target.display()),
                }
            ));
        }
        let hash = fnv(bytes);
        let meta = std::fs::metadata(&target).ok();
        if let Some(m) = &meta {
            if !m.is_file() {
                return Err("refused: it is not a regular file".into());
            }
            // Already what it should hold (boot's catch-up, an unchanged
            // flip): nothing is written, and the file is Strand's.
            if std::fs::read(&target).is_ok_and(|b| b == bytes) {
                self.own(&target, hash);
                return Ok(false);
            }
            if !self.owned.contains_key(&target) {
                return Err(format!(
                    "{UNOWNED}; [replace] it, or move it away (or delete it), and Strand \
                     writes it"
                ));
            }
            if m.permissions().readonly() {
                return Err("refused: the file is read-only".into());
            }
        }
        if let Some(hook) = &self.own_writes {
            (hook.0)(&target, bytes);
            if target != path {
                (hook.0)(path, bytes);
            }
        }
        write_atomic(&target, bytes, meta.as_ref()).map_err(|e| format!("not written: {e}"))?;
        self.own(&target, hash);
        Ok(true)
    }

    /// Records `path` as Strand's, with the hash of what it holds.
    fn own(&mut self, path: &Path, hash: u64) {
        if self.owned.get(path) == Some(&hash) {
            return;
        }
        self.owned.insert(path.to_path_buf(), hash);
        if let Some(f) = &self.manifest_file {
            let text = manifest_text(&self.owned);
            if let Err(e) = write_atomic(f, text.as_bytes(), None) {
                log::warn!("file outputs: manifest {}: {e}", f.display());
            }
        }
    }

    fn start_due(&mut self) {
        let now = Instant::now();
        let due: Vec<PathBuf> = self
            .runs
            .iter()
            .filter(|(_, r)| r.due.is_some_and(|d| d <= now))
            .map(|(p, _)| p.clone())
            .collect();
        for path in due {
            let Some(r) = entry(&mut self.runs, &path) else {
                continue;
            };
            r.due = None;
            if r.running {
                r.queued = true;
                continue;
            }
            r.running = true;
            let argv = r.argv.clone();
            let back = self.shared.clone();
            let timeout = self.timeout;
            let ran = path.clone();
            let spawned = std::thread::Builder::new()
                .name("strand-files-run".into())
                .spawn(move || {
                    let failed = run_once(&argv, timeout);
                    back.push(Job::Ran { path: ran, failed });
                });
            if let Err(e) = spawned {
                log::warn!("file outputs: no thread for `run:`: {e}");
                r.running = false;
            }
        }
    }
}

/// The start of an unowned target's refusal.
const UNOWNED: &str = "refused: Strand did not write this file";
/// A `.strand` target's refusal (never tried again).
const STRAND_REFUSED: &str = "refused: Strand never writes a `.strand` file";

fn is_strand(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "strand")
}

/// The file `path` names: a link followed (one to nothing is refused,
/// never replaced by a file), a file's own links in its directories
/// resolved, and for a file not there yet its nearest existing
/// directory's, so one file has one name in the manifest however it is
/// reached.
fn real(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path).map_err(|e| {
            format!(
                "refused: it is a link to a missing file ({e}); Strand writes through a \
                 link, never over it"
            )
        }),
        Ok(_) => Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())),
        Err(_) => {
            let mut rest = Vec::new();
            let mut at = path;
            while let (Some(dir), Some(name)) = (at.parent(), at.file_name()) {
                rest.push(name);
                if let Ok(d) = std::fs::canonicalize(dir) {
                    return Ok(rest.iter().rev().fold(d, |p, n| p.join(n)));
                }
                at = dir;
            }
            Ok(path.to_path_buf())
        }
    }
}

/// Runs `argv` in a process group of its own, at most `timeout`: why it
/// failed, with the last [`STDERR_TAIL`] bytes of its stderr. Stderr is
/// read without blocking while the command runs and for
/// [`STDERR_GRACE`] after it ends, never to its end of file: a process
/// the command backgrounds (`dunst &`), or one that left the group with
/// `setsid`, inherits the pipe and may hold it open for good. (M5) Like
/// every program strand starts, it does not inherit the xdg-activation
/// token strand was started with, nor the variables that address this
/// shell (`child::child_env`), and it gets back the transparent huge
/// page setting strand inherited (`child::restore_in_child`): a daemon a
/// `run:` restarts would otherwise run with THP off for life.
fn run_once(argv: &[String], timeout: Duration) -> Option<String> {
    let (prog, args) = argv.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    strand_services::child::child_env(&mut cmd, None);
    // SAFETY: `restore_in_child` makes only async-signal-safe calls, as
    // `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            strand_services::child::restore_in_child();
            Ok(())
        });
    }
    let child = cmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Some(format!("`run:` could not start `{prog}`: {e}")),
    };
    let mut err = child.stderr.take().filter(|e| nonblocking(e.as_raw_fd()));
    let mut keep: Vec<u8> = Vec::new();
    let deadline = Instant::now() + timeout;
    let status = loop {
        drain(&mut err, &mut keep);
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    if status.is_none()
        && let Ok(pid) = i32::try_from(child.id())
    {
        // SAFETY: `kill` takes plain integers; a negative pid names the
        // process group the child leads (`process_group(0)`).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    // What it wrote before it ended, up to its end of file or the grace.
    let grace = Instant::now() + STDERR_GRACE;
    while !drain(&mut err, &mut keep) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(err);
    let Some(status) = status else {
        return Some(format!(
            "`run:` `{prog}` took more than {} s and was killed",
            timeout.as_secs_f32()
        ));
    };
    if status.success() {
        return None;
    }
    let mut why = format!("`run:` `{prog}` failed ({status})");
    let stderr = String::from_utf8_lossy(&keep);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        why.push_str(": ");
        why.push_str(stderr);
    }
    Some(why)
}

/// Sets `fd` non-blocking: whether it is (else stderr is not read).
fn nonblocking(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: `fcntl` with F_GETFL/F_SETFL on a descriptor this process
    // owns (the child's stderr pipe) reads and sets its flags only.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
    }
}

/// Reads what `err` holds now into `keep` (its last [`STDERR_TAIL`]
/// bytes): whether it is at its end (or gone).
fn drain(err: &mut Option<std::process::ChildStderr>, keep: &mut Vec<u8>) -> bool {
    let Some(e) = err.as_mut() else {
        return true;
    };
    let mut buf = [0u8; 1024];
    loop {
        match e.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                keep.extend_from_slice(&buf[..n]);
                if keep.len() > STDERR_TAIL {
                    keep.drain(..keep.len() - STDERR_TAIL);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return e.kind() != std::io::ErrorKind::WouldBlock,
        }
    }
}

/// Writes `bytes` to `path` through a temporary file beside it and a
/// rename, with `meta`'s mode when the file was there.
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    meta: Option<&std::fs::Metadata>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map_or_else(|| "out".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.strand-tmp.{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(m) = meta {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Writes `bytes` to `path` as a `file` element writes its target, with
/// no question of ownership (`strand export -o`, which the user named):
/// a link is followed and kept (a dangling one refused), the file
/// replaced atomically with its mode kept. Errors are worded as the
/// element's notices are.
pub(crate) fn write_through(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = real(path)?;
    let meta = std::fs::metadata(&target).ok();
    if meta.as_ref().is_some_and(|m| !m.is_file()) {
        return Err("refused: it is not a regular file".into());
    }
    write_atomic(&target, bytes, meta.as_ref()).map_err(|e| format!("not written: {e}"))
}

/// FNV-1a, 64 bits: stable across builds, which the manifest needs.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The manifest: one `<hash> <path>` line per file Strand wrote.
fn manifest_text(owned: &BTreeMap<PathBuf, u64>) -> String {
    let mut s = String::from("# files strand wrote (its `file` outputs)\n");
    for (p, h) in owned {
        s.push_str(&format!("{h:016x} {}\n", p.display()));
    }
    s
}

fn parse_manifest(text: &str) -> BTreeMap<PathBuf, u64> {
    // Inserted one by one: collecting would sort, a sort compiled for
    // this one pair type.
    let mut owned = BTreeMap::new();
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        if let Some((h, p)) = l.split_once(' ')
            && let Ok(h) = u64::from_str_radix(h, 16)
        {
            owned.insert(PathBuf::from(p), h);
        }
    }
    owned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's ends of the queue: send as logic does, receive the
    /// outcomes in order.
    impl Shared {
        fn send(&self, job: Job) -> Result<(), ()> {
            self.push(job);
            Ok(())
        }

        fn try_recv(&self) -> Result<Outcome, ()> {
            let mut o = lock(&self.outcomes);
            match o.is_empty() {
                true => Err(()),
                false => Ok(o.remove(0)),
            }
        }

        fn recv_timeout(&self, wait: Duration) -> Result<Outcome, ()> {
            let until = Instant::now() + wait;
            loop {
                if let Ok(o) = self.try_recv() {
                    return Ok(o);
                }
                if Instant::now() >= until {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn drain(&self) -> Vec<Outcome> {
            std::mem::take(&mut *lock(&self.outcomes))
        }
    }

    fn writer(dir: &Path) -> (Writer, Arc<Shared>) {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let shared = Arc::new(Shared::default());
        (
            Writer::new(Some(dir.join("state/files")), shared.clone(), ping),
            shared,
        )
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strand-files-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file Strand wrote is written again (boot's catch-up over a hand
    /// edit, from the manifest a later run reads), one holding the bytes
    /// already is not, and the mode and a link are kept.
    #[test]
    fn writes_are_atomic_owned_and_caught_up() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp("owned");
        let out = dir.join("out/a.css");
        let (mut w, _rx) = writer(&dir);
        assert_eq!(w.write_file(&out, b"one"), Ok(true));
        assert_eq!(w.write_file(&out, b"one"), Ok(false));
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::write(&out, "hand edit").unwrap();
        // A later run: the manifest says the file is Strand's.
        let (mut w, _rx) = writer(&dir);
        assert_eq!(w.write_file(&out, b"two"), Ok(true));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "two");
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        // Through a link: the target is written, the link stays.
        let link = dir.join("link.css");
        std::os::unix::fs::symlink(&out, &link).unwrap();
        w.own(&link, 0);
        assert_eq!(w.write_file(&link, b"three"), Ok(true));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "three");
        // No temp file is left behind.
        let left: Vec<_> = std::fs::read_dir(dir.join("out"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("strand-tmp"))
            .collect();
        assert!(left.is_empty());
    }

    /// Unowned, read-only and `.strand` targets are refused, the file
    /// left as it was.
    #[test]
    fn unowned_read_only_and_strand_targets_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp("refused");
        let (mut w, _rx) = writer(&dir);
        let mine = dir.join("mine.css");
        std::fs::write(&mine, "the user's").unwrap();
        let why = w.write_file(&mine, b"strand's").unwrap_err();
        assert!(why.contains("did not write"), "{why}");
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "the user's");
        let ro = dir.join("ro.css");
        assert_eq!(w.write_file(&ro, b"a"), Ok(true));
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
        let why = w.write_file(&ro, b"b").unwrap_err();
        assert!(why.contains("read-only"), "{why}");
        let why = w.write_file(&dir.join("x.strand"), b"a").unwrap_err();
        assert!(why.contains(".strand"), "{why}");
    }

    /// `run:` failures carry the tail of stderr.
    #[test]
    fn a_failing_run_says_why() {
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        let why = run_once(&sh("echo boom >&2; exit 3"), RUN_TIMEOUT).unwrap();
        assert!(why.contains("boom") && why.contains("failed"), "{why}");
        assert_eq!(run_once(&["true".into()], RUN_TIMEOUT), None);
        let why = run_once(&["/nonexistent/x".into()], RUN_TIMEOUT).unwrap();
        assert!(why.contains("could not start"), "{why}");
        // Only the last 4 KiB of a long stderr is kept.
        let why = run_once(
            &sh("head -c 20000 /dev/zero | tr '\\0' x >&2; exit 1"),
            RUN_TIMEOUT,
        )
        .unwrap();
        assert!(why.len() < STDERR_TAIL + 200, "{}", why.len());
    }

    /// A `run:` command gets back the THP setting strand inherited, like
    /// every program strand starts: `process_setup!` turned THP off for
    /// this test binary only. The command asks the kernel itself
    /// (`PR_GET_THP_DISABLE`, through Python's ctypes): `THP_enabled:` in
    /// `/proc/<pid>/status` reads 1 for THP off except advised regions
    /// (decisions.md m5-speed-thp), so it cannot tell the two apart.
    #[test]
    fn a_run_gets_back_the_inherited_thp_setting() {
        let parent = format!("/proc/{}/status", std::os::unix::process::parent_id());
        let parent_on = std::fs::read_to_string(parent)
            .ok()
            .and_then(|s| strand_services::child::thp_enabled(&s));
        if parent_on != Some(true) {
            eprintln!("skipped: the test's parent has THP off, or no THP_enabled");
            return;
        }
        if Command::new("python3").arg("-V").output().is_err() {
            eprintln!("skipped: no python3 to ask PR_GET_THP_DISABLE");
            return;
        }
        // SAFETY: `prctl` with integer arguments only.
        let own = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        assert!(own > 0, "THP is off for the test binary: {own}");
        let out = std::env::temp_dir().join(format!("strand-run-thp-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let ask = format!(
            "import ctypes; open({:?}, 'w').write(str(ctypes.CDLL(None).prctl({}, 0, 0, 0, 0)))",
            out.display().to_string(),
            libc::PR_GET_THP_DISABLE
        );
        let argv = vec!["python3".to_string(), "-I".into(), "-c".into(), ask];
        assert_eq!(run_once(&argv, RUN_TIMEOUT), None);
        let got = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        assert_eq!(got.trim(), "0", "the `run:` kept strand's THP off ({own})");
    }

    /// (M5, S-platform) A `run:` command does not inherit the
    /// xdg-activation token strand was started with: run in a copy of
    /// this test binary started with both variables set, a command that
    /// fails when it sees either succeeds.
    #[test]
    fn a_run_never_inherits_strands_activation_token() {
        const CHILD: &str = "STRAND_TEST_RUN_TOKEN_CHILD";
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        let probe = sh(
            "test -z \"${XDG_ACTIVATION_TOKEN+x}${DESKTOP_STARTUP_ID+x}\" || \
             { echo inherited >&2; exit 1; }",
        );
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(
                std::env::var("XDG_ACTIVATION_TOKEN").as_deref(),
                Ok("stale")
            );
            assert_eq!(run_once(&probe, RUN_TIMEOUT), None);
            return;
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::outputs::tests::a_run_never_inherits_strands_activation_token",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("XDG_ACTIVATION_TOKEN", "stale")
            .env("DESKTOP_STARTUP_ID", "stale")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    /// A command that backgrounds a process (`pkill dunst; dunst &`)
    /// ends when the command does: the process holding its stderr open
    /// is not waited for, and the failure still carries what it wrote.
    #[test]
    fn a_run_that_backgrounds_a_process_ends_with_it() {
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        let started = Instant::now();
        assert_eq!(run_once(&sh("sleep 5 & exit 0"), RUN_TIMEOUT), None);
        let why = run_once(&sh("sleep 5 & echo boom >&2; exit 3"), RUN_TIMEOUT).unwrap();
        assert!(why.contains("boom"), "{why}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited for the background process: {:?}",
            started.elapsed()
        );
    }

    /// A run past its timeout is killed with its process group, and one
    /// that left the group (`setsid`) holding stderr does not hold the
    /// writer.
    #[test]
    fn a_run_past_its_timeout_is_killed_with_its_group() {
        let dir = temp("timeout");
        let pid_file = dir.join("bg.pid");
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        let started = Instant::now();
        let why = run_once(
            &sh(&format!(
                "sleep 30 & echo $! > {}; sleep 30",
                pid_file.display()
            )),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(why.contains("was killed"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        let gone = || {
            std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit(')')
                    .next()
                    .is_some_and(|r| r.trim_start().starts_with('Z'))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone() {
            assert!(
                Instant::now() < deadline,
                "the group's background sleep lives on"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        let why = run_once(&sh("setsid sleep 5 & sleep 30"), Duration::from_millis(300)).unwrap();
        assert!(why.contains("was killed"), "{why}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    /// A writer on its own thread, as `Files` starts it: (jobs, outcomes).
    fn spawned(dir: &Path, timeout: Duration) -> (Arc<Shared>, Arc<Shared>) {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let shared = Arc::new(Shared::default());
        let mut w = Writer::new(Some(dir.join("state/files")), shared.clone(), ping);
        w.timeout = timeout;
        std::thread::spawn(move || w.run());
        (shared.clone(), shared)
    }

    fn lines(p: &Path) -> usize {
        std::fs::read_to_string(p).map_or(0, |t| t.lines().count())
    }

    /// Three writes in quick succession run `run:` once, 200 ms after
    /// the last; a write while a run is going queues one re-run, however
    /// many writes come meanwhile.
    #[test]
    fn runs_are_debounced_and_one_rerun_is_queued() {
        let dir = temp("debounce");
        let log = dir.join("runs.log");
        let out = dir.join("out.css");
        let (jobs, _rx) = spawned(&dir, RUN_TIMEOUT);
        let run = |extra: &str| {
            Some(vec![
                "sh".to_string(),
                "-c".into(),
                format!("echo x >> {}{extra}", log.display()),
            ])
        };
        let write = |bytes: &str, extra: &str| {
            jobs.send(Job::Write {
                path: out.clone(),
                bytes: bytes.as_bytes().to_vec(),
                run: run(extra),
            })
            .unwrap();
        };
        let started = Instant::now();
        write("a", "");
        std::thread::sleep(Duration::from_millis(50));
        write("b", "");
        std::thread::sleep(Duration::from_millis(50));
        write("c", "");
        let last = Instant::now();
        while lines(&log) == 0 {
            assert!(started.elapsed() < Duration::from_secs(5), "never ran");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(last.elapsed() >= RUN_DEBOUNCE, "ran before the debounce");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(lines(&log), 1, "three writes, one run");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "c");
        // A slow run: writes during it queue one re-run.
        std::fs::remove_file(&log).unwrap();
        // Waits on the run's own line, not on time: a loaded runner can
        // take longer than the debounce to start `sh` (GitHub run
        // 38304456895). The line is echoed before the run's sleep, so
        // the writes below land while it is going.
        let slow = Instant::now();
        write("d", "; sleep 0.6");
        while lines(&log) == 0 {
            assert!(
                slow.elapsed() < Duration::from_secs(5),
                "the slow run started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        write("e", "; sleep 0.6");
        write("f", "; sleep 0.6");
        let rerun = Instant::now();
        while lines(&log) < 2 {
            assert!(
                rerun.elapsed() < Duration::from_secs(5),
                "one queued re-run"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The re-run's own sleep and a debounce more: no third run.
        std::thread::sleep(Duration::from_millis(600) + RUN_DEBOUNCE * 2);
        assert_eq!(lines(&log), 2, "one queued re-run, however many writes");
    }

    /// A refused write is tried again: once the user deletes the file
    /// the notice names, Strand writes it, and `[replace]` writes over a
    /// file at once. One notice per reason, not one per try.
    #[test]
    fn a_refused_write_is_tried_again() {
        let dir = temp("retry");
        let mine = dir.join("mine.css");
        std::fs::write(&mine, "the user's").unwrap();
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        let send = |bytes: &str| {
            jobs.send(Job::Write {
                path: mine.clone(),
                bytes: bytes.as_bytes().to_vec(),
                run: None,
            })
            .unwrap();
        };
        send("strand's");
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&first, Outcome::Unowned(p, why) if *p == mine && why.contains("[replace]")),
            "{first:?}"
        );
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "the user's");
        assert!(rx.try_recv().is_err(), "one notice for one reason");
        std::fs::remove_file(&mine).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(12)).unwrap(),
            Outcome::Written(mine.clone())
        );
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "strand's");
        // `[replace]`: another unowned file, handed over.
        let theirs = dir.join("theirs.css");
        std::fs::write(&theirs, "theirs").unwrap();
        jobs.send(Job::Write {
            path: theirs.clone(),
            bytes: b"strand's".to_vec(),
            run: None,
        })
        .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Outcome::Unowned(..)
        ));
        jobs.send(Job::Replace(theirs.clone())).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Outcome::Written(theirs.clone())
        );
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "strand's");
    }

    /// A link to a missing file is refused, and stays a link.
    #[test]
    fn a_dangling_link_is_refused_not_replaced() {
        let dir = temp("dangling");
        let link = dir.join("colors.ini");
        std::os::unix::fs::symlink(dir.join("dotfiles/colors.ini"), &link).unwrap();
        let (mut w, _rx) = writer(&dir);
        w.own(&link, 0);
        let why = w.write_file(&link, b"x").unwrap_err();
        assert!(why.contains("link to a missing file"), "{why}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!dir.join("dotfiles/colors.ini").exists());
    }

    /// (Review round 2, item 1) A `file` no longer mounted (or moved,
    /// or failing) stops its updates: its refused write is not tried
    /// again and its `run:` (for bytes that never landed) never runs, even
    /// once the target frees up.
    #[test]
    fn a_removed_file_stops_its_retries() {
        let dir = temp("keep");
        let mine = dir.join("colors.conf");
        let log = dir.join("runs.log");
        std::fs::write(&mine, "the user's").unwrap();
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        jobs.send(Job::Write {
            path: mine.clone(),
            bytes: b"stale".to_vec(),
            run: Some(vec![
                "sh".into(),
                "-c".into(),
                format!("echo x >> {}", log.display()),
            ]),
        })
        .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Outcome::Unowned(..)
        ));
        // The `file` is removed from the config: no target is left.
        jobs.send(Job::Keep(Vec::new())).unwrap();
        std::fs::remove_file(&mine).unwrap();
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!mine.exists(), "the removed `file` was written");
        assert!(!log.exists(), "the removed `file`'s `run:` ran");
        assert!(rx.try_recv().is_err());
    }

    /// (Review round 3, item 2) A `run:` due for bytes already written
    /// still runs when the `file` stops being a target within the
    /// debounce (its render starts failing, it is removed or re-pathed):
    /// the file holds the new bytes, so the consumer must hear of them.
    #[test]
    fn a_landed_writes_run_survives_its_file_going() {
        let dir = temp("keep-run");
        let out = dir.join("colors.css");
        let log = dir.join("runs.log");
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        jobs.send(Job::Write {
            path: out.clone(),
            bytes: b"new".to_vec(),
            run: Some(vec![
                "sh".into(),
                "-c".into(),
                format!("echo x >> {}", log.display()),
            ]),
        })
        .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Outcome::Written(out.clone())
        );
        // Within the debounce the render fails: no target is left.
        jobs.send(Job::Keep(Vec::new())).unwrap();
        let started = Instant::now();
        while lines(&log) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the landed write's `run:` never ran"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lines(&log), 1);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "new");
    }

    /// (Review round 2, item 2) Ownership is the written file's: a path
    /// Strand wrote that is now a link into the user's dotfiles is
    /// refused, and a link named `x.css` to a `.strand` file is refused
    /// as the `.strand` file it writes, `[replace]` or not.
    #[test]
    fn a_link_is_judged_by_the_file_it_names() {
        let dir = temp("link-owner");
        let (mut w, _rx) = writer(&dir);
        let colors = dir.join("colors.ini");
        assert_eq!(w.write_file(&colors, b"strand's"), Ok(true));
        let dotfile = dir.join("dotfiles/colors.ini");
        std::fs::create_dir_all(dotfile.parent().unwrap()).unwrap();
        std::fs::write(&dotfile, "the user's").unwrap();
        std::fs::remove_file(&colors).unwrap();
        std::os::unix::fs::symlink(&dotfile, &colors).unwrap();
        let why = w.write_file(&colors, b"strand's, newer").unwrap_err();
        assert!(why.starts_with(UNOWNED), "{why}");
        assert_eq!(std::fs::read_to_string(&dotfile).unwrap(), "the user's");
        // A link to a `.strand` file, even handed over.
        let theme = dir.join("theme.strand");
        std::fs::write(&theme, "use palette p\n").unwrap();
        let css = dir.join("x.css");
        std::os::unix::fs::symlink(&theme, &css).unwrap();
        let why = w.write_file(&css, b"a").unwrap_err();
        assert!(why.starts_with(STRAND_REFUSED), "{why}");
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        let write = || {
            jobs.send(Job::Write {
                path: css.clone(),
                bytes: b"a".to_vec(),
                run: None,
            })
            .unwrap();
        };
        write();
        jobs.send(Job::Replace(css.clone())).unwrap();
        write();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(&first, Outcome::Failed(_, why) if why.starts_with(STRAND_REFUSED)),
            "{first:?}"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            rx.drain().iter().all(|o| !matches!(o, Outcome::Written(_))),
            "written through the link"
        );
        assert_eq!(std::fs::read_to_string(&theme).unwrap(), "use palette p\n");
    }

    /// (Review round 2, item 4) Dropping `Files` (with the shell) ends
    /// the `strand-files` thread, a refused write still pending or not.
    #[test]
    fn the_writer_ends_with_the_shell() {
        let dir = temp("ends");
        let mine = dir.join("mine.css");
        std::fs::write(&mine, "the user's").unwrap();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut files = Files::new(ping, Some(dir.join("state/files")), None);
        assert!(files.send(Job::Write {
            path: mine.clone(),
            bytes: b"strand's".to_vec(),
            run: None,
        }));
        let shared = files.shared.clone().unwrap();
        assert!(matches!(
            shared.recv_timeout(Duration::from_secs(5)).unwrap(),
            Outcome::Unowned(..)
        ));
        drop(files);
        // The thread's end drops its handle on the queue.
        let until = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&shared) > 1 {
            assert!(Instant::now() < until, "the writer lives on");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// (Review round 2, item 6) Each write is registered with the
    /// watcher before it lands, the link and the file it names both, and
    /// a write that does not happen registers nothing.
    #[test]
    fn writes_are_registered_before_they_land() {
        let dir = temp("own-writes");
        let out = dir.join("a.css");
        type Seen = Vec<(PathBuf, Vec<u8>, bool)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let (mut w, _rx) = writer(&dir);
        let log = seen.clone();
        w.own_writes = Some(OwnWrites(Box::new(move |p, b| {
            // Whether the bytes had landed already.
            let landed = std::fs::read(p).is_ok_and(|on| on == b);
            log.lock()
                .unwrap()
                .push((p.to_path_buf(), b.to_vec(), landed));
        })));
        assert_eq!(w.write_file(&out, b"one"), Ok(true));
        assert_eq!(w.write_file(&out, b"one"), Ok(false));
        let link = dir.join("link.css");
        std::os::unix::fs::symlink(&out, &link).unwrap();
        assert_eq!(w.write_file(&link, b"two"), Ok(true));
        let real = std::fs::canonicalize(&out).unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (real.clone(), b"one".to_vec(), false),
                (real, b"two".to_vec(), false),
                (link, b"two".to_vec(), false),
            ]
        );
    }

    #[test]
    fn the_manifest_round_trips() {
        let mut m = BTreeMap::new();
        m.insert(PathBuf::from("/a b/c.css"), fnv(b"x"));
        m.insert(PathBuf::from("/d"), 7);
        assert_eq!(parse_manifest(&manifest_text(&m)), m);
        assert_ne!(fnv(b"a"), fnv(b"b"));
    }
}

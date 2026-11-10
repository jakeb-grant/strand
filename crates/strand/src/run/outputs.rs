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
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;
use strand_compiler::diagnostic::{Style, render, render_short};
use strand_compiler::instantiate::{OutputChange, OutputContent};
use strand_compiler::source::SourceMap;

use super::keeper::Keeper;
use super::logic::OwnWrites;
use super::shell::Shell;
use crate::overlay;

/// `run:` follows a file's last write by this much.
const RUN_DEBOUNCE: Duration = Duration::from_millis(200);
/// A `run:` still going after this is killed with its process group.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// A refused or failed write is tried again after this, doubling.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two tries of a write.
const RETRY_MAX: Duration = Duration::from_secs(10);
/// (live-files) A path is written at most once in this long: the first
/// write lands at once, the newest content in the window when it ends.
const PACE: Duration = Duration::from_millis(500);
/// (live-files) A write while its file's own `run:` runs, or this soon
/// after it ended, counts as one round of a loop ([`LOOP_ROUNDS`]).
const LOOP_WINDOW: Duration = Duration::from_secs(1);
/// (live-files) After this many rounds in a row, the `run:` is held.
const LOOP_ROUNDS: u32 = 3;
/// (live-files) A file whose text was seen to change on its own is
/// watched for loops again once it is quiet this long.
const OWN_QUIET: Duration = Duration::from_secs(2);
/// (build-ready) A `file` waiting this long for a service that is up but
/// has not answered (a declared `from dbus` service whose daemon is
/// absent, a poll whose first run hangs) is a standing notice naming it.
const SLOW_WAIT: Duration = Duration::from_secs(10);
/// The loop guard's notice.
const LOOP_NOTICE: &str = "its `run:` changes what it is written from: the `run:` is held \
                           until the file is quiet for a second";

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
    /// (live-files, review round 3) The outputs whose content waits for a
    /// service with no run up, with that notice: a write paced from
    /// before (or a held `run:`'s catch-up) that lands meanwhile takes
    /// the notice away as any write does, and it is posted again.
    waiting: Vec<(PathBuf, String)>,
    /// (build-ready) The outputs waiting for services, since when, and
    /// whether the slow-wait notice was given ([`SLOW_WAIT`]).
    slow: Vec<Slow>,
    /// (proc-keeper) Holds `run:`'s pipes past strand's reading of them.
    keeper: Arc<Keeper>,
}

/// (build-ready) One output's wait for its services.
struct Slow {
    path: PathBuf,
    services: Vec<String>,
    since: Instant,
    told: bool,
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
        keeper: Arc<Keeper>,
    ) -> Files {
        Files {
            ping,
            manifest,
            own_writes,
            shared: None,
            targets: Vec::new(),
            notices: Vec::new(),
            waiting: Vec::new(),
            slow: Vec::new(),
            keeper,
        }
    }

    /// (build-ready) `path` waits for `services` from `now` (`None`: it
    /// no longer waits). A wait for the same first service keeps its
    /// start; one for another starts again.
    fn set_slow(&mut self, path: &Path, services: Option<Vec<String>>, now: Instant) {
        match services {
            None => self.slow.retain(|w| w.path != path),
            Some(services) => match self.slow.iter_mut().find(|w| w.path == path) {
                Some(w) => {
                    if w.services.first() != services.first() {
                        (w.since, w.told) = (now, false);
                    }
                    w.services = services;
                }
                None => self.slow.push(Slow {
                    path: path.to_path_buf(),
                    services,
                    since: now,
                    told: false,
                }),
            },
        }
    }

    /// (build-ready) The waits past [`SLOW_WAIT`] not told yet, each with
    /// its notice, marked told; one that already has a no-run-up notice
    /// is not told again.
    fn slow_due(&mut self, now: Instant) -> Vec<(PathBuf, String)> {
        let mut due = Vec::new();
        for w in &mut self.slow {
            if w.told || now.saturating_duration_since(w.since) < SLOW_WAIT {
                continue;
            }
            w.told = true;
            if self.waiting.iter().any(|(p, _)| *p == w.path) {
                continue;
            }
            let Some(first) = w.services.first() else {
                continue;
            };
            due.push((
                w.path.clone(),
                format!(
                    "waits for `{first}`, which has not answered in {} s (a `from dbus` \
                     service waits for its daemon); nothing is written until it answers",
                    SLOW_WAIT.as_secs()
                ),
            ));
        }
        due
    }

    /// (build-ready) When the next slow-wait notice is due.
    pub(super) fn wait(&self, now: Instant) -> Option<Duration> {
        self.slow
            .iter()
            .filter(|w| !w.told)
            .map(|w| (w.since + SLOW_WAIT).saturating_duration_since(now))
            .min()
    }

    /// (live-files, review round 3) `path`'s content now waits, with
    /// `notice` when a service it waits for has no run up; `None`
    /// (content to write, a failure, `text: null`) when it does not wait
    /// on one that is down.
    fn set_waiting(&mut self, path: &Path, notice: Option<String>) {
        self.waiting.retain(|(p, _)| p != path);
        if let Some(n) = notice {
            self.waiting.push((path.to_path_buf(), n));
        }
    }

    /// The waiting notice `path` still has, once a write to it landed.
    fn still_waiting(&self, path: &Path) -> Option<&str> {
        self.waiting
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, n)| n.as_str())
    }

    /// Tells the writer the paths `inst`'s outputs write now, when they
    /// changed since last told. (build-ready review) The waits of paths
    /// no output writes any more (a reload removed the `file` or moved
    /// it) go, writer or not; those paths are returned, for their notices
    /// to go too.
    fn keep(&mut self, inst: &strand_compiler::instantiate::Instance) -> Vec<PathBuf> {
        let gone = self.let_go(|each| inst.output_targets(each));
        if self.shared.is_none() {
            return gone;
        }
        let (mut count, mut same) = (0, true);
        inst.output_targets(|p| {
            count += 1;
            same &= self.targets.iter().any(|t| t == p);
        });
        if same && count == self.targets.len() {
            return gone;
        }
        let mut now = Vec::with_capacity(count);
        inst.output_targets(|p| now.push(p.to_path_buf()));
        self.targets.clone_from(&now);
        self.send(Job::Keep(now));
        gone
    }

    /// (build-ready review) Forgets the waits (slow, and with no run up)
    /// of the paths `targets` no longer lists: no write will ever land
    /// there to end them, so a slow-wait notice would come for a `file`
    /// that is gone. The paths forgotten, once each.
    fn let_go(&mut self, targets: impl FnOnce(&mut dyn FnMut(&Path))) -> Vec<PathBuf> {
        if self.slow.is_empty() && self.waiting.is_empty() {
            return Vec::new();
        }
        let mut now: Vec<PathBuf> = Vec::new();
        targets(&mut |p| now.push(p.to_path_buf()));
        let mut gone: Vec<PathBuf> = Vec::new();
        let mut forget = |p: &Path| {
            let kept = now.iter().any(|t| t == p);
            if !kept && !gone.iter().any(|g| g == p) {
                gone.push(p.to_path_buf());
            }
            kept
        };
        self.slow.retain(|w| forget(&w.path));
        self.waiting.retain(|(p, _)| forget(p));
        gone
    }

    fn send(&mut self, job: Job) -> bool {
        let shared = match &self.shared {
            Some(s) => s,
            None => {
                let shared = Arc::new(Shared::default());
                let mut w = Writer::new(self.manifest.clone(), shared.clone(), self.ping.clone());
                w.own_writes = self.own_writes.take();
                w.keeper = self.keeper.clone();
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
        // (build-ready review) A `file` a reload took away while it
        // waited: its notice goes with it.
        for path in self.files.keep(&self.inst) {
            self.output_ok(&path);
        }
        for (path, why) in self.files.slow_due(Instant::now()) {
            self.output_notice(&path, &why, false);
        }
        for o in self.files.outcomes() {
            match o {
                Outcome::Written(path) => {
                    self.output_ok(&path);
                    // An earlier write landed while the content waits.
                    if let Some(why) = self.files.still_waiting(&path).map(str::to_string) {
                        self.output_notice(&path, &why, false);
                    } else if let Some(w) = self.files.slow.iter_mut().find(|w| w.path == path) {
                        // A slow wait's notice is given again when due.
                        w.told = false;
                    }
                }
                Outcome::Failed(path, why) => self.output_notice(&path, &why, false),
                Outcome::Unowned(path, why) => self.output_notice(&path, &why, true),
            }
        }
    }

    fn output(&mut self, c: OutputChange) {
        if let Some(p) = &c.path
            && !matches!(c.content, OutputContent::Waiting { .. })
        {
            self.files.set_waiting(p, None);
            self.files.set_slow(p, None, Instant::now());
        }
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
            // (live-files) `text: null`: the file is left as it is, and
            // what was still being tried there goes with its notice (the
            // path is no longer a target, so the writer lets go of it).
            OutputContent::Leave => {
                if let Some(path) = c.path {
                    self.output_ok(&path);
                }
            }
            // (live-files) It waits for a service to answer. Starting is
            // no news; a service with no run up (failed, backing off) is
            // a standing notice, until the next write lands.
            OutputContent::Waiting { services } => {
                let Some(path) = c.path else { return };
                self.files
                    .set_slow(&path, Some(services.clone()), Instant::now());
                let Some(r) = &self.real else { return };
                let snap = r.services.snapshot(self.inst.runtime());
                let why = waiting_notice(&services, &snap);
                self.files.set_waiting(&path, why.clone());
                if let Some(why) = why {
                    self.output_notice(&path, &why, false);
                }
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

/// (live-files) A waiting `file`'s notice: only when a service it waits
/// for has no run up (failed, backing off); one starting is no news.
fn waiting_notice(
    services: &[String],
    snap: &[strand_services::ServiceSnapshot],
) -> Option<String> {
    let down = services
        .iter()
        .find(|s| snap.iter().any(|m| m.name == **s && !m.running))?;
    Some(format!(
        "waits for `{down}`, which has no run up (its own notice says why); nothing is \
         written until it answers"
    ))
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
    /// (live-files) The loop guard: when its last run ended, how many
    /// writes in a row came within [`LOOP_WINDOW`] of a run ending, and,
    /// while the `run:` is held, when the hold ends (a second after the
    /// last write).
    ended: Option<Instant>,
    rounds: u32,
    held_until: Option<Instant>,
    /// A write landed while held: the hold's end runs `run:` once, so the
    /// program reads the file's last content.
    skipped: bool,
    /// That catch-up run is the last chance: a round after it holds
    /// again, and that hold ends without a run.
    probation: bool,
    stuck: bool,
    /// (review round 1) A write came while held, with no run near it:
    /// the text changes on its own (a player's position, once a second),
    /// so its writes are no rounds until the file is quiet for
    /// [`OWN_QUIET`]. When the last write landed, for that.
    own: bool,
    last_write: Option<Instant>,
}

/// (live-files) A write a path's pace holds: its bytes, its `run:` and
/// why the previous try of the path failed (see `Writer::write`).
type Held = (Vec<u8>, Option<Vec<String>>, Option<String>);

/// (live-files) A path's pace: when a write last landed there, and the
/// newest content waiting for [`PACE`] to pass.
struct Paced {
    last: Instant,
    next: Option<Held>,
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
    /// Path → FNV-1a hash of what Strand held there when it first
    /// owned it: ownership is the key; the hash is not kept current.
    owned: BTreeMap<PathBuf, u64>,
    shared: Arc<Shared>,
    ping: calloop::ping::Ping,
    /// Per path (a handful of files: lists, not maps).
    runs: Vec<(PathBuf, RunState)>,
    retries: Vec<(PathBuf, Retry)>,
    paced: Vec<(PathBuf, Paced)>,
    /// How long `run:` may take ([`RUN_TIMEOUT`]; tests shorten it).
    timeout: Duration,
    /// Registers a write with the config watcher before it lands.
    own_writes: Option<OwnWrites>,
    /// (proc-keeper) Holds `run:`'s pipes ([`run_once`]).
    keeper: Arc<Keeper>,
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
            paced: Vec::new(),
            timeout: RUN_TIMEOUT,
            own_writes: None,
            keeper: Arc::default(),
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
                .flat_map(|(_, r)| [r.due, r.held_until])
                .flatten()
                .chain(self.retries.iter().map(|(_, r)| r.at))
                .chain(
                    self.paced
                        .iter()
                        .filter(|(_, p)| p.next.is_some())
                        .map(|(_, p)| p.last + PACE),
                )
                .min();
            match self.shared.pop(next) {
                Some(Job::Write { path, bytes, run }) => {
                    // Newer content replaces a write still being retried.
                    let last = take(&mut self.retries, &path).map(|r| r.why);
                    self.write_paced(path, bytes, run, last);
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
                        r.ended = Some(Instant::now());
                        if std::mem::take(&mut r.queued) {
                            r.due = Some(Instant::now());
                        }
                    }
                }
                None => {}
            }
            self.pace_due();
            self.retry_due();
            self.unhold_due();
            self.start_due();
        }
    }

    /// (live-files) A write, at most one per [`PACE`] per path: within
    /// the window the content waits (the newest replacing it), and a
    /// `run:` due there waits for it.
    fn write_paced(
        &mut self,
        path: PathBuf,
        bytes: Vec<u8>,
        run: Option<Vec<String>>,
        last: Option<String>,
    ) {
        let now = Instant::now();
        if let Some(p) = entry(&mut self.paced, &path)
            && now < p.last + PACE
        {
            let at = p.last + PACE;
            p.next = Some((bytes, run, last));
            // `run:` follows the file's last write, and one is coming.
            if let Some(r) = entry(&mut self.runs, &path)
                && let Some(due) = &mut r.due
            {
                *due = (*due).max(at + RUN_DEBOUNCE);
            }
            return;
        }
        self.write(path, bytes, run, last);
    }

    /// Writes the paced contents whose window has passed.
    fn pace_due(&mut self) {
        let now = Instant::now();
        let due: Vec<PathBuf> = self
            .paced
            .iter()
            .filter(|(_, p)| p.next.is_some() && p.last + PACE <= now)
            .map(|(p, _)| p.clone())
            .collect();
        for path in due {
            let Some((bytes, run, last)) =
                entry(&mut self.paced, &path).and_then(|p| p.next.take())
            else {
                continue;
            };
            self.write(path, bytes, run, last);
        }
    }

    /// Ends the loop guard's holds whose file has been quiet for
    /// [`LOOP_WINDOW`]: `run:` runs once for the last content (a slider
    /// dragged past three rounds is caught up), and its next write runs
    /// it again; a round right after that catch-up run is a loop, held
    /// again and let go without a run. A catch-up run takes the loop
    /// notice away: the `run:` is no longer held (its next round, if it
    /// loops, says so again).
    fn unhold_due(&mut self) {
        let now = Instant::now();
        let mut caught_up = Vec::new();
        for (path, r) in &mut self.runs {
            if r.held_until.is_some_and(|at| at <= now) {
                r.held_until = None;
                r.rounds = 0;
                r.ended = None;
                // One catch-up run, unless that run is what looped.
                if std::mem::take(&mut r.skipped) && !std::mem::take(&mut r.stuck) {
                    r.due = Some(now);
                    r.probation = true;
                    caught_up.push(path.clone());
                } else {
                    r.probation = false;
                }
            }
        }
        for path in caught_up {
            self.tell(Outcome::Written(path));
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
                let now = Instant::now();
                // Held `run:`: the loop notice stands (the write is no news).
                let mut held = false;
                if changed {
                    match entry(&mut self.paced, &path) {
                        Some(p) => p.last = now,
                        None => self.paced.push((
                            path.clone(),
                            Paced {
                                last: now,
                                next: None,
                            },
                        )),
                    }
                }
                if changed && let Some(argv) = run.filter(|a| !a.is_empty()) {
                    let due = Some(now + RUN_DEBOUNCE);
                    let r = match entry(&mut self.runs, &path) {
                        Some(r) => r,
                        None => {
                            self.runs.push((path.clone(), RunState::default()));
                            let (_, r) = self.runs.last_mut()?;
                            r
                        }
                    };
                    r.argv = argv;
                    // (live-files) The loop guard: a write soon after the
                    // file's own run ended is a round; three in a row hold
                    // the `run:` until the file is quiet for a second.
                    let near =
                        r.running || r.ended.is_some_and(|e| now.duration_since(e) < LOOP_WINDOW);
                    if r.last_write
                        .is_some_and(|w| now.duration_since(w) >= OWN_QUIET)
                    {
                        r.own = false;
                    }
                    r.last_write = Some(now);
                    // A write while held that no run is near: the text
                    // changes on its own, and the rounds were its ticks,
                    // not a loop. The hold goes; the `run:` follows it.
                    if r.held_until.is_some() && !near {
                        r.own = true;
                        r.held_until = None;
                        r.skipped = false;
                        r.stuck = false;
                        r.probation = false;
                    }
                    let round = near && !r.own;
                    if round {
                        r.rounds += 1;
                    } else {
                        r.rounds = 0;
                        r.probation = false;
                    }
                    let was_held = r.held_until.is_some();
                    let again = round && r.probation;
                    if was_held || again || r.rounds >= LOOP_ROUNDS {
                        r.held_until = Some(now + LOOP_WINDOW);
                        r.due = None;
                        r.skipped = true;
                        r.stuck |= again;
                        held = true;
                        if !was_held {
                            self.tell(Outcome::Failed(path.clone(), LOOP_NOTICE.into()));
                        }
                    } else {
                        r.due = due;
                    }
                }
                if !held {
                    self.tell(Outcome::Written(path));
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
        self.paced.retain(|(p, _)| paths.contains(p));
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

    /// Records `path` as Strand's, with the hash of what it holds. The
    /// manifest is written only for a newly owned path: nothing reads
    /// the hash, so a later write does not rewrite and fsync it again.
    fn own(&mut self, path: &Path, hash: u64) {
        if self.owned.contains_key(path) {
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
            let keeper = self.keeper.clone();
            let ran = path.clone();
            let spawned = std::thread::Builder::new()
                .name("strand-files-run".into())
                .spawn(move || {
                    let failed = run_once(&argv, timeout, &keeper);
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
/// failed, its headline and (proc-reason) why after it, from what it
/// printed (`strand_services::proc::Tail`: its stderr's head and tail,
/// else the end of its stdout; spec.md 5.8). Its output is read while it
/// runs and for `proc::STDERR_GRACE` after it ends, never to its end of
/// file: a process the command backgrounds (`dunst &`), or one that left
/// the group with `setsid`, inherits the pipes and may hold them open
/// for good. (M5) Like every program strand starts, it does not inherit
/// the xdg-activation token strand was started with, nor the variables
/// that address this shell (`child::child_env`), and it gets back the
/// transparent huge page setting strand inherited
/// (`child::restore_in_child`): a daemon a `run:` restarts would
/// otherwise run with THP off for life. (proc-keeper) Its pipes are
/// handed to `keeper` while it runs and released after, so a process it
/// leaves holding them (`mako &`) has them read on, never dying of
/// SIGPIPE at its first line once strand stops reading them.
fn run_once(argv: &[String], timeout: Duration, keeper: &Keeper) -> Option<String> {
    use strand_services::proc::{self, Framer, Reader, Stream, Tail};
    let (prog, args) = argv.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
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
    // (proc-reason) From here it is named past `stdbuf`/`env`, as `permit
    // exec` checks it: `rsync` failed, not `stdbuf`.
    let prog = strand_core::command::program(argv);
    let mut reader = Reader::new(
        child.stdout.take().map(std::os::fd::OwnedFd::from),
        child.stderr.take().map(std::os::fd::OwnedFd::from),
    );
    let run = keeper.next_run();
    keeper.hold(run, &reader.fds());
    let mut tail = Tail::new();
    let mut framers = [Framer::new(), Framer::new()];
    let mut read = |s: Stream, bytes: Option<&[u8]>| {
        let f = &mut framers[usize::from(s.is_err())];
        let mut keep = |l: proc::Framed| tail.push(s, &l.text());
        match bytes {
            Some(b) => f.push(b, &mut keep),
            None => f.finish(&mut keep),
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        let _ = reader.step(&[], Some(Duration::from_millis(20)), true, &mut read);
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => {}
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
    reader.drain_until(Instant::now() + proc::STDERR_GRACE, &mut read);
    for (i, f) in framers.iter_mut().enumerate() {
        let s = if i == 1 { Stream::Err } else { Stream::Out };
        f.finish(&mut |l| tail.push(s, &l.text()));
    }
    keeper.release(run);
    drop(reader);
    let Some(status) = status else {
        return Some(tail.explain(&format!(
            "`run:` `{prog}` took more than {} s and was killed",
            timeout.as_secs_f32()
        )));
    };
    if status.success() {
        return None;
    }
    Some(tail.explain(&format!("`run:` `{prog}` failed ({status})")))
}

/// Writes `bytes` to `path` through a temporary file beside it and a
/// rename, with `meta`'s mode when the file was there. Over a file that
/// was there the temporary file is created 0600 and given that mode
/// only once written, so content bound for a private file is never
/// readable by others on its way; a new file gets the umask's default.
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
        use std::os::unix::fs::OpenOptionsExt as _;
        // A leftover of a run that died with this pid, so `create_new`
        // makes the file with the mode below and never opens another's.
        let _ = std::fs::remove_file(&tmp);
        let mode = if meta.is_some() { 0o600 } else { 0o666 };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
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

    /// [`super::run_once`] with one keeper for the test.
    fn run_once(argv: &[String], timeout: Duration) -> Option<String> {
        static KEEPER: std::sync::LazyLock<Keeper> = std::sync::LazyLock::new(Keeper::default);
        super::run_once(argv, timeout, &KEEPER)
    }

    /// (proc-keeper) A daemon a `run:` starts in the background
    /// (`pkill mako; mako &`) inherits its pipes: once the run has ended
    /// and strand stopped reading them, the keeper reads on, so its
    /// first lines on stdout and stderr do not kill it with SIGPIPE.
    #[test]
    fn a_daemon_a_run_starts_outlives_its_reading() {
        let dir = temp("daemon");
        let marker = dir.join("lived");
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        let started = Instant::now();
        let daemon = format!(
            "(sleep 2; echo 'daemon: started'; echo 'daemon: note' >&2; touch {}) &",
            marker.display()
        );
        assert_eq!(run_once(&sh(&daemon), RUN_TIMEOUT), None);
        // Ended (and strand stopped reading) before the daemon printed.
        assert!(started.elapsed() < Duration::from_secs(2));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the daemon died at its first line"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

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

    /// Over a private file, the temporary file the new content is written
    /// to is never readable by group or others while it holds it; a new
    /// file still gets the umask's default.
    #[test]
    fn a_private_file_is_never_readable_by_others_while_rewritten() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = temp("private");
        let out = dir.join("secret.conf");
        std::fs::write(&out, "old").unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600)).unwrap();
        let meta = std::fs::metadata(&out).unwrap();
        let tmp = dir.join(format!(".secret.conf.strand-tmp.{}", std::process::id()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let watcher = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (mut worst, mut seen) = (0, 0u32);
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(m) = std::fs::metadata(&tmp) {
                        worst |= m.permissions().mode() & 0o077;
                        seen += 1;
                    }
                }
                (worst, seen)
            })
        };
        let payload = vec![b'x'; 1 << 20];
        for _ in 0..200 {
            write_atomic(&out, &payload, Some(&meta)).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let (worst, seen) = watcher.join().unwrap();
        assert!(seen > 0, "the watcher never saw the temporary file");
        assert_eq!(worst, 0, "the temporary file was readable by others");
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A new file: the mode a plain create gives under this umask.
        let plain = dir.join("plain");
        std::fs::File::create(&plain).unwrap();
        let fresh = dir.join("fresh.conf");
        write_atomic(&fresh, b"new", None).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777
        );
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

    /// A path's manifest line is written once, as Strand first owns it:
    /// later writes of new bytes there (a status file following a track)
    /// do not rewrite and fsync the manifest each time.
    #[test]
    fn the_manifest_is_written_once_per_owned_path() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = temp("manifest-once");
        let out = dir.join("out/a.css");
        let (mut w, _rx) = writer(&dir);
        assert_eq!(w.write_file(&out, b"one"), Ok(true));
        let m = w.manifest_file.clone().unwrap();
        let (ino, text) = (
            std::fs::metadata(&m).unwrap().ino(),
            std::fs::read_to_string(&m).unwrap(),
        );
        for bytes in [&b"two"[..], b"three"] {
            assert_eq!(w.write_file(&out, bytes), Ok(true));
            assert_eq!(std::fs::metadata(&m).unwrap().ino(), ino, "not rewritten");
            assert_eq!(std::fs::read_to_string(&m).unwrap(), text);
        }
        // A second path is new: the manifest is written, naming both.
        let other = dir.join("out/b.css");
        assert_eq!(w.write_file(&other, b"b"), Ok(true));
        let text = std::fs::read_to_string(&m).unwrap();
        assert_eq!(parse_manifest(&text).len(), 2, "{text}");
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
        // (proc-reason) Named past `stdbuf`/`env`, as `permit exec`
        // checks it.
        let mut wrapped: Vec<String> = ["env", "LC_ALL=C", "stdbuf", "-oL"]
            .map(String::from)
            .to_vec();
        wrapped.extend(sh("echo boom >&2; exit 3"));
        assert_eq!(
            run_once(&wrapped, RUN_TIMEOUT).as_deref(),
            Some("`run:` `sh` failed (exit status: 3): boom")
        );
        assert_eq!(run_once(&["true".into()], RUN_TIMEOUT), None);
        let why = run_once(&["/nonexistent/x".into()], RUN_TIMEOUT).unwrap();
        assert!(why.contains("could not start"), "{why}");
        // (proc-reason) A long stderr: its head and its tail, at lines.
        let why = run_once(
            &sh(
                "{ echo 'error: the first'; i=0; while [ $i -lt 2000 ]; do echo \"noise $i\"; \
                 i=$((i+1)); done; echo 'the last'; } >&2; exit 1",
            ),
            RUN_TIMEOUT,
        )
        .unwrap();
        use strand_services::proc::{TAIL_END, TAIL_HEAD};
        assert!(why.len() < TAIL_HEAD + TAIL_END + 200, "{}", why.len());
        assert!(why.contains(": error: the first\n"), "{why}");
        assert!(why.contains("\n…\n") && why.ends_with("the last"), "{why}");
        // With nothing on stderr, the end of its stdout.
        let why = run_once(&sh("echo 'reload: no such unit'; exit 5"), RUN_TIMEOUT).unwrap();
        assert!(
            why.ends_with("failed (exit status: 5): reload: no such unit"),
            "{why}"
        );
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
        // A minute each, so 30 s tells a wait from none on any machine.
        assert_eq!(run_once(&sh("sleep 60 & exit 0"), RUN_TIMEOUT), None);
        let why = run_once(&sh("sleep 60 & echo boom >&2; exit 3"), RUN_TIMEOUT).unwrap();
        assert!(why.contains("boom"), "{why}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
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
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the run outlived its 300 ms timeout: {:?}",
            started.elapsed()
        );
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
        // A minute, so 30 s tells a wait for what holds stderr from none.
        let why = run_once(
            &sh("setsid sleep 60 & sleep 120"),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(why.contains("was killed"), "{why}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "waited for the process that left the group: {:?}",
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
        let mut files = Files::new(ping, Some(dir.join("state/files")), None, Arc::default());
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

    /// A writer on its own thread that records each write as it lands:
    /// (jobs and outcomes, the writes with when they landed).
    #[allow(clippy::type_complexity)]
    fn recorded(dir: &Path) -> (Arc<Shared>, Arc<Mutex<Vec<(Vec<u8>, Instant)>>>) {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let shared = Arc::new(Shared::default());
        let mut w = Writer::new(Some(dir.join("state/files")), shared.clone(), ping);
        let seen: Arc<Mutex<Vec<(Vec<u8>, Instant)>>> = Arc::default();
        let log = seen.clone();
        w.own_writes = Some(OwnWrites(Box::new(move |_, b| {
            log.lock().unwrap().push((b.to_vec(), Instant::now()));
        })));
        std::thread::spawn(move || w.run());
        (shared, seen)
    }

    /// (live-files) A path is written at most once per 500 ms: the first
    /// write lands at once, ten more within 100 ms give one more write,
    /// the newest, when the window ends; a quiet path writes at once.
    #[test]
    fn writes_are_paced() {
        let dir = temp("paced");
        let out = dir.join("status.txt");
        let (jobs, seen) = recorded(&dir);
        let write = |b: &str| {
            jobs.send(Job::Write {
                path: out.clone(),
                bytes: b.as_bytes().to_vec(),
                run: None,
            })
            .unwrap();
        };
        let start = Instant::now();
        write("0");
        let until = |n: usize| {
            while seen.lock().unwrap().len() < n {
                assert!(start.elapsed() < Duration::from_secs(5), "{n} writes");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        until(1);
        let first = seen.lock().unwrap()[0].1;
        assert!(
            first - start < Duration::from_millis(200),
            "the first is at once"
        );
        for i in 1..=10 {
            write(&i.to_string());
            std::thread::sleep(Duration::from_millis(10));
        }
        until(2);
        std::thread::sleep(Duration::from_millis(700));
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "eleven changes, two writes: {got:?}");
        assert_eq!(got[1].0, b"10", "the newest lands");
        assert!(got[1].1 - first >= PACE, "at the window's end");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "10");
        // Quiet for a window: the next write lands at once.
        let again = Instant::now();
        write("11");
        until(3);
        assert!(seen.lock().unwrap()[2].1 - again < Duration::from_millis(200));
    }

    /// (live-files) A `run:` that changes what its file is written from:
    /// after three rounds the `run:` is held, with a notice, and the loop
    /// stops (one catch-up run, then nothing).
    #[test]
    fn a_run_that_rewrites_its_input_is_held() {
        let dir = temp("loop");
        let log = dir.join("runs.log");
        let out = dir.join("monitors.conf");
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        let run = Some(vec![
            "sh".to_string(),
            "-c".into(),
            format!("echo x >> {}", log.display()),
        ]);
        let write = |n: usize| {
            jobs.send(Job::Write {
                path: out.clone(),
                bytes: format!("v{n}").into_bytes(),
                run: run.clone(),
            })
            .unwrap();
        };
        // The "config": every run of the program bumps what the file is
        // written from, which writes the file again.
        let start = Instant::now();
        let mut n = 0;
        write(n);
        let mut runs = 0;
        while start.elapsed() < Duration::from_secs(5) {
            let now = lines(&log);
            if now > runs {
                runs = now;
                n += 1;
                write(n);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let outcomes = rx.drain();
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, Outcome::Failed(p, why) if *p == out && why == LOOP_NOTICE)),
            "{outcomes:?}"
        );
        // Three rounds, the catch-up run once the file was quiet, and the
        // hold its own round brings, let go without a run.
        assert_eq!(lines(&log), 4, "the loop stops");
        // A change from outside runs `run:` again.
        std::thread::sleep(LOOP_WINDOW + Duration::from_millis(200));
        write(100);
        let later = Instant::now();
        while lines(&log) < 5 {
            assert!(later.elapsed() < Duration::from_secs(5), "ran again");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until `run:` has run `n` times, by its log.
    fn wait_runs(log: &Path, n: usize, seen: &[Outcome]) {
        let start = Instant::now();
        while lines(log) < n {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{} of {n} runs: {seen:?}",
                lines(log)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// (live-files, review round 2) A slider dragged past three rounds:
    /// the hold ends with its catch-up run, and that takes the loop
    /// notice away (the `run:` is no longer held).
    #[test]
    fn a_catch_up_run_takes_the_loop_notice_away() {
        let dir = temp("catch-up");
        let log = dir.join("runs.log");
        let out = dir.join("kitty.conf");
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        let run = Some(vec![
            "sh".to_string(),
            "-c".into(),
            format!("echo x >> {}", log.display()),
        ]);
        // Four clicks, each as soon as the last click's run is in the
        // log (by the run, not the clock, so a slow machine is the same
        // loop): each lands within a second of the last run ending, so
        // the fourth is held.
        let mut seen: Vec<Outcome> = Vec::new();
        for n in 0..4 {
            if n > 0 {
                wait_runs(&log, n, &seen);
            }
            jobs.send(Job::Write {
                path: out.clone(),
                bytes: format!("size {n}").into_bytes(),
                run: run.clone(),
            })
            .unwrap();
            seen.extend(rx.drain());
        }
        // Quiet: the hold ends a second after the held write, with one
        // catch-up run, and nothing runs after it.
        let held = |seen: &[Outcome]| {
            seen.iter().position(
                |o| matches!(o, Outcome::Failed(p, why) if *p == out && why == LOOP_NOTICE),
            )
        };
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            seen.extend(rx.drain());
            let caught_up = held(&seen).is_some_and(|h| {
                seen[h + 1..]
                    .iter()
                    .any(|o| matches!(o, Outcome::Written(p) if *p == out))
            });
            if caught_up && lines(&log) >= 4 {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "held and caught up: {seen:?}"
            );
        }
        std::thread::sleep(LOOP_WINDOW + Duration::from_millis(500));
        seen.extend(rx.drain());
        assert!(held(&seen).is_some(), "held: {seen:?}");
        assert_eq!(lines(&log), 4, "three runs and the catch-up: {seen:?}");
        assert!(
            matches!(seen.last(), Some(Outcome::Written(p)) if *p == out),
            "the notice goes with the catch-up run: {seen:?}"
        );
    }

    /// (live-files, review round 1) A file whose text changes on its own
    /// (a player's position, a slider) can look like a loop for three
    /// rounds; while held, the loop notice stands (a held write does not
    /// take it away), and the first write no run is near shows the text
    /// changes by itself: the hold goes and `run:` follows every later
    /// write.
    #[test]
    fn a_text_that_changes_on_its_own_is_not_held() {
        let dir = temp("own");
        let log = dir.join("runs.log");
        let out = dir.join("np.txt");
        let (jobs, rx) = spawned(&dir, RUN_TIMEOUT);
        let run = Some(vec![
            "sh".to_string(),
            "-c".into(),
            format!("echo x >> {}", log.display()),
        ]);
        let write = |n: usize| {
            jobs.send(Job::Write {
                path: out.clone(),
                bytes: format!("pos {n}").into_bytes(),
                run: run.clone(),
            })
            .unwrap();
        };
        let mut seen: Vec<Outcome> = Vec::new();
        // Three rounds, each write just after the last run (by the log,
        // not the clock): the fourth write is held.
        for n in 0..4 {
            if n > 0 {
                wait_runs(&log, n, &seen);
            }
            write(n);
        }
        // The fourth lands at its pace window's end, held: the notice
        // comes, and stands.
        let notice =
            |o: &Outcome| matches!(o, Outcome::Failed(p, why) if *p == out && why == LOOP_NOTICE);
        let start = Instant::now();
        while !seen.iter().any(notice) {
            assert!(start.elapsed() < Duration::from_secs(10), "held: {seen:?}");
            std::thread::sleep(Duration::from_millis(5));
            seen.extend(rx.drain());
        }
        std::thread::sleep(Duration::from_millis(250));
        seen.extend(rx.drain());
        let sampled = seen.last().cloned();
        // The text goes on changing every 300 ms, which keeps the hold
        // (no catch-up run comes while writes are under a second apart);
        // the first write a second past the last run is near no run.
        let mut n = 4;
        let start = Instant::now();
        while lines(&log) < 4 {
            write(n);
            std::thread::sleep(Duration::from_millis(300));
            seen.extend(rx.drain());
            n += 1;
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the hold never went: {seen:?}"
            );
        }
        assert!(
            matches!(&sampled, Some(Outcome::Failed(p, why)) if *p == out && why == LOOP_NOTICE),
            "the notice stands while held: {sampled:?} in {seen:?}"
        );
        // Every later write runs `run:`, even one just after a run. A
        // catch-up run in place of the hold going would hold the next
        // such write for good, and this would time out.
        for _ in 0..2 {
            let have = lines(&log);
            write(n);
            n += 1;
            seen.extend(rx.drain());
            wait_runs(&log, have + 1, &seen);
        }
    }

    /// (live-files) A `file` waiting for a service that is starting says
    /// nothing; one waiting for a service with no run up says which.
    #[test]
    fn a_wait_on_a_service_with_no_run_is_a_notice() {
        let snap = |name: &str, running: bool| strand_services::ServiceSnapshot {
            name: name.into(),
            running,
            fields: Vec::new(),
        };
        let waits = vec!["outputs".to_string(), "media".to_string()];
        assert_eq!(
            waiting_notice(&waits, &[snap("outputs", true), snap("media", true)]),
            None
        );
        let why = waiting_notice(&waits, &[snap("outputs", true), snap("media", false)]).unwrap();
        assert!(why.starts_with("waits for `media`"), "{why}");
    }

    /// (live-files, review round 3) A waiting notice outlives a write
    /// that lands while the content waits (one paced from before the
    /// service went down), and goes once the content is no longer
    /// waiting on it.
    #[test]
    fn a_write_landing_while_waiting_keeps_the_notice() {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut files = Files::new(ping, None, None, Arc::default());
        let (a, b) = (Path::new("/x/a.lua"), Path::new("/x/b.json"));
        files.set_waiting(a, Some("waits for `outputs`".into()));
        files.set_waiting(b, None);
        assert_eq!(files.still_waiting(a), Some("waits for `outputs`"));
        assert_eq!(files.still_waiting(b), None);
        files.set_waiting(a, Some("waits for `media`".into()));
        assert_eq!(files.still_waiting(a), Some("waits for `media`"));
        files.set_waiting(a, None);
        assert_eq!(files.still_waiting(a), None);
    }

    /// (build-ready) A `file` waiting for a service that is up but has
    /// not answered is told after [`SLOW_WAIT`], once, naming it; a wait
    /// that ends, or one with a no-run-up notice, is not.
    #[test]
    fn a_long_wait_on_a_service_that_is_up_is_a_notice() {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut files = Files::new(ping, None, None, Arc::default());
        let t0 = Instant::now();
        let (a, b, c) = (Path::new("/x/a"), Path::new("/x/b"), Path::new("/x/c"));
        files.set_slow(a, Some(vec!["news".into(), "rate".into()]), t0);
        files.set_slow(b, Some(vec!["rate".into()]), t0);
        files.set_slow(c, Some(vec!["media".into()]), t0);
        files.set_waiting(c, Some("waits for `media`, which has no run up".into()));
        assert_eq!(files.wait(t0), Some(SLOW_WAIT));
        assert!(files.slow_due(t0 + SLOW_WAIT / 2).is_empty());
        // The same wait seen again keeps its start.
        files.set_slow(a, Some(vec!["news".into()]), t0 + SLOW_WAIT / 2);
        files.set_slow(b, None, t0 + SLOW_WAIT / 2);
        let due = files.slow_due(t0 + SLOW_WAIT);
        assert_eq!(due.len(), 1, "{due:?}");
        assert_eq!(due[0].0, a);
        assert!(
            due[0]
                .1
                .starts_with("waits for `news`, which has not answered in 10 s"),
            "{}",
            due[0].1
        );
        assert!(files.slow_due(t0 + SLOW_WAIT * 3).is_empty(), "told once");
        assert_eq!(files.wait(t0 + SLOW_WAIT), None);
        // Waiting on another service starts the count again.
        files.set_slow(a, Some(vec!["rate".into()]), t0 + SLOW_WAIT * 3);
        assert_eq!(files.wait(t0 + SLOW_WAIT * 3), Some(SLOW_WAIT));
    }

    /// (build-ready review) A waiting `file` that a reload took away (or
    /// moved) is let go: no slow-wait notice comes for it later, and its
    /// path is given back once so its waiting notice goes; one still
    /// written keeps its wait.
    #[test]
    fn a_wait_whose_file_is_gone_is_let_go() {
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut files = Files::new(ping, None, None, Arc::default());
        let t0 = Instant::now();
        let (a, b) = (Path::new("/x/news.conf"), Path::new("/x/kept.conf"));
        files.set_slow(a, Some(vec!["news".into()]), t0);
        files.set_waiting(a, Some("waits for `news`, which has no run up".into()));
        files.set_slow(b, Some(vec!["news".into()]), t0);
        let gone = files.let_go(|each| each(b));
        assert_eq!(gone, [a.to_path_buf()]);
        assert_eq!(files.still_waiting(a), None);
        let due = files.slow_due(t0 + SLOW_WAIT);
        assert_eq!(due.len(), 1, "{due:?}");
        assert_eq!(due[0].0, b);
        assert!(files.let_go(|each| each(b)).is_empty(), "let go once");
    }

    /// (build-ready review) [`Files::keep`], which `after_diff` calls
    /// each tick, lets go of the waits of the paths the running config's
    /// `file`s no longer write (here the instance writes only `kept`),
    /// writer or not, and gives each such path back once for its notice
    /// to go.
    #[test]
    fn keep_lets_go_of_the_waits_no_file_writes() {
        use strand_compiler::instantiate::{Instance, Storage};
        let mut map = strand_compiler::source::SourceMap::new();
        map.add("a.strand", "file \"/x/kept.conf\" { text: \"k\" }\n");
        let b = strand_compiler::reconcile::Build::compile(None, map).unwrap();
        let rt = strand_core::Runtime::new();
        let host = std::rc::Rc::new(strand_compiler::vm::schema_host::SchemaHost::mock(
            &rt,
            &b.program.types,
        ));
        let inst = Instance::from_build(&rt, &b, host, Storage::none());
        let _ = inst.flush();
        let _ = inst.take_outputs();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut files = Files::new(ping, None, None, Arc::default());
        let t0 = Instant::now();
        let (gone, kept) = (Path::new("/x/gone.conf"), Path::new("/x/kept.conf"));
        files.set_slow(gone, Some(vec!["news".into()]), t0);
        files.set_waiting(gone, Some("waits for `news`, which has no run up".into()));
        files.set_slow(kept, Some(vec!["news".into()]), t0);
        assert_eq!(files.keep(&inst), [gone.to_path_buf()]);
        assert_eq!(files.still_waiting(gone), None);
        let due = files.slow_due(t0 + SLOW_WAIT);
        assert_eq!(due.len(), 1, "{due:?}");
        assert_eq!(due[0].0, kept, "{due:?}");
        assert!(files.keep(&inst).is_empty(), "given back once");
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

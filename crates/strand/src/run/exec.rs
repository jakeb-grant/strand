//! (M6) The commands of declared actions (`action dispatch(ws: int)
//! ["hyprctl", "dispatch", "workspace", ws]`; decisions.md
//! m6-w2-language, the owner's ruling on owner-testing Gaps 1).
//!
//! A handler's call asks for its command (`Instance::take_execs`, taken
//! after each tick's diff). Each starts on a thread of its own, so logic
//! never waits on a fork or on the program. An action runs at most once
//! at a time: a call while its command still runs queues one re-run (the
//! latest call's arguments), started when it ends, as a `file`'s `run:`
//! does, so a command that hangs holds one thread however often it is
//! clicked. Each is detached in its own process
//! group (a signal to strand's group does not reach it), stdin on
//! `/dev/null`; (proc-run) its stdout and stderr are piped to its thread,
//! which frames them (`strand_services::proc`) into the run's `lines`
//! (the last 1000, at most 256 KiB), published to logic at most once a
//! frame, and, for an action with a block, reads its stdout lines into
//! the block's fields and its `line` handlers (lossless: past
//! `proc::BACKLOG_LINES` lines waiting, stdout is not read until they are
//! taken). The keeper (`super::keeper`) holds a duplicate of the pipes,
//! so the command never gets SIGPIPE when strand goes. Like every program
//! strand starts it does not inherit strand's xdg-activation token or the
//! variables that address this shell (`child::child_env`), and it gets
//! back the transparent huge page setting strand inherited. Its
//! thread waits for it (asleep on its pidfd) and nothing else: it is
//! never killed unasked, since an app it opens may run for hours. A failure to start or a status
//! its `ok` does not list comes back (the loop is pinged awake) as a log
//! line and a `strand watch` notice naming the action, with why from what
//! it printed (`proc::Tail`), as a `file`'s `run:` failing does. The
//! shell itself is fine, so it is not an overlay notice. When it ends,
//! logic gets its last lines, its `code`, its `error` and its end in one
//! update, in that order (spec.md 5.7's end order).
//!
//! (flex) An action with a result (`action calc(q: text) -> text
//! ["qalc", "-t", q]`) runs each call at once on a thread of its own (a
//! binding's newer call supersedes the older rather than queueing behind
//! it), its stdout piped and read up to `strand_services::custom::
//! MAX_DOCUMENT` bytes. When it ends, its output is read as the declared
//! type on the logic thread (`services::custom::read_output`) and
//! answers the call's `Async`; a failure to start, a status its `ok`
//! does not list (with why, from its stderr), too
//! much output or output that does not convert is the `Async`'s error,
//! logged but no notice (the config reads `.error`). A call whose value
//! was dropped first (superseded, or a handler let it go) has its
//! process group ended: SIGTERM, then SIGKILL after
//! `strand_services::custom::KILL_GRACE`.
//!
//! (lockmon) `rec.stop()` (an [`ExecRequest`] with `stop`) asks each
//! command of the action running to stop and drops the re-run queued
//! behind one: the thread that owns the command sends its process group
//! SIGINT, then SIGTERM after [`STOP_GRACE`] while it still runs, then
//! SIGKILL after another, and reaps it after, so no signal reaches a
//! group whose id was handed on. A stopped command is no failure (no
//! notice); a stopped call's `.error` says it was stopped. After each
//! step logic is told how each action's run changed (`rec.running`,
//! `rec.started`: [`Execs::runs`], `Instance::set_action_run`).

use std::collections::{BTreeSet, HashMap};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use strand_compiler::vm::runs::{RunLine, RunUpdate};
use strand_compiler::vm::{ExecReply, ExecRequest};
use strand_services::Data;
use strand_services::custom::{self as svc, KILL_GRACE, LISTEN_FLUSH, MAX_DOCUMENT, POLL_TIMEOUT};
use strand_services::proc::{self, Framed, Framer, Reader, Ring, RingLine, Stream, Tail};

use super::keeper::Keeper;
use super::shell::Shell;
use crate::services::custom::{BlockSpec, Blocks};

/// A command's thread only starts it and waits.
const STACK: usize = 64 * 1024;

/// (lockmon) How long a stopped command has after SIGINT before SIGTERM,
/// and after SIGTERM before SIGKILL (spec.md 5.7).
pub(super) const STOP_GRACE: Duration = Duration::from_secs(2);

/// A command that ended, as its thread reports it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Ended {
    /// The action's name.
    pub action: String,
    /// Why it failed; `None`: it exited with a status its `ok` lists, or
    /// was stopped.
    pub failed: Option<String>,
    /// Its exit code ([`Captured::exit`]).
    pub exit: i32,
    /// (lockmon) `rec.stop()` ended it (no notice: it was asked to).
    pub stopped: bool,
    /// (proc-run) What it printed that logic has not taken yet: its last
    /// lines, taken after it ended, so they go before its end.
    pub taken: Taken,
}

/// (proc-run) What a job's thread read since logic last took it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Taken {
    /// Its `lines`' change ([`Ring::take`]): the number below which lines
    /// left, and the lines new or changed.
    pub lines: Option<(u64, Vec<RingLine>)>,
    /// For a block: each field the stdout lines set, the latest (by the
    /// field's index).
    pub fields: Vec<Option<Data>>,
    /// For a block: each stdout line its `line` handlers hear, in order,
    /// with the spec it was counted for ([`svc::sent_line`]).
    pub heard: Vec<(i64, Vec<Data>)>,
}

impl Taken {
    fn is_empty(&self) -> bool {
        self.lines.is_none() && self.fields.iter().all(Option::is_none) && self.heard.is_empty()
    }

    /// Its lines as a run update's.
    pub(super) fn update(&self) -> (u64, Vec<RunLine>) {
        let Some((front, lines)) = &self.lines else {
            return (0, Vec::new());
        };
        let lines = lines
            .iter()
            .map(|l| RunLine {
                n: l.n,
                text: l.text.clone(),
                stderr: l.stderr,
                open: l.open,
            })
            .collect();
        (*front, lines)
    }
}

/// (proc-run) A job's output as its thread fills it and logic takes it.
#[derive(Debug, Default)]
pub(super) struct Taking {
    ring: Ring,
    fields: Vec<Option<Data>>,
    heard: Vec<(i64, Vec<Data>)>,
}

impl Taking {
    fn take(&mut self) -> Taken {
        Taken {
            lines: self.ring.take(),
            fields: std::mem::take(&mut self.fields),
            heard: std::mem::take(&mut self.heard),
        }
    }
}

/// (proc-run) What a job's thread shares: its action, its `ok`, where its
/// output goes, the action's block, the keeper, and the loop to wake.
pub(super) struct JobCx {
    pub action: String,
    pub ok: Vec<i32>,
    /// Its run's id, for the keeper.
    pub id: u64,
    pub out: Arc<Mutex<Taking>>,
    pub blocks: Blocks,
    pub keeper: Arc<Keeper>,
    pub ping: Option<calloop::ping::Ping>,
}

impl JobCx {
    fn block(&self) -> Option<BlockSpec> {
        lock(&self.blocks).get(&self.action).cloned()
    }

    fn ping(&self) {
        if let Some(p) = &self.ping {
            p.ping();
        }
    }
}

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// (flex) Commands' outputs as their threads report them: the call's
/// id, and what it printed or why it failed.
type Outputs = Arc<Mutex<Vec<(u64, Captured)>>>;

/// Nothing asked of a command's thread: it waits for the command.
const GO: u8 = 0;
/// Its call was dropped: SIGTERM, then SIGKILL ([`end_group`]).
const DROPPED: u8 = 1;
/// (lockmon) `rec.stop()`: SIGINT, SIGTERM, SIGKILL ([`stop_group`]).
const STOPPED: u8 = 2;

/// (lockmon) What a command's thread is asked to do with its command,
/// from the logic thread: the first ask counts. The thread alone signals
/// the command's group and reaps it, so a signal never reaches a
/// process group whose id was handed on.
pub(super) struct Halt {
    why: AtomicU8,
    /// Wakes a detached command's thread from its wait (an eventfd);
    /// `None` when there is none, and that thread looks every
    /// [`SLOW_TICK`].
    wake: Option<rustix::fd::OwnedFd>,
}

impl Halt {
    /// A command not asked anything yet, with a wake for a thread that
    /// sleeps until its command ends (`wake`).
    pub(super) fn new(wake: bool) -> Halt {
        let wake = wake
            .then(|| {
                rustix::event::eventfd(
                    0,
                    rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
                )
                .ok()
            })
            .flatten();
        Halt {
            why: AtomicU8::new(GO),
            wake,
        }
    }

    fn ask(&self, why: u8) {
        let _ = self
            .why
            .compare_exchange(GO, why, Ordering::SeqCst, Ordering::SeqCst);
        if let Some(w) = &self.wake {
            let _ = rustix::io::write(w, &1u64.to_ne_bytes());
        }
    }

    /// `rec.stop()`: end it, SIGINT first.
    pub(super) fn stop(&self) {
        self.ask(STOPPED);
    }

    /// Its call was dropped: end it.
    pub(super) fn drop_call(&self) {
        self.ask(DROPPED);
    }

    fn why(&self) -> u8 {
        self.why.load(Ordering::SeqCst)
    }

    /// Empties the wake, once looked at.
    fn drain(&self) {
        if let Some(w) = &self.wake {
            let mut b = [0u8; 8];
            let _ = rustix::io::read(w, &mut b);
        }
    }
}

/// (lockmon) A detached command running.
struct Detached {
    started: SystemTime,
    halt: Arc<Halt>,
    /// (proc-run) What it printed, as its thread fills it.
    out: Arc<Mutex<Taking>>,
    /// (proc-run) Logic was told it started (its run reset).
    announced: bool,
}

/// The commands started and the ones that ended since logic last looked.
pub(super) struct Execs {
    ping: calloop::ping::Ping,
    ended: Arc<Mutex<Vec<Ended>>>,
    /// The actions whose command is running.
    running: HashMap<String, Detached>,
    /// The re-run queued behind each running one: the latest call.
    queued: HashMap<String, ExecRequest>,
    /// (flex) The commands of actions with a result, running.
    fetching: Vec<Fetching>,
    /// Their outputs as their threads report them, by [`Fetching::id`].
    outputs: Outputs,
    next_id: u64,
    /// How long a command with a result has to answer
    /// ([`POLL_TIMEOUT`]; shorter in tests).
    timeout: Duration,
    /// (lockmon) The actions whose run changed since [`Execs::runs`].
    changed: BTreeSet<String>,
    /// How long a stopped command has at each step ([`STOP_GRACE`];
    /// shorter in tests).
    grace: Duration,
    /// (proc-run) The actions' blocks ([`crate::services::custom::CustomHost::blocks`]).
    pub(super) blocks: Blocks,
    /// (proc-keeper) Started on the first job.
    keeper: Arc<Keeper>,
    /// (proc-run) Each action's `ok` statuses, for a request with none
    /// ([`Execs::set_ok`]).
    ok: HashMap<String, Vec<i32>>,
}

/// (flex) A running command whose output answers its call.
struct Fetching {
    id: u64,
    action: String,
    /// Its arguments (the recorder holds texts that are not the
    /// program's own).
    argv: Vec<String>,
    reply: ExecReply,
    /// Asked when its call was dropped or `rec.stop()`: its thread ends
    /// it.
    halt: Arc<Halt>,
    /// (lockmon) When it started.
    started: SystemTime,
}

/// A command's output answering its call, as [`Execs::answered`]
/// reports it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Answered {
    /// The action's name.
    pub action: String,
    /// Why it has no value; `None`: it answered.
    pub failed: Option<String>,
    /// Its exit code ([`Captured::exit`]).
    pub exit: i32,
    /// Its arguments.
    pub argv: Vec<String>,
    /// What answered the call: its value, or the error.
    pub answer: Result<strand_compiler::vm::Value, String>,
}

/// (lockmon) An action's run as [`Execs::runs`] reports it: whether a
/// command of it runs, and when the one running longest started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunState {
    pub action: String,
    pub running: bool,
    pub started: Option<SystemTime>,
    /// (proc-run) A job started since: its `lines`, `code`, `error` and
    /// block reset.
    pub reset: bool,
}

impl Execs {
    /// `ping` wakes the loop when a command ends.
    pub(super) fn new(ping: calloop::ping::Ping) -> Execs {
        Execs {
            ping,
            ended: Arc::default(),
            running: HashMap::new(),
            queued: HashMap::new(),
            fetching: Vec::new(),
            outputs: Arc::default(),
            next_id: 0,
            timeout: POLL_TIMEOUT,
            changed: BTreeSet::new(),
            grace: STOP_GRACE,
            blocks: Blocks::default(),
            keeper: Arc::default(),
            ok: HashMap::new(),
        }
    }

    /// (proc-ok) The actions `prog` declares and the statuses each takes
    /// for success (`[0]` unless its `ok` says otherwise).
    pub(super) fn set_ok(&mut self, prog: &strand_compiler::lower::Program) {
        self.ok = prog
            .execs
            .values()
            .map(|a| (a.name.clone(), a.ok.clone()))
            .collect();
    }

    fn ok_of(&self, action: &str) -> Vec<i32> {
        self.ok.get(action).cloned().unwrap_or_else(|| vec![0])
    }

    /// Starts `req`'s command on a thread of its own, or (its action's
    /// command still running) queues it to run once that ends, in place
    /// of any re-run queued before. (flex) A command with a reply always
    /// starts at once. (lockmon) A stop stops the action's commands.
    pub(super) fn start(&mut self, req: ExecRequest) {
        if req.stop {
            self.stop(&req.action);
            return;
        }
        if let Some(reply) = req.reply.clone() {
            self.fetch(req, reply);
            return;
        }
        if self.running.contains_key(&req.action) {
            self.queued.insert(req.action.clone(), req);
            return;
        }
        self.spawn(req);
    }

    /// (lockmon) `rec.stop()`: every command of `action` running is
    /// asked to stop (its thread signals it), and the re-run queued
    /// behind one is dropped. A call after the stop queues as usual,
    /// started once the stopped command has ended.
    fn stop(&mut self, action: &str) {
        self.queued.remove(action);
        if let Some(d) = self.running.get(action) {
            d.halt.stop();
        }
        for f in self.fetching.iter().filter(|f| f.action == action) {
            f.halt.stop();
        }
    }

    /// The commands whose action is running or queued (tests).
    #[cfg(test)]
    fn busy(&self) -> (usize, usize) {
        (self.running.len(), self.queued.len())
    }

    fn spawn(&mut self, req: ExecRequest) {
        let ended = self.ended.clone();
        let ping = self.ping.clone();
        let action = req.action.clone();
        let halt = Arc::new(Halt::new(true));
        let (h, grace) = (halt.clone(), self.grace);
        let out = Arc::new(Mutex::new(Taking::default()));
        let id = self.next_id;
        self.next_id += 1;
        let cx = JobCx {
            action: action.clone(),
            ok: self.ok_of(&action),
            id,
            out: out.clone(),
            blocks: self.blocks.clone(),
            keeper: self.keeper.clone(),
            ping: Some(ping.clone()),
        };
        let spawned = std::thread::Builder::new()
            .name("strand-exec".into())
            .stack_size(STACK)
            .spawn(move || {
                let (failed, exit, stopped) = run(&req.argv, &h, grace, &cx);
                // After its last line: logic takes them with its end.
                let taken = lock(&cx.out).take();
                lock(&ended).push(Ended {
                    action: req.action,
                    failed,
                    exit,
                    stopped,
                    taken,
                });
                ping.ping();
            });
        match spawned {
            Ok(_) => {
                self.changed.insert(action.clone());
                self.running.insert(
                    action,
                    Detached {
                        started: SystemTime::now(),
                        halt,
                        out,
                        announced: false,
                    },
                );
            }
            Err(e) => lock(&self.ended).push(Ended {
                action,
                failed: Some(format!("no thread to start it on: {e}")),
                exit: -1,
                stopped: false,
                taken: Taken::default(),
            }),
        }
    }

    /// (proc-run) What each job running printed since the last call (its
    /// start announced first, by [`Execs::runs`]).
    pub(super) fn lines(&mut self) -> Vec<(String, Taken)> {
        let mut out = Vec::new();
        for (action, d) in &self.running {
            if !d.announced {
                continue;
            }
            let taken = lock(&d.out).take();
            if !taken.is_empty() {
                out.push((action.clone(), taken));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// (flex) Runs `req`'s command, its output kept for `reply`.
    fn fetch(&mut self, req: ExecRequest, reply: ExecReply) {
        let id = self.next_id;
        self.next_id += 1;
        let halt = Arc::new(Halt::new(false));
        let (outputs, ping) = (self.outputs.clone(), self.ping.clone());
        let (h, timeout, grace) = (halt.clone(), self.timeout, self.grace);
        let argv = req.argv.clone();
        let ok = self.ok_of(&req.action);
        let spawned = std::thread::Builder::new()
            .name("strand-exec".into())
            .stack_size(STACK)
            .spawn(move || {
                let out = capture(&argv, &ok, &h, timeout, grace);
                lock(&outputs).push((id, out));
                ping.ping();
            });
        if let Err(e) = spawned {
            reply.answer(Err(format!("no thread to start it on: {e}")));
            return;
        }
        self.changed.insert(req.action.clone());
        self.fetching.push(Fetching {
            id,
            action: req.action,
            argv: req.argv,
            reply,
            halt,
            started: SystemTime::now(),
        });
    }

    /// (flex) The commands of actions with a result that ended since the
    /// last call, each one's output read as its type and answering its
    /// call (`read` converts); the ones whose call was dropped are ended.
    pub(super) fn answered(
        &mut self,
        read: &dyn Fn(&ExecReply, &str) -> Result<strand_compiler::vm::Value, String>,
    ) -> Vec<Answered> {
        let outputs = std::mem::take(&mut *lock(&self.outputs));
        let mut out = Vec::new();
        for (id, output) in outputs {
            let Some(i) = self.fetching.iter().position(|f| f.id == id) else {
                continue;
            };
            let f = self.fetching.swap_remove(i);
            self.changed.insert(f.action.clone());
            if f.reply.abandoned() {
                continue;
            }
            let r = output.out.and_then(|text| read(&f.reply, &text));
            let failed = r.as_ref().err().cloned();
            f.reply.answer(r.clone());
            out.push(Answered {
                action: f.action,
                failed,
                exit: output.exit,
                argv: f.argv,
                answer: r,
            });
        }
        // Calls dropped while their command runs: their threads end them.
        let changed = &mut self.changed;
        self.fetching.retain(|f| {
            if !f.reply.abandoned() {
                return true;
            }
            f.halt.drop_call();
            changed.insert(f.action.clone());
            // Its thread still reports; that output is dropped above.
            false
        });
        out
    }

    /// The commands that ended since the last call; each one's queued
    /// re-run starts.
    pub(super) fn ended(&mut self) -> Vec<Ended> {
        let ended = std::mem::take(&mut *lock(&self.ended));
        for e in &ended {
            if self.running.remove(&e.action).is_some() {
                self.changed.insert(e.action.clone());
                if let Some(next) = self.queued.remove(&e.action) {
                    self.spawn(next);
                }
            }
        }
        ended
    }

    /// (lockmon) Wakes the loop for one more step.
    pub(super) fn wake(&self) {
        self.ping.ping();
    }

    /// (lockmon) The runs that changed since the last call: whether a
    /// command of each action runs (a detached one, or any with a
    /// result), and when the one running longest started.
    pub(super) fn runs(&mut self) -> Vec<RunState> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(|action| {
                let reset = self
                    .running
                    .get_mut(&action)
                    .is_some_and(|d| !std::mem::replace(&mut d.announced, true));
                let started = self
                    .running
                    .get(&action)
                    .map(|d| d.started)
                    .into_iter()
                    .chain(
                        self.fetching
                            .iter()
                            .filter(|f| f.action == action)
                            .map(|f| f.started),
                    )
                    .min();
                RunState {
                    running: started.is_some(),
                    started,
                    action,
                    reset,
                }
            })
            .collect()
    }
}

/// Runs `argv` (the program, then each argument as it is) to its end, or
/// (lockmon) until `halt` stops it ([`stop_group`], `grace` at each
/// step): `None` when it exited with a status `cx.ok` lists or was
/// stopped, else why not (with why, from what it printed); its exit code
/// ([`Captured::exit`]); and whether it was stopped. (proc-run) What it
/// prints goes to `cx.out`, logic woken at most once a frame.
pub(super) fn run(
    argv: &[String],
    halt: &Halt,
    grace: Duration,
    cx: &JobCx,
) -> (Option<String>, i32, bool) {
    let Some((prog, args)) = argv.split_first() else {
        return (Some("it has no command".into()), -1, false);
    };
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Some(format!("could not start `{prog}`: {e}")), -1, false),
    };
    let mut reader = Reader::new(
        child.stdout.take().map(OwnedFd::from),
        child.stderr.take().map(OwnedFd::from),
    );
    // (proc-keeper) Its pipes outlive strand.
    cx.keeper.hold(cx.id, &reader.fds());
    let mut job = Job {
        cx,
        framers: [Framer::new(), Framer::new()],
        tail: Tail::new(),
        block: None,
    };
    let stopped = follow(&child, halt, &mut reader, &mut job);
    if stopped {
        stop_group(i32::try_from(child.id()).unwrap_or(0), grace);
    }
    // What it wrote before it ended, up to its pipes' end or the grace:
    // a process it left in the background may hold them for good (the
    // keeper reads them on).
    reader.drain_until(Instant::now() + proc::STDERR_GRACE, &mut |s, b| {
        job.read(s, b)
    });
    job.finish();
    cx.keeper.release(cx.id);
    drop(reader);
    match child.wait() {
        Ok(s) if stopped => (None, exit_code(s), true),
        Ok(s) if svc::status_ok(s, &cx.ok) => {
            if !s.success()
                && let Some(why) = job.tail.reason()
            {
                log::info!("action {}: `{prog}` ended ({s}): {why}", cx.action);
            }
            (None, exit_code(s), false)
        }
        Ok(s) => (
            Some(job.tail.explain(&format!("`{prog}` failed ({s})"))),
            exit_code(s),
            false,
        ),
        Err(e) => (
            Some(format!("`{prog}` could not be waited for: {e}")),
            -1,
            stopped,
        ),
    }
}

/// (proc-run) A job's output as its thread frames it.
struct Job<'a> {
    cx: &'a JobCx,
    framers: [Framer; 2],
    tail: Tail,
    /// The action's block, as last looked up.
    block: Option<BlockSpec>,
}

impl Job<'_> {
    /// What `stream` gave (`None`: its end): its lines framed into the
    /// ring and the reason's tail, stdout's into the block.
    fn read(&mut self, stream: Stream, bytes: Option<&[u8]>) {
        // As it stands now: a reload may have declared it afresh while
        // this thread slept.
        if !stream.is_err() {
            self.block = self.cx.block();
        }
        let mut t = lock(&self.cx.out);
        let (cx, tail, block) = (self.cx, &mut self.tail, self.block.as_ref());
        let f = &mut self.framers[usize::from(stream.is_err())];
        let mut commit = |l: Framed| {
            let text = l.text();
            tail.push(stream, &text);
            if stream.is_err() {
                log::debug!("action {}: {text}", cx.action);
            }
            t.ring.end(stream, &text);
            if stream.is_err() || l.over || l.is_blank() {
                return;
            }
            let Some(b) = block else { return };
            let values = svc::read_line(&b.fields, &text);
            if t.fields.len() < values.len() {
                t.fields.resize(values.len(), None);
            }
            for (slot, v) in t.fields.iter_mut().zip(&values) {
                if v.is_some() {
                    slot.clone_from(v);
                }
            }
            if svc::hears_lines(b.spec) {
                svc::sent_line(b.spec, text.len());
                let line = values
                    .into_iter()
                    .map(|v| v.unwrap_or(Data::Null))
                    .collect();
                t.heard.push((b.spec, line));
            }
        };
        match bytes {
            Some(b) => f.push(b, &mut commit),
            None => f.finish(&mut commit),
        }
        if f.take_changed() {
            let open = f.open().map(proc::decode);
            t.ring.open(stream, open.as_deref());
        }
    }

    /// Its output ended: each open line ends as it reads.
    fn finish(&mut self) {
        for s in [Stream::Out, Stream::Err] {
            self.read(s, None);
        }
        lock(&self.cx.out).ring.close();
    }
}

/// (proc-run) Reads `child`'s output until it ends (false) or `halt`
/// asks it to stop (true), asleep on its pipes, its pidfd and `halt`'s
/// wake; logic is woken at most once a [`LISTEN_FLUSH`] while lines
/// come. stdout is not read while its block's `line` handlers have
/// [`proc::BACKLOG_LINES`] waiting ([`svc::backlogged`]).
fn follow(
    child: &std::process::Child,
    halt: &Halt,
    reader: &mut Reader,
    job: &mut Job<'_>,
) -> bool {
    use rustix::fd::AsFd as _;
    let pid = i32::try_from(child.id()).unwrap_or(0);
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_child(child),
        rustix::process::PidfdFlags::empty(),
    )
    .ok();
    let mut sent = Instant::now();
    let mut unsent = false;
    loop {
        if exited(pid) {
            return false;
        }
        if halt.why() == STOPPED {
            return true;
        }
        job.block = job.cx.block();
        let read_out = job.block.as_ref().is_none_or(|b| !svc::backlogged(b.spec));
        let mut wakes = Vec::with_capacity(2);
        if let Some(p) = &pidfd {
            wakes.push(p.as_fd());
        }
        if let Some(w) = &halt.wake {
            wakes.push(w.as_fd());
        }
        let flush = LISTEN_FLUSH.saturating_sub(sent.elapsed());
        let timeout = if !read_out {
            Some(svc::BACKLOG_TICK)
        } else if unsent {
            Some(flush)
        } else if wakes.len() < 2 {
            Some(SLOW_TICK)
        } else {
            None
        };
        let mut got = false;
        let stepped = reader.step(&wakes, timeout, read_out, &mut |s, b| {
            got = true;
            job.read(s, b);
        });
        if stepped.is_err() && !got {
            std::thread::sleep(TICK);
        }
        halt.drain();
        unsent |= got;
        if unsent && sent.elapsed() >= LISTEN_FLUSH {
            job.cx.ping();
            sent = Instant::now();
            unsent = false;
        }
    }
}

/// How often a detached command's thread looks at it when the kernel
/// gives no pidfd or eventfd to sleep on.
const SLOW_TICK: Duration = Duration::from_millis(250);

/// (flex) What a command with a result left: what it printed or why it
/// has no value, and its exit code (`-1` when it did not start or did
/// not end by itself, `128 + n` when signal `n` ended it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Captured {
    pub out: Result<String, String>,
    pub exit: i32,
}

/// The exit code of `status` as a shell reports it.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .or_else(|| status.signal().map(|n| 128 + n))
        .unwrap_or(-1)
}

/// (flex) Runs `argv` and returns what it printed on stdout (at most
/// [`MAX_DOCUMENT`] bytes, framed as a terminal shows it), or why not:
/// (proc-reason) a status `ok` does not list is a failure with why, from
/// its stderr (`proc::Tail`). It is given `timeout` to print
/// its output and end, as a `from poll` command is (a command that
/// hangs, or leaves a child holding its stdout, would otherwise hold its
/// call pending for ever); `halt` asked (its call was dropped, or
/// (lockmon) `rec.stop()` with `grace` at each step) ends it at once.
/// This thread alone ends its process group, before it reaps the
/// command, so the group's id cannot have been reused by then.
pub(super) fn capture(
    argv: &[String],
    ok: &[i32],
    halt: &Halt,
    timeout: Duration,
    grace: Duration,
) -> Captured {
    let failed = |why: String| Captured {
        out: Err(why),
        exit: -1,
    };
    let Some((prog, args)) = argv.split_first() else {
        return failed("it has no command".into());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    strand_services::child::child_env(&mut cmd, None);
    // SAFETY: only async-signal-safe calls, as `pre_exec` requires. The death signal ends it with this thread,
    // which lives until it ends: when strand dies.
    unsafe {
        cmd.pre_exec(|| {
            strand_services::child::restore_in_child();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(format!("could not start `{prog}`: {e}")),
    };
    let group = i32::try_from(child.id()).unwrap_or(0);
    let deadline = Instant::now() + timeout;
    let mut reader = Reader::new(
        child.stdout.take().map(OwnedFd::from),
        child.stderr.take().map(OwnedFd::from),
    );
    let mut out = Vec::new();
    let mut err = Framer::new();
    let mut tail = Tail::new();
    let mut read = |s: Stream, bytes: Option<&[u8]>| {
        if !s.is_err() {
            if let Some(b) = bytes {
                out.extend_from_slice(b);
            }
            return;
        }
        let mut keep = |l: Framed| tail.push(s, &l.text());
        match bytes {
            Some(b) => err.push(b, &mut keep),
            None => err.finish(&mut keep),
        }
    };
    let reading = read_until(&mut reader, &mut read, deadline, halt);
    let ended = |child: &mut std::process::Child, why: String| {
        end_group(child, group);
        Captured {
            out: Err(why),
            exit: -1,
        }
    };
    // (lockmon) `rec.stop()`: SIGINT first; the call's error says so.
    let stopped = |child: &mut std::process::Child| {
        stop_group(group, grace);
        let exit = child.wait().map_or(-1, exit_code);
        Captured {
            out: Err(format!("`{prog}` was stopped")),
            exit,
        }
    };
    match reading {
        Reading::Done => {}
        Reading::TooMuch => {
            return ended(
                &mut child,
                format!("`{prog}` printed more than {MAX_DOCUMENT} bytes; ended"),
            );
        }
        Reading::Halted if halt.why() == STOPPED => return stopped(&mut child),
        Reading::Halted => {
            return ended(&mut child, format!("`{prog}` was no longer needed"));
        }
        Reading::TimedOut => {
            return ended(
                &mut child,
                format!("`{prog}` timed out after {}", span(timeout)),
            );
        }
    }
    // Its stdout closed: it ends now, or within what is left (its stderr
    // read meanwhile).
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if halt.why() == STOPPED => return stopped(&mut child),
            Ok(None) if halt.why() != GO => {
                return ended(&mut child, format!("`{prog}` was no longer needed"));
            }
            Ok(None) if Instant::now() >= deadline => {
                return ended(
                    &mut child,
                    format!("`{prog}` timed out after {}", span(timeout)),
                );
            }
            Ok(None) if reader.done() => std::thread::sleep(TICK),
            Ok(None) => {
                let _ = reader.step(&[], Some(TICK), true, &mut read);
            }
            Err(e) => return failed(format!("`{prog}` could not be waited for: {e}")),
        }
    };
    // What it wrote on stderr before it ended, up to the pipe's end or the
    // grace: a process it left in the background may hold it.
    reader.drain_until(Instant::now() + proc::STDERR_GRACE, &mut read);
    err.finish(&mut |l| tail.push(Stream::Err, &l.text()));
    drop(reader);
    let exit = exit_code(status);
    if out.len() > MAX_DOCUMENT {
        return Captured {
            out: Err(format!("`{prog}` printed more than {MAX_DOCUMENT} bytes")),
            exit,
        };
    }
    if !svc::status_ok(status, ok) {
        // With nothing on stderr, the end of what it printed.
        for line in proc::decode(&Framer::whole(&out)).lines() {
            tail.push(Stream::Out, line);
        }
        return Captured {
            out: Err(tail.explain(&format!("`{prog}` failed ({status})"))),
            exit,
        };
    }
    if !status.success()
        && let Some(why) = tail.reason()
    {
        log::info!("`{prog}` ended ({status}): {why}");
    }
    Captured {
        out: Ok(proc::decode(&Framer::whole(&out))),
        exit,
    }
}

/// `10 s`, or `300 ms` below a second.
fn span(d: Duration) -> String {
    if d >= Duration::from_secs(1) {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// How often a command's thread looks at its call and its deadline.
const TICK: Duration = Duration::from_millis(20);

/// How reading a command's output ended.
enum Reading {
    /// At its stdout's end.
    Done,
    /// Past [`MAX_DOCUMENT`] bytes.
    TooMuch,
    /// Its call was dropped, or (lockmon) it was stopped.
    Halted,
    /// Its deadline passed.
    TimedOut,
}

/// Reads `reader` into `read` (stderr too) until stdout's end,
/// [`MAX_DOCUMENT`] bytes of stdout, `deadline` or `halt`, waiting at
/// most [`TICK`] at a time.
fn read_until(
    reader: &mut Reader,
    read: &mut dyn FnMut(Stream, Option<&[u8]>),
    deadline: Instant,
    halt: &Halt,
) -> Reading {
    let mut printed = 0usize;
    loop {
        if halt.why() != GO {
            return Reading::Halted;
        }
        if printed > MAX_DOCUMENT {
            return Reading::TooMuch;
        }
        if !reader.is_open(Stream::Out) {
            return Reading::Done;
        }
        let now = Instant::now();
        if now >= deadline {
            return Reading::TimedOut;
        }
        let wait = (deadline - now).min(TICK);
        let stepped = reader.step(&[], Some(wait), true, &mut |s, b| {
            if !s.is_err() {
                printed += b.map_or(0, <[u8]>::len);
            }
            read(s, b);
        });
        if stepped.is_err() {
            std::thread::sleep(TICK.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

/// Ends `child`'s process group `pgid` and reaps it: SIGTERM, then
/// SIGKILL once [`KILL_GRACE`] has passed and the group's leader has not
/// ended. The leader is not reaped until then (it is only looked at with
/// `WNOWAIT`), so `pgid` names this group throughout.
fn end_group(child: &mut std::process::Child, pgid: i32) {
    if pgid > 1 {
        // SAFETY: kill(2) on our child's own process group, whose
        // leader we have not reaped; no memory is involved.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let until = Instant::now() + KILL_GRACE;
        while Instant::now() < until && !exited(pgid) {
            std::thread::sleep(TICK);
        }
        // Members it left behind (a child holding its stdout) go too.
        // SAFETY: as above; the leader is still unreaped.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    let _ = child.wait();
}

/// (lockmon) `rec.stop()` on the command leading process group `pgid`,
/// our unreaped child (the caller reaps it after): SIGINT to the group,
/// then SIGTERM once `grace` has passed and the leader has not ended,
/// then SIGKILL after another `grace`. A leader that ends at a step ends
/// the escalation; the group's other members had each signal sent so
/// far, as a terminal's Ctrl-C would send it.
fn stop_group(pgid: i32, grace: Duration) {
    if pgid <= 1 {
        return;
    }
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: kill(2) on our child's own process group, whose leader
        // we have not reaped; no memory is involved.
        unsafe { libc::kill(-pgid, sig) };
        let until = Instant::now() + grace;
        while Instant::now() < until && !exited(pgid) {
            std::thread::sleep(TICK);
        }
        if exited(pgid) {
            return;
        }
    }
    // SAFETY: as above; the leader is still unreaped.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// Whether process `pid` (our child) has ended, without reaping it.
fn exited(pid: i32) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return true;
    };
    // SAFETY: waitid(2) with WNOWAIT only reads our child's state into
    // `info`, which outlives the call.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r == 0 && info.si_pid() != 0
    }
}

impl Shell {
    /// After the tick's diff: starts the commands handlers asked for,
    /// and reports the ones that failed.
    pub(super) fn run_execs(&mut self) {
        for req in self.inst.take_execs() {
            // (lockmon) A stop is the handler's, replayed with it; the
            // commands' ends are recorded as they come.
            if req.stop {
                self.execs.start(req);
                continue;
            }
            // (report) Its start, never its arguments (their count).
            let t = strand_core::flight::now_ns();
            let argc = req.argv.len().saturating_sub(1);
            self.recorder
                .borrow_mut()
                .action(t, &req.action, argc, true, 0);
            // (action-error) A new try: its last failure no longer stands.
            self.inst.set_action_error(&req.action, None);
            self.execs.start(req);
        }
        // (proc-run) A job's end: its block's last lines, then its last
        // `lines`, `code`, `error` and `running` in one update (spec.md
        // 5.7's end order).
        for e in self.execs.ended() {
            let t = strand_core::flight::now_ns();
            {
                let mut rec = self.recorder.borrow_mut();
                if let Some(why) = &e.failed {
                    rec.action_failed(t, &e.action, why);
                }
                rec.action(t, &e.action, 0, false, e.exit);
            }
            self.feed_block(&e.action, false, &e.taken);
            let (keep_from, lines) = e.taken.update();
            self.inst.update_action_run(
                &e.action,
                RunUpdate {
                    keep_from,
                    lines,
                    code: (e.exit >= 0).then_some(i64::from(e.exit)),
                    error: Some(e.failed.clone()),
                    running: Some((false, None)),
                    ..RunUpdate::default()
                },
            );
            if e.stopped {
                log::info!("action {}: stopped (exit {})", e.action, e.exit);
            }
            if let Some(why) = e.failed {
                self.exec_notice(&e.action, &why);
            }
        }
        // (flex) Results: the call's `.error` says why one failed; the
        // log keeps it too, with no notice.
        let read = |reply: &ExecReply, out: &str| {
            crate::services::custom::read_output(reply.types(), reply.ty(), out)
        };
        for a in self.execs.answered(&read) {
            let t = strand_core::flight::now_ns();
            let mut rec = self.recorder.borrow_mut();
            rec.action(t, &a.action, 0, false, a.exit);
            // (flex review) What answered the call, so a replay answers it.
            rec.exec_answered(t, &a.action, &a.argv, &a.answer);
            drop(rec);
            if let Some(why) = a.failed {
                log::info!("action {}: {why}", a.action);
            }
        }
        // (lockmon) `rec.running` and `rec.started` as the commands now
        // stand, (proc-run) a job started since resetting its run; the
        // loop steps once more to show them.
        let runs = self.execs.runs();
        for r in &runs {
            let started = r
                .started
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local());
            if r.reset {
                self.feed_block(&r.action, true, &Taken::default());
            }
            self.inst.update_action_run(
                &r.action,
                RunUpdate {
                    reset: r.reset,
                    running: Some((r.running, started)),
                    ..RunUpdate::default()
                },
            );
        }
        // (proc-run) What the jobs running printed since.
        let printed = self.execs.lines();
        for (action, taken) in &printed {
            self.feed_block(action, false, taken);
            let (keep_from, lines) = taken.update();
            if keep_from > 0 || !lines.is_empty() {
                self.inst.update_action_run(
                    action,
                    RunUpdate {
                        keep_from,
                        lines,
                        ..RunUpdate::default()
                    },
                );
            }
        }
        if !runs.is_empty() || !printed.is_empty() {
            self.execs.wake();
        }
    }

    /// (proc-run) What a job of `action` read into its block: its fields
    /// and the lines its `line` handlers hear (`reset`: a run started).
    fn feed_block(&self, action: &str, reset: bool, taken: &Taken) {
        let Some(real) = &self.real else {
            // No host to take them: they are not waited for.
            for (spec, _) in &taken.heard {
                svc::took_line(*spec);
            }
            return;
        };
        if !reset && taken.fields.iter().all(Option::is_none) && taken.heard.is_empty() {
            return;
        }
        real.custom.feed_block(
            self.inst.runtime(),
            action,
            reset,
            &taken.fields,
            taken.heard.clone(),
        );
    }

    /// Action `action`'s command failed: a log line and a `strand
    /// watch` notice.
    fn exec_notice(&mut self, action: &str, why: &str) {
        let text = format!("action {action}: {why}");
        log::warn!("{text}");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// (flex) A compiled program declaring `Probe`, and the type `ty`
    /// names in it.
    fn program(
        ty: &str,
    ) -> (
        Arc<strand_compiler::lower::Program>,
        strand_compiler::ty::Ty,
    ) {
        program_with(
            "type Probe { load: float; name: text }\nenum Level { low, high }\n",
            ty,
        )
    }

    /// An action `probe() -> ty` beside the declarations `decls`.
    fn program_with(
        decls: &str,
        ty: &str,
    ) -> (
        Arc<strand_compiler::lower::Program>,
        strand_compiler::ty::Ty,
    ) {
        let mut map = strand_compiler::SourceMap::new();
        let src = format!("{decls}permit exec \"sh\"\naction probe() -> {ty} [\"sh\"]\n");
        map.add("p.strand", src);
        let c = strand_compiler::compile(&map);
        assert_eq!(c.errors(), 0, "{:?}", c.diagnostics);
        let def = c
            .program
            .defs
            .iter()
            .find(|d| d.name == "probe")
            .expect("probe");
        let strand_compiler::ty::Ty::Fn(sig) = &def.ty else {
            panic!("{:?}", def.ty);
        };
        let strand_compiler::ty::Ty::Async(t) = &sig.ret else {
            panic!("{:?}", sig.ret);
        };
        let t = (**t).clone();
        let p =
            strand_compiler::lower::lower(&c.program, strand_compiler::schema::Schema::builtin());
        (Arc::new(p), t)
    }

    fn sh(script: &str) -> Vec<String> {
        ["sh", "-c", script].map(String::from).to_vec()
    }

    /// (flex) What a command prints, read as its action's declared type:
    /// text as printed (trimmed), numbers and enums from text, records
    /// from JSON or `key=value` lines, nothing at all `null` for an
    /// optional type; what does not convert is an error.
    #[test]
    fn a_commands_output_is_read_as_its_declared_type() {
        use strand_compiler::vm::Value;
        let read = |ty: &str, out: &str| {
            let (p, t) = program(ty);
            crate::services::custom::read_output(&p.types, &t, out)
        };
        assert_eq!(read("text", "  4\n"), Ok(Value::text("4")));
        assert_eq!(read("text", "{\"a\": 1}\n"), Ok(Value::text("{\"a\": 1}")));
        assert_eq!(read("float", "42.5\n"), Ok(Value::float(42.5)));
        assert_eq!(read("int", "7"), Ok(Value::int(7)));
        assert!(matches!(read("Level", "high\n"), Ok(Value::Enum(_, _))));
        assert_eq!(read("float?", "\n"), Ok(Value::Null));
        assert!(read("float", "many").is_err_and(|e| e.contains("not a `float`")));
        let (p, t) = program("Probe");
        let rec = crate::services::custom::read_output(
            &p.types,
            &t,
            "{\"load\": 0.25, \"name\": \"cpu\", \"extra\": true}",
        )
        .unwrap();
        assert_eq!(rec.field(&p.types, "load"), Some(&Value::float(0.25)));
        assert_eq!(rec.field(&p.types, "name"), Some(&Value::text("cpu")));
        let rec =
            crate::services::custom::read_output(&p.types, &t, "load=0.5\nname=\"gpu\"\n").unwrap();
        assert_eq!(rec.field(&p.types, "load"), Some(&Value::float(0.5)));
        assert_eq!(rec.field(&p.types, "name"), Some(&Value::text("gpu")));
        let (p, t) = program("[int]");
        assert_eq!(
            crate::services::custom::read_output(&p.types, &t, "[1, 2, 3]"),
            Ok(Value::list(vec![
                Value::int(1),
                Value::int(2),
                Value::int(3)
            ]))
        );
    }

    /// (proc-format) A record read from output naming none of its
    /// fields (a pacman.log line, `error: x`, `key=value here`): its
    /// `text` fields read the output as written; one naming a field is
    /// data as before.
    #[test]
    fn a_record_reads_output_naming_no_field_as_written() {
        use strand_compiler::vm::Value;
        let (p, t) = program_with("type Msg { line: text; level: text? }\n", "Msg");
        let read = |out: &str| crate::services::custom::read_output(&p.types, &t, out).unwrap();
        for out in [
            "[2026-11-08T16:04:17-0700] [ALPM] transaction completed",
            "error: failed to commit transaction (conflicting files)",
            "key=value here",
            "{\"kind\": \"out\"}",
        ] {
            let rec = read(out);
            assert_eq!(
                rec.field(&p.types, "line"),
                Some(&Value::text(out)),
                "{out}"
            );
            assert_eq!(rec.field(&p.types, "level"), Some(&Value::Null), "{out}");
        }
        let rec = read("level=warn\nline=disk almost full\n");
        assert_eq!(
            rec.field(&p.types, "line"),
            Some(&Value::text("disk almost full"))
        );
        assert_eq!(rec.field(&p.types, "level"), Some(&Value::text("warn")));
    }

    /// (doc-formats) `nmap -oX -` through `-> Scan`: XML read as the
    /// declared type, the whole root as it; a Unix-seconds attribute as a
    /// `Date`; one host's ports and several hosts as lists.
    #[test]
    fn a_commands_xml_is_read_as_its_declared_type() {
        use strand_compiler::vm::Value;
        let (p, t) = program_with(
            "type State { state: text }\ntype Service { name: text; product: text? }\n\
             type Port { protocol: text; portid: int; state: State; service: Service? }\n\
             type Ports { port: [Port] }\ntype Address { addr: text; addrtype: text }\n\
             type Host { starttime: Date; status: State; address: [Address]; ports: Ports? }\n\
             type Scan { start: Date; version: text; host: [Host] }\n",
            "Scan",
        );
        let nmap = include_str!("../../../strand-services/tests/fixtures/xml/nmap.xml");
        let scan = crate::services::custom::read_output(&p.types, &t, nmap).unwrap();
        let types = &p.types;
        let f = |v: &Value, n: &str| v.field(types, n).cloned().unwrap();
        let list = |v: Value| match v {
            Value::List(l) => l.iter().cloned().collect::<Vec<_>>(),
            v => panic!("{v:?}"),
        };
        assert_eq!(f(&f(&scan, "start"), "year"), Value::int(2026));
        assert_eq!(f(&scan, "version"), Value::text("7.98"));
        let hosts = list(f(&scan, "host"));
        assert_eq!(hosts.len(), 2);
        assert_eq!(f(&f(&hosts[0], "status"), "state"), Value::text("up"));
        assert_eq!(list(f(&hosts[0], "address")).len(), 2);
        let ports = list(f(&f(&hosts[0], "ports"), "port"));
        assert_eq!(ports.len(), 2);
        assert_eq!(f(&ports[0], "portid"), Value::int(22));
        assert_eq!(f(&f(&ports[0], "state"), "state"), Value::text("open"));
        assert_eq!(
            f(&f(&ports[1], "service"), "product"),
            Value::text("lighttpd")
        );
        assert_eq!(f(&hosts[1], "ports"), Value::Null, "a host down has none");
        assert_eq!(
            list(f(&hosts[1], "address")).len(),
            1,
            "one is a list of one"
        );
        // `-> text` is still what it printed.
        let (p, t) = program("text");
        assert_eq!(
            crate::services::custom::read_output(&p.types, &t, "<a>1</a>"),
            Ok(Value::text("<a>1</a>"))
        );
    }

    /// (doc-formats) `action now() -> Date ["date", "-Iseconds"]`: the
    /// date text a command prints reads as a `Date`.
    #[test]
    fn an_action_reads_a_date_it_prints() {
        use strand_compiler::vm::Value;
        let read = |ty: &str, out: &str| {
            let (p, t) = program(ty);
            let v = crate::services::custom::read_output(&p.types, &t, out);
            (p, v)
        };
        let (p, v) = read("Date", "2026-11-07T21:05:09+01:00\n");
        let v = v.unwrap();
        assert_eq!(v.field(&p.types, "year"), Some(&Value::int(2026)));
        assert!(matches!(v.field(&p.types, "hour"), Some(Value::Num(..))));
        let (p, v) = read("Date", "2026-11-07\n");
        assert_eq!(v.unwrap().field(&p.types, "hour"), Some(&Value::Null));
        assert_eq!(read("Date?", "\n").1, Ok(Value::Null));
        assert!(
            read("Date", "soon")
                .1
                .is_err_and(|e| e.contains("not a `Date`"))
        );
        // `date +%Y%m%d` prints digits the document reads as a JSON
        // number: they are still YYYYMMDD, not Unix seconds (1970), and
        // fewer digits are no date.
        let (p, v) = read("Date", "20261107\n");
        let v = v.unwrap();
        assert_eq!(v.field(&p.types, "year"), Some(&Value::int(2026)));
        assert_eq!(v.field(&p.types, "month"), Some(&Value::int(11)));
        assert_eq!(v.field(&p.types, "day"), Some(&Value::int(7)));
        assert!(read("Date", "2026\n").1.is_err());
        let (p, v) = read("Date", "1794084315\n");
        assert_eq!(v.unwrap().field(&p.types, "year"), Some(&Value::int(2026)));
    }

    /// (flex) A real command's output answers its call: run on a thread,
    /// read on the logic thread as the declared type; a failing command
    /// is the call's error; a dropped call's command is ended.
    #[test]
    fn a_command_with_a_result_answers_its_call() {
        use strand_compiler::vm::Value;
        let loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        loop_.handle().insert_source(source, |_, _, _| {}).unwrap();
        let mut execs = Execs::new(ping);
        let read = |reply: &ExecReply, out: &str| {
            crate::services::custom::read_output(reply.types(), reply.ty(), out)
        };
        let (p, probe) = program("Probe");
        let (_, text) = program("text");
        let call = |argv: Vec<String>, ty: &strand_compiler::ty::Ty| {
            let reply = ExecReply::new(p.clone(), ty.clone());
            let req = ExecRequest {
                action: "probe".into(),
                argv,
                reply: Some(reply.clone()),
                stop: false,
            };
            (reply, req)
        };
        let (ok, req) = call(
            sh("printf '{\"load\": 0.75, \"name\": \"%s\"}' \"$(echo cpu)\""),
            &probe,
        );
        let ok_wait = ok.wait();
        execs.start(req);
        let (bad, req) = call(sh("echo nope; exit 3"), &text);
        let bad_wait = bad.wait();
        execs.start(req);
        let (gone, req) = call(sh("echo $$ > \"$0\"; exec sleep 30"), &text);
        let dir = std::env::temp_dir().join(format!("strand-exec-r-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let mut req = req;
        req.argv.push(pid_file.to_str().unwrap().to_string());
        let gone_wait = gone.wait();
        execs.start(req);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut answered = Vec::new();
        while answered.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "{answered:?}");
            answered.extend(execs.answered(&read));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let rec = ok.peek().unwrap().unwrap();
        assert_eq!(rec.field(&p.types, "load"), Some(&Value::float(0.75)));
        assert_eq!(rec.field(&p.types, "name"), Some(&Value::text("cpu")));
        // (proc-reason) With nothing on stderr, why is the end of its
        // stdout.
        assert_eq!(
            bad.peek(),
            Some(Err("`sh` failed (exit status: 3): nope".to_string()))
        );
        assert!(
            answered.iter().any(|a| a.action == "probe"
                && a.failed.as_deref() == Some("`sh` failed (exit status: 3): nope")
                && a.exit == 3),
            "{answered:?}"
        );
        drop((ok_wait, bad_wait));
        // The third runs until its call is dropped; then it is ended.
        while std::fs::read_to_string(&pid_file).map_or(true, |s| s.trim().is_empty()) {
            assert!(std::time::Instant::now() < deadline, "it never started");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone.peek().is_none());
        drop(gone_wait);
        assert!(gone.abandoned());
        assert!(execs.answered(&read).is_empty());
        // SAFETY: kill(2) with signal 0 only asks whether it is there.
        let alive = || unsafe { libc::kill(pid, 0) } == 0 && !zombie(pid);
        while alive() {
            assert!(
                std::time::Instant::now() < deadline,
                "the dropped call's command runs on"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(execs.fetching.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (flex review) A command with a result has a deadline, as a `from
    /// poll` command has: one that hangs, and one that leaves a child
    /// holding its stdout, answer with a timeout error once it passes,
    /// and their process group (the child too) is ended.
    #[test]
    fn a_command_with_a_result_times_out() {
        let loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        loop_.handle().insert_source(source, |_, _, _| {}).unwrap();
        let mut execs = Execs::new(ping);
        execs.timeout = Duration::from_millis(300);
        let read = |reply: &ExecReply, out: &str| {
            crate::services::custom::read_output(reply.types(), reply.ty(), out)
        };
        let (p, text) = program("text");
        let dir = std::env::temp_dir().join(format!("strand-exec-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("bg");
        let mut waits = Vec::new();
        let mut replies = Vec::new();
        for argv in [
            sh("exec sleep 60"),
            vec![
                "sh".into(),
                "-c".into(),
                "sleep 60 & echo $! > \"$0\"; echo hi".into(),
                pid_file.to_str().unwrap().into(),
            ],
        ] {
            let reply = ExecReply::new(p.clone(), text.clone());
            waits.push(reply.wait());
            replies.push(reply.clone());
            execs.start(ExecRequest {
                action: "probe".into(),
                argv,
                reply: Some(reply),
                stop: false,
            });
        }
        let start = Instant::now();
        let mut answered = Vec::new();
        while answered.len() < 2 {
            assert!(start.elapsed() < Duration::from_secs(20), "{answered:?}");
            answered.extend(execs.answered(&read));
            std::thread::sleep(Duration::from_millis(10));
        }
        let took = start.elapsed();
        for r in &replies {
            assert_eq!(
                r.peek(),
                Some(Err("`sh` timed out after 300 ms".to_string()))
            );
        }
        // The child that held the output was ended with its group.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: kill(2) with signal 0 only asks whether it is there.
        while unsafe { libc::kill(pid, 0) } == 0 && !zombie(pid) {
            assert!(Instant::now() < deadline, "the background child runs on");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(waits);
        let _ = std::fs::remove_dir_all(&dir);
        let mut gates = strand_test_support::gate::Gates::default();
        gates.check(took < Duration::from_secs(5), || {
            format!("both timed out after {took:?}")
        });
        gates.hold();
    }

    /// [`run`] to its end.
    fn run_(argv: &[String]) -> (Option<String>, i32, bool) {
        run(argv, &Halt::new(true), STOP_GRACE, &job_cx("run", &[0]))
    }

    /// A job's context with no loop to wake and no block.
    fn job_cx(action: &str, ok: &[i32]) -> JobCx {
        JobCx {
            action: action.into(),
            ok: ok.to_vec(),
            id: 0,
            out: Arc::default(),
            blocks: Blocks::default(),
            keeper: Arc::default(),
            ping: None,
        }
    }

    /// Whether `pid` has ended and waits to be reaped.
    fn zombie(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit(')')
                    .next()
                    .map(|r| r.trim_start().starts_with('Z'))
            })
            .unwrap_or(false)
    }

    /// A loop that ticks (its ping source registered) and an `Execs` on
    /// it, with `grace` for stopped commands.
    fn looped(grace: Duration) -> (calloop::EventLoop<'static, ()>, Execs) {
        let loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        loop_.handle().insert_source(source, |_, _, _| {}).unwrap();
        let mut execs = Execs::new(ping);
        execs.grace = grace;
        (loop_, execs)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("strand-exec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wait_for(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !cond() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// (lockmon) `rec.stop()` on a detached command: its process group
    /// gets SIGINT, then SIGTERM once the grace has passed and it still
    /// runs, then SIGKILL; the re-run queued behind it is dropped, its
    /// end is no failure, and its run reads running from its start to
    /// its end. A command that ends on SIGINT ends at the first step.
    #[test]
    fn a_stopped_command_gets_sigint_then_sigterm_then_sigkill() {
        let dir = scratch("stop");
        let log = dir.join("log");
        let pid_file = dir.join("pid");
        // Ignores SIGINT and SIGTERM (saying so): only SIGKILL ends it.
        let script = "trap 'echo int >> \"$0\"' INT; trap 'echo term >> \"$0\"' TERM; \
                      echo $$ > \"$1\"; while :; do sleep 0.05; done";
        let req = |action: &str, argv: Vec<String>| ExecRequest {
            action: action.into(),
            argv,
            reply: None,
            stop: false,
        };
        let stubborn: Vec<String> = [
            "sh",
            "-c",
            script,
            log.to_str().unwrap(),
            pid_file.to_str().unwrap(),
        ]
        .map(String::from)
        .to_vec();
        let (mut loop_, mut execs) = looped(Duration::from_millis(400));
        let before = SystemTime::now();
        execs.start(req("rec", stubborn.clone()));
        execs.start(req("rec", sh("echo rerun >> \"$0\"")));
        assert_eq!(execs.busy(), (1, 1));
        let runs = execs.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!(runs[0].running && runs[0].action == "rec");
        assert!(runs[0].started.is_some_and(|t| t >= before));
        assert!(execs.runs().is_empty(), "reported once");
        wait_for("it never started", 20, || {
            std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.trim().is_empty())
        });
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let asked = Instant::now();
        execs.start(ExecRequest {
            action: "rec".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        assert_eq!(execs.busy(), (1, 0), "the queued re-run is dropped");
        let mut ended = Vec::new();
        while ended.is_empty() {
            assert!(asked.elapsed() < Duration::from_secs(20), "it never ended");
            loop_
                .dispatch(Some(Duration::from_millis(50)), &mut ())
                .unwrap();
            ended.extend(execs.ended());
        }
        let took = asked.elapsed();
        assert!(took >= Duration::from_millis(800), "two graces: {took:?}");
        assert_eq!(ended.len(), 1);
        assert_eq!(
            (
                &*ended[0].action,
                &ended[0].failed,
                ended[0].exit,
                ended[0].stopped
            ),
            ("rec", &None, 128 + libc::SIGKILL, true)
        );
        let said = std::fs::read_to_string(&log).unwrap();
        assert_eq!(said, "int\nterm\n", "SIGINT, then SIGTERM, then SIGKILL");
        // SAFETY: kill(2) with signal 0 only asks whether it is there.
        assert!(unsafe { libc::kill(pid, 0) } != 0 || zombie(pid));
        assert_eq!(execs.busy(), (0, 0), "no re-run");
        let runs = execs.runs();
        assert_eq!(
            runs,
            [RunState {
                action: "rec".into(),
                running: false,
                started: None,
                reset: false,
            }]
        );
        // One that ends on SIGINT ends at once.
        let asked = Instant::now();
        execs.start(req("rec", sh("exec sleep 30")));
        std::thread::sleep(Duration::from_millis(100));
        execs.start(ExecRequest {
            action: "rec".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        let mut ended = Vec::new();
        while ended.is_empty() {
            assert!(asked.elapsed() < Duration::from_secs(20), "it never ended");
            loop_
                .dispatch(Some(Duration::from_millis(50)), &mut ())
                .unwrap();
            ended.extend(execs.ended());
        }
        let interrupted = asked.elapsed();
        assert_eq!(ended[0].exit, 128 + libc::SIGINT);
        assert!(ended[0].stopped && ended[0].failed.is_none());
        // A stop with nothing running does nothing.
        execs.start(ExecRequest {
            action: "idle".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        assert_eq!(execs.busy(), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
        let mut gates = strand_test_support::gate::Gates::default();
        gates.check(interrupted < Duration::from_secs(5), || {
            format!("one that heeds SIGINT ended after {interrupted:?}")
        });
        gates.hold();
    }

    /// (bead #17) Strand blocks SIGINT and SIGTERM on every thread for its
    /// signalfd, and a thread's mask outlives `exec`: a command started
    /// from such a thread must still get the signals `rec.stop()` sends.
    /// With the mask inherited it hears neither and dies at SIGKILL.
    #[test]
    fn a_stopped_command_hears_the_signals_strand_blocks_for_itself() {
        let dir = scratch("stop-mask");
        let log = dir.join("log");
        let ready = dir.join("ready");
        // Not `sh`: dash unblocks signals while it waits for a child, which
        // hides the bug. A program that keeps its mask is the one that
        // shows it, as a recorder does.
        let script = "import signal, sys, time\n\
                      def on(*_):\n    \
                      open(sys.argv[1], 'w').write('int\\n')\n    \
                      sys.exit(0)\n\
                      signal.signal(signal.SIGINT, on)\n\
                      open(sys.argv[2], 'w').close()\n\
                      while True:\n    \
                      time.sleep(0.05)\n";
        let argv: Vec<String> = [
            "python3",
            "-I",
            "-c",
            script,
            log.to_str().unwrap(),
            ready.to_str().unwrap(),
        ]
        .map(String::from)
        .to_vec();
        // As `signal_fd` does on strand's main thread. The mask is this
        // test thread's own, and the command's thread inherits it.
        // SAFETY: `set` is initialised by `sigemptyset`; the calls change
        // only this thread's mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()),
                0
            );
        }
        let (mut loop_, mut execs) = looped(Duration::from_millis(400));
        execs.start(ExecRequest {
            action: "rec".into(),
            argv,
            reply: None,
            stop: false,
        });
        wait_for("it never started", 20, || ready.exists());
        // The handler was set before `ready` was written.
        let asked = Instant::now();
        execs.start(ExecRequest {
            action: "rec".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        let mut ended = Vec::new();
        while ended.is_empty() {
            assert!(asked.elapsed() < Duration::from_secs(20), "it never ended");
            loop_
                .dispatch(Some(Duration::from_millis(50)), &mut ())
                .unwrap();
            ended.extend(execs.ended());
        }
        assert_eq!(std::fs::read_to_string(&log).ok().as_deref(), Some("int\n"));
        assert_eq!(ended[0].exit, 0, "it ended on its own: {:?}", ended[0]);
        assert!(ended[0].stopped && ended[0].failed.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (lockmon) `rec.stop()` on an action with a result: each running
    /// call's command is stopped (SIGINT first) and its call answers with
    /// an error saying so; its run reads running while any call's
    /// command runs, from the oldest one's start.
    #[test]
    fn a_stopped_call_answers_that_it_was_stopped() {
        let (mut loop_, mut execs) = looped(Duration::from_millis(300));
        let read = |reply: &ExecReply, out: &str| {
            crate::services::custom::read_output(reply.types(), reply.ty(), out)
        };
        let (p, text) = program("text");
        let mut replies = Vec::new();
        let mut waits = Vec::new();
        for _ in 0..2 {
            let reply = ExecReply::new(p.clone(), text.clone());
            waits.push(reply.wait());
            replies.push(reply.clone());
            execs.start(ExecRequest {
                action: "probe".into(),
                argv: sh("exec sleep 30"),
                reply: Some(reply),
                stop: false,
            });
            std::thread::sleep(Duration::from_millis(20));
        }
        let runs = execs.runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].running);
        let first = execs.fetching.iter().map(|f| f.started).min();
        assert_eq!(runs[0].started, first, "the oldest call's start");
        std::thread::sleep(Duration::from_millis(100));
        let asked = Instant::now();
        execs.start(ExecRequest {
            action: "probe".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        let mut answered = Vec::new();
        while answered.len() < 2 {
            assert!(asked.elapsed() < Duration::from_secs(20), "{answered:?}");
            loop_
                .dispatch(Some(Duration::from_millis(50)), &mut ())
                .unwrap();
            answered.extend(execs.answered(&read));
        }
        let stopped = asked.elapsed();
        for r in &replies {
            assert_eq!(r.peek(), Some(Err("`sh` was stopped".to_string())));
        }
        assert!(
            answered.iter().all(|a| a.exit == 128 + libc::SIGINT),
            "{answered:?}"
        );
        assert_eq!(
            execs.runs(),
            [RunState {
                action: "probe".into(),
                running: false,
                started: None,
                reset: false,
            }]
        );
        drop(waits);
        let mut gates = strand_test_support::gate::Gates::default();
        gates.check(stopped < Duration::from_secs(5), || {
            format!("a stopped call answered after {stopped:?}")
        });
        gates.hold();
    }

    /// (M6, the review of the owner's ruling) An action runs at most
    /// once at a time: calls while it runs queue one re-run, the latest
    /// call's, started when it ends; another action is not held up.
    #[test]
    fn an_action_runs_once_at_a_time_with_one_rerun_queued() {
        let dir = std::env::temp_dir().join(format!("strand-exec-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gate = dir.join("gate");
        let log = dir.join("log");
        // Each run appends its argument once the gate file is there.
        let script = "while [ ! -e \"$0\" ]; do sleep 0.01; done; echo \"$2\" >> \"$1\"";
        let req = |action: &str, arg: &str| ExecRequest {
            action: action.into(),
            argv: [
                "sh",
                "-c",
                script,
                gate.to_str().unwrap(),
                log.to_str().unwrap(),
                arg,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            reply: None,
            stop: false,
        };
        let mut loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        loop_.handle().insert_source(source, |_, _, _| {}).unwrap();
        let mut execs = Execs::new(ping);
        for arg in ["1", "2", "3", "4"] {
            execs.start(req("a", arg));
        }
        execs.start(req("b", "b"));
        assert_eq!(execs.busy(), (2, 1), "a and b run, one re-run of a queued");
        std::fs::write(&gate, "").unwrap();
        let mut ended = Vec::new();
        let start = std::time::Instant::now();
        while ended.len() < 3 {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(20),
                "{ended:?}"
            );
            loop_
                .dispatch(Some(std::time::Duration::from_millis(50)), &mut ())
                .unwrap();
            ended.extend(execs.ended());
        }
        assert_eq!(execs.busy(), (0, 0));
        assert!(ended.iter().all(|e| e.failed.is_none()), "{ended:?}");
        let mut lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            ["1", "4", "b"],
            "the first call, the latest re-run, and b"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (launch-env) A declared action's command (a terminal, often) never
    /// inherits the variables that address this shell: run in a copy of
    /// this test binary started with `STRAND_SOCKET` and
    /// `STRAND_STATE_DIR` set, a command that fails when it sees either
    /// succeeds, and one that needs the rest of the environment too.
    #[test]
    fn a_command_never_inherits_the_variables_that_address_this_shell() {
        const CHILD: &str = "STRAND_TEST_EXEC_ENV_CHILD";
        let sh = |c: &str| vec!["sh".to_string(), "-c".into(), c.into()];
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(
                std::env::var("STRAND_SOCKET").as_deref(),
                Ok("/run/strand-second.sock")
            );
            assert_eq!(
                run_(&sh("test -z \"${STRAND_SOCKET+x}${STRAND_STATE_DIR+x}\"")).0,
                None,
                "inherited"
            );
            assert_eq!(
                run_(&sh("test \"$STRAND_TEST_EXEC_ENV_CHILD\" = 1")).0,
                None
            );
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::exec::tests::a_command_never_inherits_the_variables_that_address_this_shell",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("STRAND_SOCKET", "/run/strand-second.sock")
            .env("STRAND_STATE_DIR", "/tmp/strand-second-state")
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

    /// (M6, the review of the owner's ruling) The command is the leader
    /// of a process group of its own, not strand's.
    #[test]
    fn a_command_runs_in_a_process_group_of_its_own() {
        let dir = std::env::temp_dir().join(format!("strand-exec-pg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("pg");
        // `$$` and its process group, from /proc (fields 1 and 5).
        let script = "set -- $(cut -d' ' -f1,5 /proc/$$/stat) && echo \"$1 $2\" > \"$0\"";
        let argv: Vec<String> = ["sh", "-c", script, out.to_str().unwrap()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(run_(&argv).0, None);
        let text = std::fs::read_to_string(&out).unwrap();
        let ids: Vec<i32> = text
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids[0], ids[1], "it leads its group: {text}");
        // SAFETY: getpgrp has no preconditions.
        let ours = unsafe { libc::getpgrp() };
        assert_ne!(ids[1], ours, "not strand's group");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each argument reaches the program as it is (spaces, quotes and
    /// `$` untouched: no shell stands between); a failing program and one that
    /// cannot start say so.
    #[test]
    fn a_command_runs_with_its_arguments_whole() {
        let dir = std::env::temp_dir().join(format!("strand-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("argv");
        let script = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0\"";
        let argv: Vec<String> = [
            "sh",
            "-c",
            script,
            out.to_str().unwrap(),
            "two words",
            "it's \"quoted\"",
            "$HOME; rm -rf /",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_(&argv).0, None);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "two words\nit's \"quoted\"\n$HOME; rm -rf /\n"
        );
        let (failed, exit, _) = run_(&["sh".into(), "-c".into(), "exit 3".into()]);
        assert_eq!(exit, 3, "the real exit code");
        assert!(
            failed
                .as_deref()
                .is_some_and(|f| f.starts_with("`sh` failed") && f.contains('3')),
            "{failed:?}"
        );
        let missing = run_(&["/nonexistent/strand-test-program".into()]).0;
        assert!(
            missing
                .as_deref()
                .is_some_and(|f| f.starts_with("could not start `/nonexistent/")),
            "{missing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (proc-run) The lines a run's updates leave, applied as
    /// `ActionRuns` applies them.
    #[derive(Default, Debug)]
    struct Shown {
        lines: Vec<RingLine>,
        /// Every open line seen on the way.
        opened: Vec<String>,
        resets: usize,
        /// The block's fields each take set.
        fields: Vec<Vec<Option<Data>>>,
    }

    impl Shown {
        fn apply(&mut self, t: &Taken) {
            if t.fields.iter().any(Option::is_some) {
                self.fields.push(t.fields.clone());
            }
            let Some((front, lines)) = &t.lines else {
                return;
            };
            self.lines.retain(|l| l.n >= *front);
            for l in lines {
                if l.open {
                    self.opened.push(l.text.clone());
                }
                match self.lines.iter_mut().find(|k| k.n == l.n) {
                    Some(k) => *k = l.clone(),
                    None => self.lines.push(l.clone()),
                }
            }
        }

        fn texts(&self) -> Vec<(String, bool, bool)> {
            self.lines
                .iter()
                .map(|l| (l.text.clone(), l.stderr, l.open))
                .collect()
        }
    }

    /// Steps `execs` as the shell does until `n` commands ended, giving
    /// what it printed to `shown`, each start's reset counted.
    fn follow_runs(
        loop_: &mut calloop::EventLoop<'static, ()>,
        execs: &mut Execs,
        shown: &mut Shown,
        n: usize,
    ) -> Vec<Ended> {
        let mut ended = Vec::new();
        let start = Instant::now();
        while ended.len() < n {
            assert!(start.elapsed() < Duration::from_secs(30), "{ended:?}");
            loop_
                .dispatch(Some(Duration::from_millis(10)), &mut ())
                .unwrap();
            for e in execs.ended() {
                shown.apply(&e.taken);
                ended.push(e);
            }
            for r in execs.runs() {
                if r.reset {
                    shown.resets += 1;
                    *shown = Shown {
                        resets: shown.resets,
                        ..Shown::default()
                    };
                }
            }
            for (_, t) in execs.lines() {
                shown.apply(&t);
            }
        }
        ended
    }

    fn job(action: &str, argv: Vec<String>) -> ExecRequest {
        ExecRequest {
            action: action.into(),
            argv,
            reply: None,
            stop: false,
        }
    }

    /// (proc-run) A run's `lines`: stdout and stderr, each line tagged,
    /// as a terminal shows them: a `\r` progress line redrawn in place
    /// (seen open on the way), escapes removed, the open line at the end
    /// ended when the run ends; then its code.
    #[test]
    fn a_run_has_its_lines_both_streams_tagged() {
        let (mut loop_, mut execs) = looped(STOP_GRACE);
        let script = "printf 'one\\n'; sleep 0.05; printf 'oops\\n' >&2; sleep 0.05; \
                      printf 'two\\n'; printf 'pro 10%%\\r'; sleep 0.4; \
                      printf 'pro 50%%\\rpro 100%%\\n'; \
                      printf '\\033[31mred\\033[0m \\033]0;title\\007ok\\n'; printf 'last'";
        execs.start(job("upd", sh(script)));
        let mut shown = Shown::default();
        let ended = follow_runs(&mut loop_, &mut execs, &mut shown, 1);
        assert_eq!((ended[0].failed.as_deref(), ended[0].exit), (None, 0));
        let s = |t: &str, err: bool| (t.to_string(), err, false);
        assert_eq!(
            shown.texts(),
            [
                s("one", false),
                s("oops", true),
                s("two", false),
                s("pro 100%", false),
                s("red ok", false),
                s("last", false),
            ]
        );
        assert!(
            shown.opened.iter().any(|t| t == "pro 10%"),
            "the progress line was seen open: {:?}",
            shown.opened
        );
        assert_eq!(shown.resets, 1);
        let n: Vec<u64> = shown.lines.iter().map(|l| l.n).collect();
        assert_eq!(n, [1, 2, 3, 4, 5, 6], "numbered from 1");
    }

    /// (proc-reason) A failing job's error is its headline and why, from
    /// its stderr after framing (a long one: its head and its tail at
    /// lines; none: the end of its stdout; nothing printed: the headline
    /// alone, as before).
    #[test]
    fn a_run_failing_says_why() {
        let (why, exit, _) = run_(&sh("echo 'resolving dependencies...'; \
             echo 'error: failed to commit transaction (conflicting files)' >&2; \
             printf 'foo: /usr/bin/foo exists in filesystem\\r\\n\\n' >&2; exit 1"));
        assert_eq!(exit, 1);
        assert_eq!(
            why.as_deref(),
            Some(
                "`sh` failed (exit status: 1): error: failed to commit transaction \
                 (conflicting files)\nfoo: /usr/bin/foo exists in filesystem"
            )
        );
        let (why, ..) = run_(&sh("echo 'set -e said this'; exit 2"));
        assert_eq!(
            why.as_deref(),
            Some("`sh` failed (exit status: 2): set -e said this")
        );
        assert_eq!(
            run_(&sh("exit 3")).0.as_deref(),
            Some("`sh` failed (exit status: 3)")
        );
        let (why, ..) = run_(&sh(
            "{ echo 'error: the first'; i=0; while [ $i -lt 3000 ]; do echo \"noise $i\"; \
             i=$((i+1)); done; echo 'the last'; } >&2; exit 1",
        ));
        let why = why.unwrap();
        assert!(why.starts_with("`sh` failed (exit status: 1): error: the first\n"));
        assert!(
            why.contains("\n…\n") && why.ends_with("\nthe last"),
            "{why}"
        );
        assert!(
            why.len() < proc::TAIL_HEAD + proc::TAIL_END + 100,
            "{}",
            why.len()
        );
    }

    /// (proc-ok) A status `ok` lists is no failure (its code kept), one
    /// it does not is; a signal never is; a result's output is read at
    /// a listed status.
    #[test]
    fn an_ok_status_is_not_a_failure() {
        let run_ok = |script: &str, ok: &[i32]| {
            run(&sh(script), &Halt::new(true), STOP_GRACE, &job_cx("b", ok))
        };
        assert_eq!(run_ok("exit 3", &[0, 3]), (None, 3, false));
        assert_eq!(run_ok("exit 0", &[0, 3]), (None, 0, false));
        assert!(run_ok("exit 2", &[0, 3]).0.is_some());
        let (why, exit, _) = run_ok("kill -TERM $$", &[0, 143]);
        assert_eq!(exit, 143);
        assert!(
            why.is_some_and(|w| w.contains("signal")),
            "a signal is never ok"
        );
        let got = capture(
            &sh("echo none; exit 2"),
            &[0, 2],
            &Halt::new(false),
            POLL_TIMEOUT,
            STOP_GRACE,
        );
        assert_eq!(
            got,
            Captured {
                out: Ok("none".into()),
                exit: 2
            }
        );
        let got = capture(
            &sh("exit 2"),
            &[0],
            &Halt::new(false),
            POLL_TIMEOUT,
            STOP_GRACE,
        );
        assert_eq!(got.out, Err("`sh` failed (exit status: 2)".into()));
    }

    /// (proc-reason) A call with a result failing: its `.error` says why,
    /// from its stderr, else the end of its stdout; its output is framed
    /// as a terminal shows it when it succeeds.
    #[test]
    fn a_result_call_failing_says_why() {
        let call = |script: &str| {
            capture(
                &sh(script),
                &[0],
                &Halt::new(false),
                POLL_TIMEOUT,
                STOP_GRACE,
            )
        };
        assert_eq!(
            call("echo partial; echo 'qalc: no network' >&2; exit 4").out,
            Err("`sh` failed (exit status: 4): qalc: no network".into())
        );
        assert_eq!(
            call("echo 'nothing to say'; exit 4").out,
            Err("`sh` failed (exit status: 4): nothing to say".into())
        );
        assert_eq!(
            call("printf '\\033[1m42\\033[0m\\r43\\n'").out,
            Ok("43".into())
        );
    }

    /// (proc-run) A run's `lines`, `code` and `error` reset when one
    /// starts, a queued re-run included, never when one ends.
    #[test]
    fn lines_reset_when_a_run_starts() {
        let dir = scratch("reset");
        let gate = dir.join("gate");
        let (mut loop_, mut execs) = looped(STOP_GRACE);
        let first = format!(
            "echo first; while [ ! -e '{}' ]; do sleep 0.01; done; exit 1",
            gate.display()
        );
        execs.start(job("u", sh(&first)));
        execs.start(job("u", sh("echo second")));
        let mut shown = Shown::default();
        // The first's line, before it ends.
        let start = Instant::now();
        while shown.lines.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(20));
            loop_
                .dispatch(Some(Duration::from_millis(10)), &mut ())
                .unwrap();
            for r in execs.runs() {
                shown.resets += usize::from(r.reset);
            }
            for (_, t) in execs.lines() {
                shown.apply(&t);
            }
        }
        assert_eq!(shown.texts(), [("first".into(), false, false)]);
        std::fs::write(&gate, "").unwrap();
        let ended = follow_runs(&mut loop_, &mut execs, &mut shown, 2);
        assert!(ended[0].failed.is_some() && ended[1].failed.is_none());
        assert_eq!(shown.resets, 2, "the re-run reset its run");
        assert_eq!(shown.texts(), [("second".into(), false, false)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (proc-run) A stopped run keeps what it printed; its code is how it
    /// ended (130 for SIGINT), and it is no failure.
    #[test]
    fn a_stopped_run_keeps_its_lines_and_code_130() {
        let (mut loop_, mut execs) = looped(Duration::from_millis(400));
        execs.start(job("rec", sh("echo working; exec sleep 30")));
        let mut shown = Shown::default();
        let start = Instant::now();
        while shown.lines.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(20));
            loop_
                .dispatch(Some(Duration::from_millis(10)), &mut ())
                .unwrap();
            let _ = execs.runs();
            for (_, t) in execs.lines() {
                shown.apply(&t);
            }
        }
        execs.start(ExecRequest {
            action: "rec".into(),
            argv: Vec::new(),
            reply: None,
            stop: true,
        });
        let ended = follow_runs(&mut loop_, &mut execs, &mut shown, 1);
        assert_eq!(ended[0].exit, 130);
        assert!(ended[0].stopped && ended[0].failed.is_none());
        assert_eq!(shown.texts(), [("working".into(), false, false)]);
    }

    /// (proc-run) An action's block reads its stdout lines into its
    /// fields, the latest of each; stderr never reaches them; a block
    /// changed while the run reads (a reload) is read from then on.
    #[test]
    fn a_block_reads_stdout_lines_into_fields() {
        use strand_services::custom::{FieldSpec, TextField};
        let field = |name: &str, text| FieldSpec {
            name: name.into(),
            key: vec![name.into()],
            rw: false,
            text,
        };
        let (mut loop_, mut execs) = looped(STOP_GRACE);
        let spec = |fields| BlockSpec {
            spec: -77,
            fields: Arc::new(fields),
        };
        lock(&execs.blocks).insert(
            "backup".into(),
            spec(vec![
                field("message_type", TextField::Required),
                field("percent_done", TextField::No),
            ]),
        );
        let script = "echo '{\"message_type\":\"status\",\"percent_done\":0.25}'; \
                      echo '{\"percent_done\":0.5,\"message_type\":\"bogus\"}' >&2; \
                      echo 'scanning...'; \
                      echo '{\"message_type\":\"status\",\"percent_done\":0.75}'";
        execs.start(job("backup", sh(script)));
        let mut fields: Vec<Option<Data>> = Vec::new();
        let mut take = |t: &Taken| {
            for (i, v) in t.fields.iter().enumerate() {
                if fields.len() <= i {
                    fields.resize(i + 1, None);
                }
                if v.is_some() {
                    fields[i].clone_from(v);
                }
            }
        };
        let start = Instant::now();
        let mut ended = Vec::new();
        while ended.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(20));
            loop_
                .dispatch(Some(Duration::from_millis(10)), &mut ())
                .unwrap();
            let _ = execs.runs();
            for (_, t) in execs.lines() {
                take(&t);
            }
            for e in execs.ended() {
                take(&e.taken);
                ended.push(e);
            }
        }
        assert_eq!(
            fields,
            [
                // `scanning...` names no field: the required text field
                // reads it as written, then the last JSON line's value.
                Some(Data::text("status")),
                Some(Data::Float(0.75)),
            ]
        );
    }

    /// (proc-run) The end order: everything a run printed comes with or
    /// before its end (one report, taken after its last line), none after.
    #[test]
    fn the_end_order_is_lines_then_error_then_running() {
        let (mut loop_, mut execs) = looped(STOP_GRACE);
        for i in 0..5 {
            // (Order across the two pipes is the order they were read:
            // the pause makes it this one.)
            let script = format!("seq 1 2000; sleep 0.2; echo 'boom {i}' >&2; exit 1");
            execs.start(job(&format!("a{i}"), sh(&script)));
        }
        let mut shown: HashMap<String, Shown> = HashMap::new();
        let start = Instant::now();
        let mut ended = 0;
        while ended < 5 {
            assert!(start.elapsed() < Duration::from_secs(30));
            loop_
                .dispatch(Some(Duration::from_millis(5)), &mut ())
                .unwrap();
            let _ = execs.runs();
            for (a, t) in execs.lines() {
                shown.entry(a).or_default().apply(&t);
            }
            for e in execs.ended() {
                let s = shown.entry(e.action.clone()).or_default();
                s.apply(&e.taken);
                let last = s.lines.last().map(|l| l.text.clone());
                assert_eq!(last, Some(e.action.replace('a', "boom ")), "{}", e.action);
                assert!(
                    e.failed
                        .as_deref()
                        .is_some_and(|f| f.ends_with(&e.action.replace('a', "boom "))),
                    "{:?}",
                    e.failed
                );
                ended += 1;
            }
        }
        // Nothing after an end.
        loop_
            .dispatch(Some(Duration::from_millis(50)), &mut ())
            .unwrap();
        assert!(execs.lines().is_empty());
        for s in shown.values() {
            assert_eq!(s.lines.len(), 1000, "the ring keeps the last 1000");
            assert!(s.lines.iter().all(|l| !l.open));
        }
    }

    /// (proc-run) A flood of output (200 000 lines) costs its ring, not
    /// its size: the ring stays at 1000 lines, and logic is woken at
    /// most once a frame while it reads.
    #[test]
    fn a_chatty_action_costs_its_ring() {
        let mut loop_ = calloop::EventLoop::<()>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        let wakes = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let w = wakes.clone();
        loop_
            .handle()
            .insert_source(source, move |_, _, _| w.set(w.get() + 1))
            .unwrap();
        let mut execs = Execs::new(ping);
        let start = Instant::now();
        execs.start(job("chatty", sh("seq 1 200000")));
        let mut shown = Shown::default();
        let mut most = 0;
        let ended = loop {
            loop_
                .dispatch(Some(Duration::from_millis(50)), &mut ())
                .unwrap();
            let _ = execs.runs();
            for (_, t) in execs.lines() {
                shown.apply(&t);
            }
            for d in execs.running.values() {
                most = most.max(lock(&d.out).ring.lines().count());
            }
            let e = execs.ended();
            if !e.is_empty() {
                break e;
            }
            assert!(start.elapsed() < Duration::from_secs(60));
        };
        let took = start.elapsed();
        shown.apply(&ended[0].taken);
        assert!(most <= proc::RING_LINES, "{most}");
        assert_eq!(shown.lines.len(), proc::RING_LINES);
        assert_eq!(shown.lines.last().map(|l| l.text.as_str()), Some("200000"));
        // At most one wake a frame, and one at its end.
        let frames = (took.as_secs_f64() / LISTEN_FLUSH.as_secs_f64()).ceil() as u32;
        assert!(
            wakes.get() <= frames + 2,
            "{} wakes in {took:?}",
            wakes.get()
        );
    }

    /// (proc-keeper) A job outlives strand without SIGPIPE: a copy of
    /// this test binary runs two jobs writing for a second (on stdout,
    /// on stderr), then is killed; both jobs end with success (their
    /// writes never failed), and the keeper exits once their pipes end.
    #[test]
    fn a_run_outlives_strand_without_sigpipe() {
        const CHILD: &str = "STRAND_TEST_KEEPER_RUN";
        let writer = |to: &str| {
            format!(
                "i=0; while [ $i -lt 50 ]; do seq 1 3000 {to} || {{ echo \"seq $?\" > \"$0\"; \
                 exit 1; }}; sleep 0.02; i=$((i+1)); done; echo ok > \"$0\""
            )
        };
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = std::path::PathBuf::from(dir);
            let (mut loop_, mut execs) = looped(STOP_GRACE);
            for (name, to) in [("out", ""), ("err", ">&2")] {
                let mut argv = sh(&writer(to));
                argv.push(dir.join(name).to_str().unwrap().into());
                execs.start(job(name, argv));
            }
            let mut seen = 0;
            let start = Instant::now();
            while seen < 100 || start.elapsed() < Duration::from_millis(200) {
                assert!(start.elapsed() < Duration::from_secs(20));
                loop_
                    .dispatch(Some(Duration::from_millis(10)), &mut ())
                    .unwrap();
                let _ = execs.runs();
                for (_, t) in execs.lines() {
                    seen += t.lines.map_or(0, |l| l.1.len());
                }
            }
            let pid = execs.keeper.pid().expect("the keeper runs");
            std::fs::write(dir.join("keeper.part"), pid.to_string()).unwrap();
            std::fs::rename(dir.join("keeper.part"), dir.join("keeper")).unwrap();
            // Killed here, mid-run.
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        let dir = scratch("keeper");
        let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::exec::tests::a_run_outlives_strand_without_sigpipe",
                "--test-threads=1",
            ])
            .env(CHILD, &dir)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        wait_for("the helper never started its jobs", 30, || {
            dir.join("keeper").exists()
        });
        let keeper: i32 = std::fs::read_to_string(dir.join("keeper"))
            .unwrap()
            .parse()
            .unwrap();
        helper.kill().unwrap();
        let _ = helper.wait();
        for name in ["out", "err"] {
            wait_for(&format!("{name} never ended"), 30, || {
                dir.join(name).exists()
            });
            wait_for(&format!("{name} said nothing"), 5, || {
                std::fs::read_to_string(dir.join(name)).is_ok_and(|s| s.ends_with('\n'))
            });
            assert_eq!(
                std::fs::read_to_string(dir.join(name)).unwrap(),
                "ok\n",
                "{name}'s writes failed after strand went"
            );
        }
        // SAFETY: kill(2) with signal 0 only asks whether it is there.
        wait_for("the keeper runs on", 20, || {
            (unsafe { libc::kill(keeper, 0) }) != 0 || zombie(keeper)
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (proc-run) A reload never stops a run or resets what it printed:
    /// its lines go on numbering, and a block the reload declared afresh
    /// (a new spec, other fields) is read from then on.
    #[test]
    fn a_reload_keeps_reading() {
        use strand_services::custom::{FieldSpec, TextField};
        let dir = scratch("reload");
        let gate = dir.join("gate");
        let field = |name: &str| FieldSpec {
            name: name.into(),
            key: vec![name.into()],
            rw: false,
            text: TextField::No,
        };
        let (mut loop_, mut execs) = looped(STOP_GRACE);
        let block = |spec, f: &str| BlockSpec {
            spec,
            fields: Arc::new(vec![field(f)]),
        };
        lock(&execs.blocks).insert("u".into(), block(-80, "a"));
        let script = format!(
            "echo a=1; while [ ! -e '{}' ]; do sleep 0.01; done; echo b=2",
            gate.display()
        );
        execs.start(job("u", sh(&script)));
        let mut shown = Shown::default();
        let mut fields = Vec::new();
        let start = Instant::now();
        while shown.lines.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(20));
            loop_
                .dispatch(Some(Duration::from_millis(10)), &mut ())
                .unwrap();
            for r in execs.runs() {
                shown.resets += usize::from(r.reset);
            }
            for (_, t) in execs.lines() {
                shown.apply(&t);
                fields.push(t.fields.clone());
            }
        }
        // (Pairs hold text: the host converts it to the field's type.)
        assert_eq!(fields.concat(), [Some(Data::text("1"))]);
        // The reload: the block is now `{ b: int }`, another spec.
        lock(&execs.blocks).insert("u".into(), block(-81, "b"));
        std::fs::write(&gate, "").unwrap();
        let ended = follow_runs(&mut loop_, &mut execs, &mut shown, 1);
        assert_eq!(shown.resets, 1, "never reset");
        assert_eq!(
            shown.texts(),
            [("a=1".into(), false, false), ("b=2".into(), false, false)]
        );
        assert!(ended[0].failed.is_none());
        assert_eq!(
            shown.fields.concat(),
            [Some(Data::text("1")), Some(Data::text("2"))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

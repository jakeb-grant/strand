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
//! group (a signal to strand's group does not reach it), stdin and stdout
//! on `/dev/null`, stderr strand's own (its log). Like every program
//! strand starts it does not inherit strand's xdg-activation token or the
//! variables that address this shell (`child::child_env`), and it gets
//! back the transparent huge page setting strand inherited. Its
//! thread waits for it and nothing else: it is never killed, since an
//! app it opens may run for hours. A failure to start or a non-zero exit
//! comes back (the loop is pinged awake) as a log line and a `strand
//! watch` notice naming the action, as a `file`'s `run:` failing does.
//! The shell itself is fine, so it is not an overlay notice.
//!
//! (flex) An action with a result (`action calc(q: text) -> text
//! ["qalc", "-t", q]`) runs each call at once on a thread of its own (a
//! binding's newer call supersedes the older rather than queueing behind
//! it), its stdout piped and read up to `strand_services::custom::
//! MAX_DOCUMENT` bytes. When it ends, its output is read as the declared
//! type on the logic thread (`services::custom::read_output`) and
//! answers the call's `Async`; a failure to start, a non-zero exit, too
//! much output or output that does not convert is the `Async`'s error,
//! logged but no notice (the config reads `.error`). A call whose value
//! was dropped first (superseded, or a handler let it go) has its
//! process group ended: SIGTERM, then SIGKILL after
//! `strand_services::custom::KILL_GRACE`.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;
use strand_compiler::vm::{ExecReply, ExecRequest};
use strand_services::custom::{KILL_GRACE, MAX_DOCUMENT, POLL_TIMEOUT};

use super::shell::Shell;

/// A command's thread only starts it and waits.
const STACK: usize = 64 * 1024;

/// A command that ended, as its thread reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ended {
    /// The action's name.
    pub action: String,
    /// Why it failed; `None`: it exited with success.
    pub failed: Option<String>,
    /// Its exit code ([`Captured::exit`]).
    pub exit: i32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// (flex) Commands' outputs as their threads report them: the call's
/// id, and what it printed or why it failed.
type Outputs = Arc<Mutex<Vec<(u64, Captured)>>>;

/// The commands started and the ones that ended since logic last looked.
pub(super) struct Execs {
    ping: calloop::ping::Ping,
    ended: Arc<Mutex<Vec<Ended>>>,
    /// The actions whose command is running.
    running: HashSet<String>,
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
}

/// (flex) A running command whose output answers its call.
struct Fetching {
    id: u64,
    action: String,
    /// Its arguments (the recorder holds texts that are not the
    /// program's own).
    argv: Vec<String>,
    reply: ExecReply,
    /// Set when its call was dropped: its thread ends it.
    cancel: Arc<AtomicBool>,
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

impl Execs {
    /// `ping` wakes the loop when a command ends.
    pub(super) fn new(ping: calloop::ping::Ping) -> Execs {
        Execs {
            ping,
            ended: Arc::default(),
            running: HashSet::new(),
            queued: HashMap::new(),
            fetching: Vec::new(),
            outputs: Arc::default(),
            next_id: 0,
            timeout: POLL_TIMEOUT,
        }
    }

    /// Starts `req`'s command on a thread of its own, or (its action's
    /// command still running) queues it to run once that ends, in place
    /// of any re-run queued before. (flex) A command with a reply always
    /// starts at once.
    pub(super) fn start(&mut self, req: ExecRequest) {
        if let Some(reply) = req.reply.clone() {
            self.fetch(req, reply);
            return;
        }
        if self.running.contains(&req.action) {
            self.queued.insert(req.action.clone(), req);
            return;
        }
        self.spawn(req);
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
        let spawned = std::thread::Builder::new()
            .name("strand-exec".into())
            .stack_size(STACK)
            .spawn(move || {
                let (failed, exit) = run(&req.argv);
                lock(&ended).push(Ended {
                    action: req.action,
                    failed,
                    exit,
                });
                ping.ping();
            });
        match spawned {
            Ok(_) => {
                self.running.insert(action);
            }
            Err(e) => lock(&self.ended).push(Ended {
                action,
                failed: Some(format!("no thread to start it on: {e}")),
                exit: -1,
            }),
        }
    }

    /// (flex) Runs `req`'s command, its output kept for `reply`.
    fn fetch(&mut self, req: ExecRequest, reply: ExecReply) {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let (outputs, ping) = (self.outputs.clone(), self.ping.clone());
        let (c, timeout) = (cancel.clone(), self.timeout);
        let argv = req.argv.clone();
        let spawned = std::thread::Builder::new()
            .name("strand-exec".into())
            .stack_size(STACK)
            .spawn(move || {
                let out = capture(&argv, &c, timeout);
                lock(&outputs).push((id, out));
                ping.ping();
            });
        if let Err(e) = spawned {
            reply.answer(Err(format!("no thread to start it on: {e}")));
            return;
        }
        self.fetching.push(Fetching {
            id,
            action: req.action,
            argv: req.argv,
            reply,
            cancel,
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
        self.fetching.retain(|f| {
            if !f.reply.abandoned() {
                return true;
            }
            f.cancel.store(true, Ordering::SeqCst);
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
            if self.running.remove(&e.action)
                && let Some(next) = self.queued.remove(&e.action)
            {
                self.spawn(next);
            }
        }
        ended
    }
}

/// Runs `argv` (the program, then each argument as it is) to its end:
/// `None` when it exited with success, else why not; and its exit code
/// ([`Captured::exit`]).
pub(super) fn run(argv: &[String]) -> (Option<String>, i32) {
    let Some((prog, args)) = argv.split_first() else {
        return (Some("it has no command".into()), -1);
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    strand_services::child::child_env(&mut cmd, None);
    // SAFETY: `restore_in_child` makes only async-signal-safe calls (an
    // atomic load, `prctl`), as `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            strand_services::child::restore_in_child();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Some(format!("could not start `{prog}`: {e}")), -1),
    };
    match child.wait() {
        Ok(s) if s.success() => (None, 0),
        Ok(s) => (Some(format!("`{prog}` failed ({s})")), exit_code(s)),
        Err(e) => (Some(format!("`{prog}` could not be waited for: {e}")), -1),
    }
}

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
/// [`MAX_DOCUMENT`] bytes), or why not. It is given `timeout` to print
/// its output and end, as a `from poll` command is (a command that
/// hangs, or leaves a child holding its stdout, would otherwise hold its
/// call pending for ever); `cancel` set (its call was dropped) ends it
/// at once. This thread alone ends its process group, before it reaps
/// the command, so the group's id cannot have been reused by then.
pub(super) fn capture(argv: &[String], cancel: &AtomicBool, timeout: Duration) -> Captured {
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
        .process_group(0);
    strand_services::child::child_env(&mut cmd, None);
    // SAFETY: only async-signal-safe calls (an atomic load, `prctl`), as
    // `pre_exec` requires. The death signal ends it with this thread,
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
    let mut out = Vec::new();
    let read = match child.stdout.take() {
        Some(stdout) => read_until(stdout, &mut out, deadline, cancel),
        None => Ok(Reading::Done),
    };
    let ended = |child: &mut std::process::Child, why: String| {
        end_group(child, group);
        Captured {
            out: Err(why),
            exit: -1,
        }
    };
    match read {
        Ok(Reading::Done) => {}
        Ok(Reading::TooMuch) => {
            return ended(
                &mut child,
                format!("`{prog}` printed more than {MAX_DOCUMENT} bytes; ended"),
            );
        }
        Ok(Reading::Cancelled) => {
            return ended(&mut child, format!("`{prog}` was no longer needed"));
        }
        Ok(Reading::TimedOut) => {
            return ended(
                &mut child,
                format!("`{prog}` timed out after {}", span(timeout)),
            );
        }
        Err(e) => {
            return ended(
                &mut child,
                format!("`{prog}`'s output could not be read: {e}"),
            );
        }
    }
    // Its stdout closed: it ends now, or within what is left.
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if cancel.load(Ordering::SeqCst) => {
                return ended(&mut child, format!("`{prog}` was no longer needed"));
            }
            Ok(None) if Instant::now() >= deadline => {
                return ended(
                    &mut child,
                    format!("`{prog}` timed out after {}", span(timeout)),
                );
            }
            Ok(None) => std::thread::sleep(TICK),
            Err(e) => return failed(format!("`{prog}` could not be waited for: {e}")),
        }
    };
    let exit = exit_code(status);
    if !status.success() {
        return Captured {
            out: Err(format!("`{prog}` failed ({status})")),
            exit,
        };
    }
    Captured {
        out: Ok(String::from_utf8_lossy(&out).into_owned()),
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

/// How reading a command's stdout ended.
enum Reading {
    /// At its end.
    Done,
    /// Past [`MAX_DOCUMENT`] bytes.
    TooMuch,
    /// Its call was dropped.
    Cancelled,
    /// Its deadline passed.
    TimedOut,
}

/// Reads `stdout` into `out` until its end, [`MAX_DOCUMENT`] bytes,
/// `deadline` or `cancel`, waiting at most [`TICK`] at a time.
fn read_until(
    mut stdout: std::process::ChildStdout,
    out: &mut Vec<u8>,
    deadline: Instant,
    cancel: &AtomicBool,
) -> std::io::Result<Reading> {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut buf = [0u8; 8192];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(Reading::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Reading::TimedOut);
        }
        let wait = (deadline - now).min(TICK);
        let ready = {
            let mut fds = [PollFd::new(&stdout, PollFlags::IN)];
            let ts = rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: i64::from(wait.subsec_nanos()),
            };
            match poll(&mut fds, Some(&ts)) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(e.into()),
            }
        };
        if !ready {
            continue;
        }
        match stdout.read(&mut buf) {
            Ok(0) => return Ok(Reading::Done),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_DOCUMENT {
                    return Ok(Reading::TooMuch);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
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
            // (report) Its start, never its arguments (their count).
            let t = strand_core::flight::now_ns();
            let argc = req.argv.len().saturating_sub(1);
            self.recorder
                .borrow_mut()
                .action(t, &req.action, argc, true, 0);
            self.execs.start(req);
        }
        for e in self.execs.ended() {
            let t = strand_core::flight::now_ns();
            self.recorder
                .borrow_mut()
                .action(t, &e.action, 0, false, e.exit);
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
        let mut map = strand_compiler::SourceMap::new();
        let src = format!(
            "type Probe {{ load: float; name: text }}\nenum Level {{ low, high }}\n\
             permit exec \"sh\"\naction probe() -> {ty} [\"sh\"]\n"
        );
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
        assert_eq!(
            bad.peek(),
            Some(Err("`sh` failed (exit status: 3)".to_string()))
        );
        assert!(
            answered.iter().any(|a| a.action == "probe"
                && a.failed.as_deref() == Some("`sh` failed (exit status: 3)")
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
            });
        }
        let start = Instant::now();
        let mut answered = Vec::new();
        while answered.len() < 2 {
            assert!(start.elapsed() < Duration::from_secs(20), "{answered:?}");
            answered.extend(execs.answered(&read));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
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
                run(&sh("test -z \"${STRAND_SOCKET+x}${STRAND_STATE_DIR+x}\"")).0,
                None,
                "inherited"
            );
            assert_eq!(run(&sh("test \"$STRAND_TEST_EXEC_ENV_CHILD\" = 1")).0, None);
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
        assert_eq!(run(&argv).0, None);
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
        assert_eq!(run(&argv).0, None);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "two words\nit's \"quoted\"\n$HOME; rm -rf /\n"
        );
        let (failed, exit) = run(&["sh".into(), "-c".into(), "exit 3".into()]);
        assert_eq!(exit, 3, "the real exit code");
        assert!(
            failed
                .as_deref()
                .is_some_and(|f| f.starts_with("`sh` failed") && f.contains('3')),
            "{failed:?}"
        );
        let missing = run(&["/nonexistent/strand-test-program".into()]).0;
        assert!(
            missing
                .as_deref()
                .is_some_and(|f| f.starts_with("could not start `/nonexistent/")),
            "{missing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

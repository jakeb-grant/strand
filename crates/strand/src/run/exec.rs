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
//! strand starts it does not inherit strand's xdg-activation token, and it
//! gets back the transparent huge page setting strand inherited. Its
//! thread waits for it and nothing else: it is never killed, since an
//! app it opens may run for hours. A failure to start or a non-zero exit
//! comes back (the loop is pinged awake) as a log line and a `strand
//! watch` notice naming the action, as a `file`'s `run:` failing does.
//! The shell itself is fine, so it is not an overlay notice.

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::json;
use strand_compiler::vm::ExecRequest;

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
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The commands started and the ones that ended since logic last looked.
pub(super) struct Execs {
    ping: calloop::ping::Ping,
    ended: Arc<Mutex<Vec<Ended>>>,
    /// The actions whose command is running.
    running: HashSet<String>,
    /// The re-run queued behind each running one: the latest call.
    queued: HashMap<String, ExecRequest>,
}

impl Execs {
    /// `ping` wakes the loop when a command ends.
    pub(super) fn new(ping: calloop::ping::Ping) -> Execs {
        Execs {
            ping,
            ended: Arc::default(),
            running: HashSet::new(),
            queued: HashMap::new(),
        }
    }

    /// Starts `req`'s command on a thread of its own, or (its action's
    /// command still running) queues it to run once that ends, in place
    /// of any re-run queued before.
    pub(super) fn start(&mut self, req: ExecRequest) {
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
                let failed = run(&req.argv);
                lock(&ended).push(Ended {
                    action: req.action,
                    failed,
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
            }),
        }
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
/// `None` when it exited with success, else why not.
pub(super) fn run(argv: &[String]) -> Option<String> {
    let Some((prog, args)) = argv.split_first() else {
        return Some("it has no command".into());
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    strand_services::child::activation_env(&mut cmd, None);
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
        Err(e) => return Some(format!("could not start `{prog}`: {e}")),
    };
    match child.wait() {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!("`{prog}` failed ({s})")),
        Err(e) => Some(format!("`{prog}` could not be waited for: {e}")),
    }
}

impl Shell {
    /// After the tick's diff: starts the commands handlers asked for,
    /// and reports the ones that failed.
    pub(super) fn run_execs(&mut self) {
        for req in self.inst.take_execs() {
            self.execs.start(req);
        }
        for e in self.execs.ended() {
            if let Some(why) = e.failed {
                self.exec_notice(&e.action, &why);
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
        assert_eq!(run(&argv), None);
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
        assert_eq!(run(&argv), None);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "two words\nit's \"quoted\"\n$HOME; rm -rf /\n"
        );
        let failed = run(&["sh".into(), "-c".into(), "exit 3".into()]);
        assert!(
            failed
                .as_deref()
                .is_some_and(|f| f.starts_with("`sh` failed") && f.contains('3')),
            "{failed:?}"
        );
        let missing = run(&["/nonexistent/strand-test-program".into()]);
        assert!(
            missing
                .as_deref()
                .is_some_and(|f| f.starts_with("could not start `/nonexistent/")),
            "{missing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

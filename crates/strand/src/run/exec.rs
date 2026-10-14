//! (M6) The commands of declared actions (`action dispatch(ws: int)
//! ["hyprctl", "dispatch", "workspace", ws]`; decisions.md
//! m6-w2-language, the owner's ruling on owner-testing Gaps 1).
//!
//! A handler's call asks for its command (`Instance::take_execs`, taken
//! after each tick's diff). Each starts on a thread of its own, so logic
//! never waits on a fork or on the program: detached in its own process
//! group (a signal to strand's group does not reach it), stdin and stdout
//! on `/dev/null`, stderr strand's own (its log). Like every program
//! strand starts it does not inherit strand's xdg-activation token, and it
//! gets back the transparent huge page setting strand inherited. Its
//! thread waits for it and nothing else: it is never killed, since an
//! app it opens may run for hours. A failure to start or a non-zero exit
//! comes back (the loop is pinged awake) as a log line and a `strand
//! watch` notice naming the action, as a `file`'s `run:` failing does.
//! The shell itself is fine, so it is not an overlay notice.

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
}

impl Execs {
    /// `ping` wakes the loop when a command ends.
    pub(super) fn new(ping: calloop::ping::Ping) -> Execs {
        Execs {
            ping,
            ended: Arc::default(),
        }
    }

    /// Starts `req`'s command on a thread of its own.
    pub(super) fn start(&self, req: ExecRequest) {
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
        if let Err(e) = spawned {
            lock(&self.ended).push(Ended {
                action,
                failed: Some(format!("no thread to start it on: {e}")),
            });
        }
    }

    /// The commands that ended since the last call.
    pub(super) fn ended(&self) -> Vec<Ended> {
        std::mem::take(&mut *lock(&self.ended))
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

    /// Each argument reaches the program as it is (spaces, quotes and
    /// `$` untouched: no shell reads it); a failing program and one that
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

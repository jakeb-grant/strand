//! The client against fake helpers (shell scripts speaking, or failing
//! to speak, the protocol): garbage, early exits and silence all fail
//! closed; only a success verdict mints a token. A conversation's texts
//! and prompts arrive as the helper sends them, and its deadlines count
//! only while a password waits for PAM or is with it (m6-lock).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use strand_auth::{AuthError, Client, Conversed, Password, Said, Verdict, answers};

fn no_hook() {}

/// A fake helper: `body` after `#!/bin/sh`. Written by a `sh` of its own,
/// never through a descriptor of this process: a test on another thread
/// forking while this process held the file open for writing would carry
/// that descriptor until its `exec`, and executing the file meanwhile
/// fails with `ETXTBSY` ("Text file busy", seen once in GitHub run
/// 38217404775).
fn fake(dir: &Path, body: &str) -> PathBuf {
    use std::io::Write as _;
    let path = dir.join("fake-helper");
    let mut sh = std::process::Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = sh.stdin.take().unwrap();
    stdin
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    drop(stdin);
    assert!(sh.wait().unwrap().success(), "writing {path:?}");
    path
}

/// The bytes of a `HELLO` (version 2, `strand`).
const HELLO: &str = r"\003\000\000\000\001\002\000";

/// Shell functions for a fake helper that speaks conversations: `hello`;
/// `frame [FILE]` reads one frame and writes its payload to FILE (or
/// drops it); `info TEXT` and `prompt TEXT` send those frames;
/// `verdict CODE` sends a verdict with no message.
const SPEAK: &str = r#"
hello() { printf '\003\000\000\000\001\002\000'; }
frame() {
  len=$(head -c 4 | od -An -tu4 | tr -d ' ')
  [ -n "$len" ] || exit 0
  head -c 1 >/dev/null
  if [ "$len" -gt 1 ]; then head -c $((len - 1)) > "${1:-/dev/null}"; else : > "${1:-/dev/null}"; fi
}
send() { printf "$(printf '\\%03o\\000\\000\\000\\%03o' $((${#2} + 1)) "$1")"; printf '%s' "$2"; }
info() { send 5 "$1"; }
prompt() { send 6 "$1"; }
verdict() { printf "$(printf '\\002\\000\\000\\000\\003\\%03o' "$1")"; }
"#;

fn run(body: &str) -> Verdict {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::new(fake(dir.path(), body), no_hook).with_timeout(Duration::from_secs(2));
    c.submit(Password::from("pw".to_string()))
}

#[test]
fn a_success_verdict_mints_a_token() {
    let v = run(&format!(
        r"printf '{HELLO}'; sleep 0.2; printf '\002\000\000\000\003\000'; sleep 5"
    ));
    assert!(v.is_unlocked(), "{v:?}");
}

#[test]
fn a_denied_verdict_carries_pams_message() {
    let v = run(&format!(
        r"printf '{HELLO}'; sleep 0.2; printf '\005\000\000\000\003\001no!'; sleep 5"
    ));
    let Verdict::Denied { message } = v else {
        panic!("{v:?}");
    };
    assert_eq!(message.as_deref(), Some("no!"));
}

#[test]
fn an_error_verdict_is_a_failure() {
    let v = run(&format!(
        r"printf '{HELLO}'; sleep 0.2; printf '\005\000\000\000\003\002bad'; sleep 5"
    ));
    assert!(
        matches!(&v, Verdict::Failed(AuthError::Pam(m)) if m == "bad"),
        "{v:?}"
    );
}

#[test]
fn garbage_and_silence_fail_closed() {
    type Expect = fn(&Verdict) -> bool;
    let cases: [(&str, Expect); 7] = [
        // Text instead of a hello.
        ("printf 'hello, world'; sleep 5", |v| {
            matches!(v, Verdict::Failed(AuthError::Protocol(_)))
        }),
        // Another protocol version (1, before conversations).
        (r"printf '\003\000\000\000\001\001\000'; sleep 5", |v| {
            matches!(v, Verdict::Failed(AuthError::Protocol(_)))
        }),
        // A verdict before the hello.
        (r"printf '\001\000\000\000\003\000'; sleep 5", |v| {
            matches!(v, Verdict::Failed(AuthError::Protocol(_)))
        }),
        // An unknown verdict code.
        (
            r"printf '\003\000\000\000\001\002\000'; sleep 0.2; printf '\002\000\000\000\003\011'; sleep 5",
            |v| matches!(v, Verdict::Failed(AuthError::Protocol(_))),
        ),
        // A frame cut short by an exit.
        (
            r"printf '\003\000\000\000\001\002\000'; sleep 0.2; printf '\011\000\000\000\003'",
            |v| {
                matches!(
                    v,
                    Verdict::Failed(AuthError::Protocol(_) | AuthError::HelperDied)
                )
            },
        ),
        // Gone at once.
        ("exit 0", |v| {
            matches!(v, Verdict::Failed(AuthError::HelperDied))
        }),
        // Says hello, then nothing.
        (r"printf '\003\000\000\000\001\002\000'; sleep 30", |v| {
            matches!(v, Verdict::Failed(AuthError::Timeout))
        }),
    ];
    for (body, ok) in cases {
        let v = run(body);
        assert!(!v.is_unlocked(), "{body}: {v:?}");
        assert!(ok(&v), "{body}: {v:?}");
    }
}

/// The helper's environment is scrubbed: only PATH and the locale.
#[test]
fn the_helper_starts_with_a_scrubbed_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env");
    let body = format!(
        r"env > {}; printf '{HELLO}'; sleep 0.2; printf '\002\000\000\000\003\001'; sleep 5",
        out.display()
    );
    let mut c = Client::new(fake(dir.path(), &body), no_hook).with_timeout(Duration::from_secs(2));
    let _ = c.submit(Password::from("pw".to_string()));
    let env = std::fs::read_to_string(&out).unwrap();
    for line in env.lines() {
        let key = line.split('=').next().unwrap_or("");
        assert!(
            [
                "PATH",
                "PWD",
                "SHLVL",
                "_",
                "LANG",
                "LANGUAGE",
                "LC_ALL",
                "LC_MESSAGES",
                "LC_CTYPE"
            ]
            .contains(&key),
            "leaked into the helper: {line}"
        );
    }
}

/// The owner's `pre_exec` runs in the child, before exec, on every spawn.
#[test]
fn the_owners_pre_exec_runs_in_the_child() {
    fn hook() {
        // Async-signal-safe, and visible in the child: its umask.
        // SAFETY: umask takes no pointers.
        unsafe {
            libc::umask(0o077);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("umask");
    let body = format!(
        r"umask > {}; printf '{HELLO}'; sleep 0.2; printf '\002\000\000\000\003\001'; sleep 5",
        out.display()
    );
    let mut c = Client::new(fake(dir.path(), &body), hook).with_timeout(Duration::from_secs(2));
    let _ = c.submit(Password::from("pw".to_string()));
    let umask = std::fs::read_to_string(&out).unwrap();
    assert_eq!(umask.trim(), "0077");
}

// ---- conversations (m6-lock) -------------------------------------------------

/// A client for a fake helper speaking conversations (`body` after
/// [`SPEAK`]), with a 1.5 s timeout (so a password must be answered
/// within 4.5 s).
fn talker(dir: &Path, body: &str) -> Client {
    Client::new(fake(dir, &format!("{SPEAK}\n{body}")), no_hook)
        .with_timeout(Duration::from_millis(1500))
}

/// What a conversation's `said` saw, in order.
#[derive(Debug, PartialEq)]
enum Seen {
    Message(String),
    Prompt(String),
}

fn seen(s: Said<'_>) -> Seen {
    match s {
        Said::Message(m) => Seen::Message(m.to_string()),
        Said::Prompt(p) => Seen::Prompt(p.to_string()),
    }
}

/// A text reaches the client while the helper still waits on what it
/// asked for: the fake helper goes on only once the client has said it
/// saw the text (it waits for a file the test's `said` writes), as a
/// fingerprint module waits for the finger its text asked for. Texts
/// that came only with the verdict would leave it waiting.
#[test]
fn a_text_arrives_while_pam_still_waits() {
    let dir = tempfile::tempdir().unwrap();
    let ack = dir.path().join("ack");
    let mut c = talker(
        dir.path(),
        &format!(
            r#"hello; frame; info "Place your finger"
            while [ ! -e {ack} ]; do sleep 0.05; done
            verdict 0; sleep 5"#,
            ack = ack.display()
        ),
    );
    let (_answers, inbox) = answers().unwrap();
    let mut log = Vec::new();
    let Conversed { verdict, answered } = c.converse(inbox, &mut |s| {
        if s == Said::Message("Place your finger") {
            std::fs::write(&ack, "").unwrap();
        }
        log.push(seen(s));
    });
    assert!(verdict.is_unlocked(), "{verdict:?}");
    assert!(!answered, "no password was asked for");
    assert_eq!(log, [Seen::Message("Place your finger".into())]);
}

/// With no password waiting, PAM may stay quiet past the timeout (a
/// fingerprint module's wait); a prompt is then answered by the password
/// sent after it.
#[test]
fn silence_with_no_password_waiting_has_no_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let got = dir.path().join("got");
    let mut c = talker(
        dir.path(),
        &format!(
            r#"hello; frame; info "Place your finger"; sleep 2; prompt "Password: "
            frame {got}; verdict 0; sleep 5"#,
            got = got.display()
        ),
    );
    let (answers, inbox) = answers().unwrap();
    let mut log = Vec::new();
    let out = c.converse(inbox, &mut |s| {
        if let Said::Prompt(_) = s {
            assert!(answers.send(Password::from("typed".to_string())));
        }
        log.push(seen(s));
    });
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert!(out.answered);
    assert_eq!(
        log,
        [
            Seen::Message("Place your finger".into()),
            Seen::Prompt("Password: ".into())
        ]
    );
    assert_eq!(std::fs::read(&got).unwrap(), b"typed");
}

/// A password sent before PAM asks waits for the prompt: past the
/// timeout (the module ahead of the prompt is silent for 2 s, the
/// timeout is 1.5 s) but within the limit (4.5 s), and then it is the
/// answer.
#[test]
fn a_password_typed_early_waits_for_the_prompt_within_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let got = dir.path().join("got");
    let mut c = talker(
        dir.path(),
        &format!(
            r#"hello; frame; info "Place your finger"; sleep 2; prompt "Password: "
            frame {got}; verdict 0; sleep 5"#,
            got = got.display()
        ),
    );
    let (answers, inbox) = answers().unwrap();
    assert!(answers.send(Password::from("early".to_string())));
    assert!(
        !answers.send(Password::from("second".to_string())),
        "one password waits at a time"
    );
    let out = c.converse(inbox, &mut |_| {});
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert_eq!(std::fs::read(&got).unwrap(), b"early");
}

/// PAM saying something keeps a check with a password alive past the
/// timeout (a text every 0.4 s for 2 s against a 1.5 s timeout)...
#[test]
fn texts_keep_a_check_alive_past_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; prompt "Password: "; frame
        for i in 1 2 3 4 5; do sleep 0.4; info "still checking $i"; done
        verdict 0; sleep 5"#,
    );
    let (answers, inbox) = answers().unwrap();
    assert!(answers.send(Password::from("pw".to_string())));
    let mut texts = 0;
    let out = c.converse(inbox, &mut |s| {
        if let Said::Message(_) = s {
            texts += 1;
        }
    });
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert_eq!(texts, 5);
}

/// ...but not past the limit: a helper that keeps talking and never
/// answers the password times out, and is killed.
#[test]
fn a_check_that_talks_forever_times_out_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; prompt "Password: "; frame
        while :; do sleep 0.3; info "still checking"; done"#,
    );
    let (answers, inbox) = answers().unwrap();
    assert!(answers.send(Password::from("pw".to_string())));
    let out = c.converse(inbox, &mut |_| {});
    assert!(
        matches!(out.verdict, Verdict::Failed(AuthError::Timeout)),
        "{:?}",
        out.verdict
    );
    assert_eq!(c.helper_pid(), None, "the helper was killed");
}

/// Once PAM has a password it must say something within the timeout.
#[test]
fn a_quiet_check_with_a_password_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; prompt "Password: "; frame; sleep 30"#,
    );
    let (answers, inbox) = answers().unwrap();
    assert!(answers.send(Password::from("pw".to_string())));
    let out = c.converse(inbox, &mut |_| {});
    assert!(
        matches!(out.verdict, Verdict::Failed(AuthError::Timeout)),
        "{:?}",
        out.verdict
    );
}

/// Dropping the answers ends a conversation that waits on the user (here
/// at a prompt, after a silence longer than the timeout): it is
/// cancelled, the helper killed, and nothing was answered.
#[test]
fn dropping_the_answers_cancels_a_conversation_waiting_on_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; info "Place your finger"; sleep 2; prompt "Password: "; frame; sleep 30"#,
    );
    let (answers, inbox) = answers().unwrap();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let out = c.converse(inbox, &mut |s| {
            let _ = tx.send(seen(s));
        });
        (out, c.helper_pid())
    });
    assert_eq!(
        rx.recv().unwrap(),
        Seen::Message("Place your finger".into())
    );
    assert_eq!(rx.recv().unwrap(), Seen::Prompt("Password: ".into()));
    drop(answers);
    let (out, pid) = thread.join().unwrap();
    assert!(
        matches!(out.verdict, Verdict::Failed(AuthError::Cancelled)),
        "{:?}",
        out.verdict
    );
    assert!(!out.answered);
    assert_eq!(pid, None, "the helper was killed");
}

/// A conversation whose answers go before PAM asks (while a module
/// waits) also ends, without waiting for the prompt.
#[test]
fn dropping_the_answers_cancels_before_any_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; info "Place your finger"; sleep 30"#,
    );
    let (answers, inbox) = answers().unwrap();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        c.converse(inbox, &mut |s| {
            let _ = tx.send(seen(s));
        })
    });
    assert_eq!(
        rx.recv().unwrap(),
        Seen::Message("Place your finger".into())
    );
    drop(answers);
    let out = thread.join().unwrap();
    assert!(
        matches!(out.verdict, Verdict::Failed(AuthError::Cancelled)),
        "{:?}",
        out.verdict
    );
}

/// A password already with PAM is still answered after the answers are
/// dropped (a check outlives whoever asked for it).
#[test]
fn a_password_with_pam_outlives_its_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; prompt "Password: "; frame; sleep 0.5; verdict 0; sleep 5"#,
    );
    let (answers, inbox) = answers().unwrap();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        c.converse(inbox, &mut |s| {
            let _ = tx.send(seen(s));
        })
    });
    assert_eq!(rx.recv().unwrap(), Seen::Prompt("Password: ".into()));
    assert!(answers.send(Password::from("pw".to_string())));
    drop(answers);
    let out = thread.join().unwrap();
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert!(out.answered);
}

/// A password sent after the conversation ended (the user's Return as
/// PAM gave its verdict) reached nobody: `send` still takes it, and
/// `take_back` gives it back, so its sender can begin the next
/// conversation with it rather than lose it (m6-lock, lock-r1-5).
#[test]
fn a_password_sent_too_late_is_taken_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; info "The account is locked"; verdict 1; sleep 5"#,
    );
    let (answers, inbox) = answers().unwrap();
    let out = c.converse(inbox, &mut |_| {});
    assert!(matches!(out.verdict, Verdict::Denied { .. }));
    assert!(!out.answered);
    assert!(answers.take_back().is_none(), "nothing was sent");
    assert!(answers.send(Password::from("late".to_string())));
    let back = answers.take_back().expect("the late password comes back");
    assert_eq!(back.as_bytes(), b"late");
    assert!(answers.take_back().is_none(), "taken once");
}

/// Each prompt takes the next password (a code after the password);
/// `submit` answers every prompt with its one password, as before.
#[test]
fn each_prompt_takes_the_next_password_and_submit_repeats_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (dir.path().join("one"), dir.path().join("two"));
    let body = format!(
        r#"while :; do frame; prompt "Password: "; frame {one}; prompt "Code: "; frame {two}
        verdict 0; done"#,
        one = one.display(),
        two = two.display()
    );
    let mut c = talker(dir.path(), &format!("hello\n{body}"));
    let (answers, inbox) = answers().unwrap();
    let out = c.converse(inbox, &mut |s| match s {
        Said::Prompt("Password: ") => assert!(answers.send(Password::from("first".to_string()))),
        Said::Prompt("Code: ") => assert!(answers.send(Password::from("123456".to_string()))),
        s => panic!("{s:?}"),
    });
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert_eq!(std::fs::read(&one).unwrap(), b"first");
    assert_eq!(std::fs::read(&two).unwrap(), b"123456");

    // The same helper, the next conversation: `submit`.
    let v = c.submit(Password::from("pw".to_string()));
    assert!(v.is_unlocked(), "{v:?}");
    assert_eq!(std::fs::read(&one).unwrap(), b"pw");
    assert_eq!(std::fs::read(&two).unwrap(), b"pw");
    assert_eq!(c.spawns(), 1);
}

/// A password PAM cannot take is wiped unsent and said; the prompt then
/// takes the next one.
#[test]
fn a_password_pam_cannot_take_is_refused_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let got = dir.path().join("got");
    let mut c = talker(
        dir.path(),
        &format!(
            r#"hello; frame; prompt "Password: "; frame {got}; verdict 0; sleep 5"#,
            got = got.display()
        ),
    );
    let (answers, inbox) = answers().unwrap();
    let mut refused = Vec::new();
    let out = c.converse(inbox, &mut |s| {
        let next = match s {
            Said::Prompt(_) => "x".repeat(600),
            Said::Message(m) => {
                refused.push(m.to_string());
                (if refused.len() == 1 {
                    "nul\0byte"
                } else {
                    "fine"
                })
                .to_string()
            }
        };
        assert!(answers.send(Password::from(next)));
    });
    assert!(out.verdict.is_unlocked(), "{:?}", out.verdict);
    assert_eq!(std::fs::read(&got).unwrap(), b"fine");
    assert!(refused[0].contains("longer than 511 bytes"), "{refused:?}");
    assert!(refused[1].contains("NUL"), "{refused:?}");
}

/// A frame of the wrong kind mid-conversation (a second hello) is a
/// protocol error, and the helper is killed.
#[test]
fn a_hello_mid_conversation_is_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(dir.path(), r#"hello; frame; hello; sleep 5"#);
    let v = c.submit(Password::from("pw".to_string()));
    assert!(
        matches!(v, Verdict::Failed(AuthError::Protocol(_))),
        "{v:?}"
    );
    assert_eq!(c.helper_pid(), None);
}

/// `submit` returns what PAM said with a denial.
#[test]
fn submit_joins_what_pam_said_into_a_denial() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = talker(
        dir.path(),
        r#"hello; frame; info "1 failed attempt"; prompt "Password: "; frame
        printf '\005\000\000\000\003\001no!'; sleep 5"#,
    );
    let v = c.submit(Password::from("pw".to_string()));
    let Verdict::Denied { message } = v else {
        panic!("{v:?}");
    };
    assert_eq!(message.as_deref(), Some("1 failed attempt\nno!"));
}

/// A conversation that cannot start says whether a password was in it:
/// the `auth` store shows the fallback only for one that was.
#[test]
fn a_conversation_that_cannot_start_says_whether_it_had_a_password() {
    let mut c = Client::new("/nonexistent/strand-auth".into(), no_hook);
    for waiting in [false, true] {
        let (answers, inbox) = answers().unwrap();
        if waiting {
            assert!(answers.send(Password::from("pw".to_string())));
        }
        let out = c.converse(inbox, &mut |_| {});
        assert!(
            matches!(out.verdict, Verdict::Failed(AuthError::Spawn(_))),
            "{:?}",
            out.verdict
        );
        assert_eq!(out.answered, waiting);
    }
}

//! The client against fake helpers (shell scripts speaking, or failing
//! to speak, the protocol): garbage, early exits and silence all fail
//! closed; only a success verdict mints a token.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_auth::{AuthError, Client, Password, Verdict};

fn no_hook() {}

/// A fake helper: `body` after `#!/bin/sh`.
fn fake(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-helper");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The bytes of a `HELLO` (version 1, `strand`).
const HELLO: &str = r"\003\000\000\000\001\001\000";

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
        // Another protocol version.
        (r"printf '\003\000\000\000\001\002\000'; sleep 5", |v| {
            matches!(v, Verdict::Failed(AuthError::Protocol(_)))
        }),
        // A verdict before the hello.
        (r"printf '\001\000\000\000\003\000'; sleep 5", |v| {
            matches!(v, Verdict::Failed(AuthError::Protocol(_)))
        }),
        // An unknown verdict code.
        (
            r"printf '\003\000\000\000\001\001\000'; sleep 0.2; printf '\002\000\000\000\003\011'; sleep 5",
            |v| matches!(v, Verdict::Failed(AuthError::Protocol(_))),
        ),
        // A frame cut short by an exit.
        (
            r"printf '\003\000\000\000\001\001\000'; sleep 0.2; printf '\011\000\000\000\003'",
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
        (r"printf '\003\000\000\000\001\001\000'; sleep 30", |v| {
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

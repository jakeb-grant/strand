//! Tier A (docs/m4-plan.md, "Lock fails closed under faults"): the real
//! helper against private PAM stacks (`pam_start_confdir`, through the
//! `faults` feature's `STRAND_AUTH_PAM_CONFDIR`), never the host's PAM.
//! `pam_permit`, `pam_deny` and `pam_exec` stand in for real modules;
//! `pam_exec expose_authtok` checks the password, so right, wrong and
//! empty passwords are tested without `pam_unix` (the lock VM tests
//! that).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use strand_auth::{AuthError, Client, Password, Service, Verdict, take_service_warning};

const HELPER: &str = env!("CARGO_BIN_EXE_strand-auth");

fn no_hook() {}

/// A client for the real helper with `dir` as its PAM confdir.
fn client(dir: &Path) -> Client {
    Client::new(HELPER.into(), no_hook)
        .with_test_env(&[("STRAND_AUTH_PAM_CONFDIR", dir.to_str().unwrap())])
        .with_timeout(Duration::from_secs(20))
}

/// Writes PAM service `name` into `dir`: one `auth` and one `account`
/// line.
fn service(dir: &Path, name: &str, auth: &str, account: &str) {
    std::fs::write(
        dir.join(name),
        format!("auth required {auth}\naccount required {account}\n"),
    )
    .unwrap();
}

/// An executable shell script in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

fn submit(c: &mut Client, password: &str) -> Verdict {
    c.submit(Password::from(password.to_string()))
}

#[test]
fn a_permit_stack_unlocks() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_permit.so", "pam_permit.so");
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(v.is_unlocked(), "{v:?}");
    assert_eq!(c.service(), Some(Service::Strand));
    // One helper answers every password of a lock session.
    assert!(submit(&mut c, "again").is_unlocked());
    assert_eq!(c.spawns(), 1);
}

#[test]
fn a_deny_stack_does_not_unlock() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_deny.so", "pam_permit.so");
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Denied { .. }), "{v:?}");
}

#[test]
fn the_right_password_unlocks_and_a_wrong_or_empty_one_does_not() {
    let dir = tempfile::tempdir().unwrap();
    // pam_exec hands the password on stdin (NUL-terminated).
    let check = script(
        dir.path(),
        "check",
        r#"pw=$(tr -d '\000'); [ "$pw" = "right horse" ]"#,
    );
    service(
        dir.path(),
        "strand",
        &format!("pam_exec.so expose_authtok quiet {check}"),
        "pam_permit.so",
    );
    let mut c = client(dir.path());
    let wrong = submit(&mut c, "wrong horse");
    assert!(!wrong.is_unlocked(), "{wrong:?}");
    let empty = submit(&mut c, "");
    assert!(!empty.is_unlocked(), "{empty:?}");
    let right = submit(&mut c, "right horse");
    assert!(right.is_unlocked(), "{right:?}");
    // A password with a NUL is refused before PAM sees it.
    let nul = submit(&mut c, "right horse\0tail");
    assert!(matches!(nul, Verdict::Denied { .. }), "{nul:?}");
}

#[test]
fn a_refused_account_does_not_unlock() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_permit.so", "pam_deny.so");
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(!v.is_unlocked(), "{v:?}");
}

#[test]
fn a_failing_module_does_not_unlock() {
    let dir = tempfile::tempdir().unwrap();
    service(
        dir.path(),
        "strand",
        "pam_exec.so quiet /bin/false",
        "pam_permit.so",
    );
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(!v.is_unlocked(), "{v:?}");
}

#[test]
fn a_broken_stack_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    // A module that does not exist, and a stack line PAM cannot parse.
    service(
        dir.path(),
        "strand",
        "pam_strand_no_such_module.so",
        "pam_permit.so",
    );
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(!v.is_unlocked(), "{v:?}");
    std::fs::write(dir.path().join("strand"), "this is not a PAM stack\n").unwrap();
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(!v.is_unlocked(), "{v:?}");
}

#[test]
fn a_hanging_module_times_out_and_its_processes_are_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let hang = script(
        dir.path(),
        "hang",
        &format!("echo $$ > {}; exec sleep 600", pidfile.display()),
    );
    service(
        dir.path(),
        "strand",
        &format!("pam_exec.so quiet {hang}"),
        "pam_permit.so",
    );
    let mut c = client(dir.path()).with_timeout(Duration::from_secs(1));
    let helper = c.helper_pid().unwrap();
    let start = Instant::now();
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::Timeout)), "{v:?}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    // The helper and the module's process both went (the helper's
    // process group is killed).
    let sleeper: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone(helper as i32), "the helper {helper} still runs");
    assert!(
        gone(sleeper),
        "the hung module's process {sleeper} still runs"
    );
    assert_eq!(c.helper_pid(), None);
    // The next password starts a new helper.
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::Timeout)), "{v:?}");
    assert_eq!(c.spawns(), 2);
}

/// `pid` is gone, or a zombie nobody has reaped yet, within 2 s.
fn gone(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        let alive = match &stat {
            Err(_) => false,
            Ok(s) => s
                .rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')),
        };
        if !alive {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// decisions.md, m4-owner: no `strand` service falls back to `login`
/// with one warning per process; with neither, PAM's `other` (missing
/// here too) fails closed. The only test in this binary that reads the
/// process-wide warning.
#[test]
fn a_missing_strand_service_falls_back_to_login_with_one_warning() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "login", "pam_permit.so", "pam_permit.so");
    let mut c = client(dir.path());
    assert_eq!(take_service_warning(), None, "no helper has said yet");
    let v = submit(&mut c, "anything");
    assert!(v.is_unlocked(), "{v:?}");
    assert_eq!(c.service(), Some(Service::Login));
    let warning = take_service_warning().expect("the fallback warns");
    assert!(warning.contains("/etc/pam.d/strand"), "{warning}");
    assert!(warning.contains("login"), "{warning}");
    // (m4-audit) The helper also falls back for a file that exists but
    // cannot be read or is in a vendor directory libpam ignores, and the
    // hello does not say which: the text names every case, and the stack
    // README recommends.
    assert!(!warning.contains("is missing,"), "{warning}");
    for case in [
        "missing",
        "unreadable",
        "/usr/lib/pam.d",
        "system-auth",
        "common-auth",
    ] {
        assert!(warning.contains(case), "no {case:?} in {warning}");
    }
    // Once per process, however many helpers say it.
    let mut again = client(dir.path());
    assert!(submit(&mut again, "x").is_unlocked());
    assert_eq!(take_service_warning(), None);

    // `strand` present: it is used even when `login` would permit.
    service(dir.path(), "strand", "pam_deny.so", "pam_permit.so");
    let mut c = client(dir.path());
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Denied { .. }), "{v:?}");
    assert_eq!(c.service(), Some(Service::Strand));

    // Neither: nothing unlocks.
    let empty = tempfile::tempdir().unwrap();
    let mut c = client(empty.path());
    let v = submit(&mut c, "anything");
    assert!(!v.is_unlocked(), "{v:?}");
}

/// The helper itself says nothing on stderr (strand's) about the
/// `login` fallback: it is started again on every lock and after every
/// fault, and the warning is once per process, from its hello.
#[test]
fn the_helper_does_not_repeat_the_fallback_warning_on_stderr() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "login", "pam_permit.so", "pam_permit.so");
    let mut child = Command::new(HELPER)
        .env("STRAND_AUTH_PAM_CONFDIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its hello (the fallback is in it), then the end of its input.
    let mut hello = [0u8; 1];
    child.stdout.take().unwrap().read_exact(&mut hello).unwrap();
    drop(child.stdin.take());
    child.wait().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(!err.contains("login"), "the helper warned itself: {err}");
}

#[test]
fn a_helper_that_died_while_idle_is_started_again() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_permit.so", "pam_permit.so");
    let mut c = client(dir.path());
    let pid = c.helper_pid().unwrap() as i32;
    // SAFETY: kill(2) takes no pointers; the pid is this test's child.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(gone(pid));
    let v = submit(&mut c, "anything");
    assert!(v.is_unlocked(), "{v:?}");
    assert_eq!(c.spawns(), 2);
}

#[test]
fn a_missing_helper_fails_closed() {
    let mut c = Client::new("/nonexistent/strand-auth".into(), no_hook);
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::Spawn(_))), "{v:?}");
    assert_eq!(c.spawns(), 0);
}

/// The helper's `STRAND_FAULT` points: a crash, a hang and garbage are
/// each a failure, and the next password gets a new helper.
#[test]
fn helper_faults_fail_closed_and_respawn() {
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_permit.so", "pam_permit.so");
    let confdir = dir.path().to_str().unwrap();
    let faulty = |fault: &str| {
        Client::new(HELPER.into(), no_hook)
            .with_test_env(&[
                ("STRAND_AUTH_PAM_CONFDIR", confdir),
                ("STRAND_FAULT", fault),
            ])
            .with_timeout(Duration::from_secs(2))
    };

    let mut c = faulty("auth_crash");
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::HelperDied)), "{v:?}");
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::HelperDied)), "{v:?}");
    assert_eq!(c.spawns(), 2);

    let mut c = faulty("auth_hang");
    let v = submit(&mut c, "anything");
    assert!(matches!(v, Verdict::Failed(AuthError::Timeout)), "{v:?}");

    let mut c = faulty("auth_garbage");
    let pid = c.helper_pid().unwrap() as i32;
    let v = submit(&mut c, "anything");
    assert!(
        matches!(v, Verdict::Failed(AuthError::Protocol(_))),
        "{v:?}"
    );
    assert!(gone(pid), "a helper that answered garbage is killed");

    // A fault list names several; an unrelated one changes nothing.
    let mut c = faulty("logic_panic, other");
    assert!(submit(&mut c, "anything").is_unlocked());
}

/// PAM takes at most `MAX_PASSWORD` bytes. A longer password is refused,
/// never cut to a prefix that PAM would then accept.
#[test]
fn an_overlong_password_is_refused_not_cut() {
    use strand_auth::protocol::MAX_PASSWORD;
    let dir = tempfile::tempdir().unwrap();
    // pam_exec hands the password on stdin (NUL-terminated); the right
    // one is exactly as long as PAM takes.
    let right = "a".repeat(MAX_PASSWORD);
    let check = script(
        dir.path(),
        "check",
        &format!(r#"pw=$(tr -d '\000'); [ "$pw" = "{right}" ]"#),
    );
    service(
        dir.path(),
        "strand",
        &format!("pam_exec.so expose_authtok quiet {check}"),
        "pam_permit.so",
    );
    let mut c = client(dir.path());
    let v = submit(&mut c, &right);
    assert!(v.is_unlocked(), "{v:?}");
    for extra in ["a", "b", &"z".repeat(3000)] {
        let v = submit(&mut c, &format!("{right}{extra}"));
        assert!(matches!(v, Verdict::Denied { .. }), "{extra:?}: {v:?}");
    }
}

/// The helper refuses an overlong password itself, whatever sends it:
/// with a stack that accepts anything, a 600-byte password is denied.
#[test]
fn the_helper_refuses_an_overlong_password() {
    use std::process::{Command, Stdio};
    use strand_auth::protocol::{Code, MAX_PASSWORD, Message, read_message, write_message};
    let dir = tempfile::tempdir().unwrap();
    service(dir.path(), "strand", "pam_permit.so", "pam_permit.so");
    let mut child = Command::new(HELPER)
        .env_clear()
        .env("STRAND_AUTH_PAM_CONFDIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    assert!(matches!(
        read_message(&mut output).unwrap(),
        Message::Hello { .. }
    ));
    let mut verdict = |pw: String| {
        write_message(&mut input, &Message::Submit(Password::from(pw))).unwrap();
        match read_message(&mut output).unwrap() {
            Message::Verdict { code, .. } => code,
            m => panic!("{m:?}"),
        }
    };
    assert_eq!(verdict("x".repeat(MAX_PASSWORD)), Code::Success, "control");
    assert_eq!(verdict("x".repeat(MAX_PASSWORD + 1)), Code::Denied);
    assert_eq!(verdict("x".repeat(600)), Code::Denied);
    drop(input);
    assert!(child.wait().unwrap().success());
}

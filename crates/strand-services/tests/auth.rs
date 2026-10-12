//! `auth` against fake helpers (shell scripts speaking strand-auth's
//! protocol; the real helper and PAM are strand-auth's tests and the
//! lock VM's): `busy` while a password is checked, `failed` after a
//! refusal or a failure, an [`UnlockToken`] to the sink only on success,
//! one check at a time, the `login` fallback's single warning, and a
//! check that outlives its store still delivering, with its session tag.
//! (m6-lock) PAM's texts and prompts in `message` and `prompt` as they
//! come, a conversation kept open while the lock signal says a lock is
//! shown (a fingerprint unlocks with no password; a password typed while
//! it waits is tried after it), and a stack that ends at once not
//! looping.
//!
//! [`UnlockToken`]: strand_auth::UnlockToken
//!
//! One test: `auth::configure` is process-wide.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::auth::{self, AuthAction, AuthConfig, FailureSink, LockSignal, UnlockSink};
use strand_services::{Buses, ServiceDiagnostic};
use support::*;

/// Shell functions for fake helpers that speak conversations (as in
/// strand-auth's tests/client.rs): `hello SERVICE`, `frame [FILE]`,
/// `info TEXT`, `prompt TEXT`, `verdict CODE`.
const SPEAK: &str = r#"
hello() { printf "$(printf '\\003\\000\\000\\000\\001\\002\\%03o' "$1")"; }
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

/// An executable fake helper `name` in `dir`: [`SPEAK`], then `body`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{SPEAK}\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake helper: says hello (with `service`: 0 `strand`, 1 `login`),
/// then for each conversation asks for a password, counts it in
/// `dir/<name>.asked`, and after `delay` answers `verdict` (0 success,
/// 1 denied, 2 error).
fn fake(dir: &Path, name: &str, service: u8, verdict: u8, delay: &str) -> PathBuf {
    let asked = dir.join(format!("{name}.asked"));
    script(
        dir,
        name,
        &format!(
            "hello {service}\nwhile frame; do\n  prompt 'Password: '; frame\n  echo . >> {asked}\n  \
             sleep {delay}\n  verdict {verdict}\ndone",
            asked = asked.display()
        ),
    )
}

/// A fake helper shaped like `pam_fprintd` ahead of `pam_unix`: each
/// conversation is counted in `dir/<name>.begun`, says "Place your
/// finger", waits up to `wait` seconds for the file `dir/finger`
/// (removed when seen) and succeeds on it; otherwise it counts the wait
/// in `dir/<name>.waited`, asks for the password and takes only `right`.
fn reader(dir: &Path, name: &str, wait: u32) -> PathBuf {
    let finger = dir.join("finger");
    let begun = dir.join(format!("{name}.begun"));
    let got = dir.join(format!("{name}.got"));
    let waited = dir.join(format!("{name}.waited"));
    script(
        dir,
        name,
        &format!(
            r#"hello 0
while frame; do
  echo . >> {begun}
  info "Place your finger"
  n=0
  while [ ! -e {finger} ] && [ $n -lt {ticks} ]; do sleep 0.05; n=$((n + 1)); done
  if [ -e {finger} ]; then rm -f {finger}; verdict 0; continue; fi
  echo . >> {waited}
  prompt "Password: "; frame {got}
  if [ "$(cat {got})" = right ]; then verdict 0; else verdict 1; fi
done"#,
            begun = begun.display(),
            finger = finger.display(),
            got = got.display(),
            waited = waited.display(),
            ticks = wait * 20,
        ),
    )
}

fn count(dir: &Path, file: &str) -> usize {
    std::fs::read_to_string(dir.join(file))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// The session tag checks read when they begin.
static SESSION: std::sync::LazyLock<Arc<AtomicUsize>> =
    std::sync::LazyLock::new(|| Arc::new(AtomicUsize::new(1)));

fn asked(dir: &Path, name: &str) -> usize {
    std::fs::read_to_string(dir.join(format!("{name}.asked")))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

struct Harness {
    rt: Runtime,
    services: strand_services::Services,
    builtin: strand_services::Builtin,
    unlocks: Arc<AtomicUsize>,
    /// The session tags the sinks were given.
    tags: Arc<Mutex<Vec<u64>>>,
    /// What the failure sink was told.
    failures: Arc<Mutex<Vec<String>>>,
    diagnostics: Mutex<Vec<ServiceDiagnostic>>,
}

impl Harness {
    /// Starts `auth` with `helper`.
    fn start(helper: PathBuf) -> Harness {
        Harness::start_with(helper, None)
    }

    /// Starts `auth` with `helper` and the binary's lock signal.
    fn start_with(helper: PathBuf, lock: Option<LockSignal>) -> Harness {
        let unlocks = Arc::new(AtomicUsize::new(0));
        let tags = Arc::new(Mutex::new(Vec::new()));
        let u = unlocks.clone();
        let t = tags.clone();
        let sink: UnlockSink = Arc::new(move |_token, tag| {
            t.lock().unwrap().push(tag);
            u.fetch_add(1, Ordering::SeqCst);
        });
        let failures = Arc::new(Mutex::new(Vec::new()));
        let f = failures.clone();
        let t = tags.clone();
        let failed: FailureSink = Arc::new(move |why, tag| {
            t.lock().unwrap().push(tag);
            f.lock().unwrap().push(why.to_string());
        });
        // The binary's lock session generation, as it reads at a submit.
        let session = SESSION.clone();
        auth::configure(Some(AuthConfig {
            helper: Some(helper),
            timeout: Duration::from_secs(5),
            sink: Some(sink),
            failed: Some(failed),
            session: Some(Arc::new(move || session.load(Ordering::SeqCst) as u64)),
            client_hook: None,
            lock,
        }));
        let rt = Runtime::new();
        let (services, builtin) = services(&rt, Buses::none());
        builtin.auth.acquire(&rt);
        assert!(services.wait_ready(&rt, Duration::from_secs(10)));
        rt.flush();
        Harness {
            rt,
            services,
            builtin,
            unlocks,
            tags,
            failures,
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    fn submit(&self, password: &str) {
        self.builtin
            .auth
            .act(
                &self.rt,
                AuthAction::Submit {
                    password: password.to_string(),
                },
            )
            .unwrap();
        self.rt.flush();
    }

    fn busy(&self) -> bool {
        self.builtin.auth.cells().busy.get_untracked(&self.rt) == Ok(true)
    }

    fn failed(&self) -> bool {
        self.builtin.auth.cells().failed.get_untracked(&self.rt) == Ok(true)
    }

    fn message(&self) -> String {
        self.builtin
            .auth
            .cells()
            .message
            .get_untracked(&self.rt)
            .unwrap_or_default()
    }

    fn prompt(&self) -> String {
        self.builtin
            .auth
            .cells()
            .prompt
            .get_untracked(&self.rt)
            .unwrap_or_default()
    }

    fn unlocks(&self) -> usize {
        self.unlocks.load(Ordering::SeqCst)
    }

    fn until(&self, what: &str, cond: impl Fn(&Harness) -> bool) {
        until(&self.rt, &self.services, what, || {
            self.diagnostics
                .lock()
                .unwrap()
                .extend(self.services.take_diagnostics());
            cond(self)
        });
    }

    fn warnings(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .unwrap()
            .extend(self.services.take_diagnostics());
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.service == "auth")
            .map(|d| d.message.clone())
            .collect()
    }

    fn stop(self) {
        self.builtin.auth.release(&self.rt);
        self.services.shutdown();
    }
}

#[test]
fn auth_checks_through_the_helper_and_only_success_unlocks() {
    let dir = tempfile::tempdir().unwrap();

    // A refusal: busy while checked, then failed, and nothing unlocks.
    let h = Harness::start(fake(dir.path(), "deny", 0, 1, "0.3"));
    assert!(!h.busy() && !h.failed());
    h.submit("wrong");
    h.until("busy", Harness::busy);
    h.until("refused", |h| !h.busy() && h.failed());
    assert_eq!(h.unlocks.load(Ordering::SeqCst), 0);
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    assert!(
        h.failures.lock().unwrap().is_empty(),
        "a refusal is not a failure to check"
    );
    h.stop();

    // A success: the token goes to the sink; one check at a time.
    let h = Harness::start(fake(dir.path(), "permit", 0, 0, "0.5"));
    h.submit("right");
    h.until("busy", Harness::busy);
    h.submit("typed again while checking");
    h.until("accepted", |h| !h.busy() && !h.failed());
    assert_eq!(h.unlocks.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        asked(dir.path(), "permit"),
        1,
        "the second submit was dropped"
    );
    // The next one is checked again.
    h.submit("right");
    h.until("accepted again", |h| h.unlocks.load(Ordering::SeqCst) == 2);
    assert_eq!(asked(dir.path(), "permit"), 2);
    h.stop();

    // A PAM error is a failure and a warning.
    let h = Harness::start(fake(dir.path(), "error", 0, 2, "0"));
    h.submit("x");
    h.until("failed", |h| !h.busy() && h.failed());
    assert_eq!(h.unlocks.load(Ordering::SeqCst), 0);
    // The binary hears it (and shows its built-in password field).
    assert_eq!(h.failures.lock().unwrap().len(), 1);
    assert!(
        h.warnings()
            .iter()
            .any(|w| w.contains("could not be checked")),
        "{:?}",
        h.warnings()
    );
    h.stop();

    // No helper: every submit fails, with a warning.
    let h = Harness::start(dir.path().join("no-such-helper"));
    h.submit("x");
    h.until("failed", Harness::failed);
    assert_eq!(h.unlocks.load(Ordering::SeqCst), 0);
    assert!(!h.warnings().is_empty());
    assert!(!h.failures.lock().unwrap().is_empty());
    h.stop();

    // The `login` fallback: a success, and the warning once.
    let h = Harness::start(fake(dir.path(), "login", 1, 0, "0"));
    h.submit("right");
    h.until("accepted", |h| h.unlocks.load(Ordering::SeqCst) == 1);
    h.submit("right");
    h.until("accepted again", |h| h.unlocks.load(Ordering::SeqCst) == 2);
    let fallback: Vec<String> = h
        .warnings()
        .into_iter()
        .filter(|w| w.contains("/etc/pam.d/strand"))
        .collect();
    assert_eq!(fallback.len(), 1, "{fallback:?}");
    h.stop();

    // (m4-audit) A check outlives its store: a lock that reads no `auth`
    // field holds no reader, so the store stops 5 s after a submit, here
    // mid-check. The token still reaches the sink, with the session tag
    // read when the check began, not the one current when it ends.
    let h = Harness::start(fake(dir.path(), "slow", 0, 0, "1"));
    SESSION.store(4, Ordering::SeqCst);
    h.submit("right");
    h.until("busy", Harness::busy);
    SESSION.store(5, Ordering::SeqCst);
    h.builtin.auth.stop_now(&h.rt);
    h.until("accepted after the stop", |h| {
        h.unlocks.load(Ordering::SeqCst) == 1
    });
    assert_eq!(*h.tags.lock().unwrap(), [4]);
    h.stop();
    // So does a failure to check: the binary shows its fallback.
    let h = Harness::start(fake(dir.path(), "slow-error", 0, 2, "1"));
    h.submit("x");
    h.until("busy", Harness::busy);
    h.builtin.auth.stop_now(&h.rt);
    h.until("the failure after the stop", |h| {
        !h.failures.lock().unwrap().is_empty()
    });
    assert_eq!(*h.tags.lock().unwrap(), [5]);
    assert_eq!(h.unlocks.load(Ordering::SeqCst), 0);
    h.stop();

    conversations(dir.path());
    auth::configure(None);
}

/// The binary's lock sessions as `LockScreen` keeps them: each lock that
/// shows, and each that ends, bumps the session generation (the tag a
/// conversation reads when it begins) before the signal changes.
fn show(lock: &LockSignal, shown: bool) {
    SESSION.fetch_add(1, Ordering::SeqCst);
    lock.set(shown);
}

fn session() -> u64 {
    SESSION.load(Ordering::SeqCst) as u64
}

/// (m6-lock) PAM's conversation through the store, with the binary's
/// lock signal.
fn conversations(dir: &Path) {
    // Without a lock shown nothing begins; once one is, PAM's text shows
    // before any password, and a finger unlocks with none.
    let lock = LockSignal::new();
    let h = Harness::start_with(reader(dir, "fp", 3), Some(lock.clone()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(count(dir, "fp.begun"), 0, "no lock, no conversation");
    show(&lock, true);
    h.until("PAM's text", |h| h.message() == "Place your finger");
    assert!(!h.busy() && h.prompt().is_empty());
    std::fs::write(dir.join("finger"), "").unwrap();
    h.until("unlocked by the finger", |h| h.unlocks() == 1);
    assert!(!h.failed());
    assert_eq!(count(dir, "fp.begun"), 1, "an unlock begins nothing more");

    // The next lock: the reader gives up, PAM asks for the password and
    // the text goes; a wrong one is refused and PAM starts over (its text
    // again); the right one, typed while the reader waits, stays busy
    // through the reader's wait and unlocks once PAM asks for it.
    show(&lock, false);
    h.until("the texts cleared", |h| h.message().is_empty());
    show(&lock, true);
    h.until("PAM's text again", |h| h.message() == "Place your finger");
    h.until("the password prompt", |h| h.prompt() == "Password: ");
    assert!(h.message().is_empty(), "a prompt clears the text");
    h.submit("wrong");
    h.until("refused", |h| h.failed() && !h.busy());
    h.until("PAM starts over", |h| {
        h.message() == "Place your finger" && count(dir, "fp.begun") == 3
    });
    h.submit("right");
    h.until("busy while the reader waits", Harness::busy);
    h.until("unlocked by the password", |h| h.unlocks() == 2);
    assert!(
        h.prompt().is_empty(),
        "the prompt was answered, never shown"
    );

    // The lock goes while PAM waits on the user: that conversation ends
    // (no password in it) and the next lock begins its own.
    show(&lock, false);
    show(&lock, true);
    h.until("a new conversation", |_| count(dir, "fp.begun") == 4);
    let waited = count(dir, "fp.waited");
    show(&lock, false);
    h.until("cleared", |h| {
        h.message().is_empty() && h.prompt().is_empty()
    });
    // The reader would prompt after its 3 s wait, counting it (lock-r1-4):
    // a conversation still running past it was never ended.
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(
        count(dir, "fp.waited"),
        waited,
        "the conversation ended with the lock, before the reader gave up"
    );
    assert_eq!(count(dir, "fp.begun"), 4, "nothing begins while unlocked");
    assert_eq!(h.unlocks(), 2);
    h.stop();

    // (lock-r1-2) The binary ends a lock and shows the next in one turn,
    // while the first lock's conversation still waits on the reader: the
    // store may never see the signal false, and the old conversation's
    // tag is the ended session's (the binary would drop its unlock). It
    // ends, and the new lock gets a conversation of its own, which the
    // finger unlocks with the new session's tag.
    let lock = LockSignal::new();
    let h = Harness::start_with(reader(dir, "long", 60), Some(lock.clone()));
    show(&lock, true);
    h.until("the first lock's text", |h| {
        h.message() == "Place your finger" && count(dir, "long.begun") == 1
    });
    h.tags.lock().unwrap().clear();
    show(&lock, false);
    show(&lock, true);
    h.until("the new lock's own conversation", |_| {
        count(dir, "long.begun") == 2
    });
    h.until("its text", |h| h.message() == "Place your finger");
    assert!(!h.busy() && !h.failed());
    std::fs::write(dir.join("finger"), "").unwrap();
    h.until("unlocked by the finger", |h| h.unlocks() == 1);
    assert_eq!(*h.tags.lock().unwrap(), [session()], "the new lock's tag");
    assert_eq!(count(dir, "long.begun"), 2);
    h.stop();

    // (lock-r1-3) A password typed while the reader waits keeps that
    // conversation running after the lock goes; what PAM says after it
    // (a refusal's text) does not show while no lock is.
    let (go, got) = (dir.join("chatty.go"), dir.join("chatty.got"));
    let begun = dir.join("chatty.begun");
    let chatty = script(
        dir,
        "chatty",
        &format!(
            r#"hello 0
while frame; do
  echo . >> {begun}
  info "Place your finger"
  while [ ! -e {go} ]; do sleep 0.05; done
  rm -f {go}
  prompt "Password: "; frame {got}
  info "Authentication failed"
  verdict 1
done"#,
            begun = begun.display(),
            go = go.display(),
            got = got.display()
        ),
    );
    let lock = LockSignal::new();
    let h = Harness::start_with(chatty, Some(lock.clone()));
    show(&lock, true);
    h.until("PAM's text", |h| h.message() == "Place your finger");
    h.submit("typed early");
    h.until("busy while the reader waits", Harness::busy);
    show(&lock, false);
    h.until("the texts cleared", |h| h.message().is_empty());
    std::fs::write(&go, "").unwrap();
    h.until("the password checked and refused", |h| {
        got.exists() && !h.busy() && h.failed()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(h.message(), "", "no PAM text while no lock is shown");
    assert_eq!(h.prompt(), "");
    assert_eq!(
        count(dir, "chatty.begun"),
        1,
        "nothing begins while unlocked"
    );
    h.stop();

    // (lock-r2-1) The reader gives up and PAM waits on the password with
    // no deadline: the reader listens again only once the lock is woken
    // (a key or the pointer on it), which ends that conversation and
    // begins the next; a wake while the reader still waits, or at a
    // prompt no text came before, begins nothing.
    let lock = LockSignal::new();
    let h = Harness::start_with(reader(dir, "idle", 3), Some(lock.clone()));
    show(&lock, true);
    h.until("the reader gave up", |h| {
        h.prompt() == "Password: " && count(dir, "idle.waited") == 1
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(dir, "idle.begun"), 1, "nothing asks the reader again");
    lock.wake();
    h.until("the reader asked again", |h| {
        count(dir, "idle.begun") == 2 && h.message() == "Place your finger"
    });
    assert!(h.prompt().is_empty() && !h.busy() && !h.failed());
    lock.wake();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(count(dir, "idle.begun"), 2, "a wake while it listens");
    std::fs::write(dir.join("finger"), "").unwrap();
    h.until("unlocked by the finger", |h| h.unlocks() == 1);
    assert!(!h.failed());
    assert!(h.failures.lock().unwrap().is_empty());
    h.stop();
    let begun = dir.join("plain.begun");
    let plain = script(
        dir,
        "plain",
        &format!(
            r#"hello 0
while frame; do echo . >> {begun}; prompt "Password: "; frame; verdict 1; done"#,
            begun = begun.display()
        ),
    );
    let lock = LockSignal::new();
    let h = Harness::start_with(plain, Some(lock.clone()));
    show(&lock, true);
    h.until("the prompt", |h| h.prompt() == "Password: ");
    lock.wake();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(dir, "plain.begun"), 1, "no text came before it");
    assert_eq!(h.prompt(), "Password: ");
    h.stop();

    // (lock-r3-1) Texts that come at once before the prompt, with no
    // module waiting between them (a `pam_echo` banner; a
    // `pam_faillock` lockout's texts under `required`, after which PAM
    // goes on to `pam_unix`): input on the lock leaves that prompt alone,
    // however much of it there is, rather than killing the helper and
    // beginning a conversation that says the same and asks again.
    let begun = dir.join("banner.begun");
    let banner = script(
        dir,
        "banner",
        &format!(
            r#"hello 0
while frame; do
  echo . >> {begun}
  info "The account is locked due to 3 failed logins."
  info "(9 minutes left to unlock)"
  prompt "Password: "; frame; verdict 1
done"#,
            begun = begun.display()
        ),
    );
    let lock = LockSignal::new();
    let h = Harness::start_with(banner, Some(lock.clone()));
    show(&lock, true);
    h.until("the prompt", |h| h.prompt() == "Password: ");
    for _ in 0..20 {
        lock.wake();
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(dir, "banner.begun"), 1, "input began nothing");
    assert_eq!(h.prompt(), "Password: ", "the prompt stays");
    assert_eq!(
        h.message(),
        "",
        "the prompt cleared the texts and nothing said them again"
    );
    h.stop();

    // A stack that ends at once with no prompt (pam_faillock locked out):
    // its text shows, `failed` stays unset, and no conversation follows
    // until a submit, which is refused.
    let begun = dir.join("locked.begun");
    let locked = script(
        dir,
        "locked",
        &format!(
            r#"hello 0
while frame; do echo . >> {begun}; info "The account is locked"; verdict 1; done"#,
            begun = begun.display()
        ),
    );
    let lock = LockSignal::new();
    show(&lock, true);
    let h = Harness::start_with(locked, Some(lock.clone()));
    h.until("the lockout's text", |h| {
        h.message() == "The account is locked"
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(dir, "locked.begun"), 1, "no loop");
    assert!(!h.failed());
    h.submit("anything");
    h.until("refused", |h| h.failed() && !h.busy());
    // The submit's conversation, then one more (a password was given),
    // and no other.
    h.until("PAM started over once", |_| count(dir, "locked.begun") == 3);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(dir, "locked.begun"), 3, "no loop");
    assert!(h.failures.lock().unwrap().is_empty(), "refusals only");
    assert_eq!(h.unlocks(), 0);
    h.stop();

    // A second prompt (a one-time code) takes the next submit; `busy` is
    // false while PAM waits on the user.
    let (one, two) = (dir.join("otp.one"), dir.join("otp.two"));
    let otp = script(
        dir,
        "otp",
        &format!(
            r#"hello 0
while frame; do prompt "Password: "; frame {one}; prompt "Code: "; frame {two}; verdict 0; done"#,
            one = one.display(),
            two = two.display()
        ),
    );
    let h = Harness::start(otp);
    h.submit("pw");
    h.until("the code's prompt", |h| h.prompt() == "Code: " && !h.busy());
    h.submit("123456");
    h.until("unlocked", |h| h.unlocks() == 1);
    assert_eq!(std::fs::read(&one).unwrap(), b"pw");
    assert_eq!(std::fs::read(&two).unwrap(), b"123456");
    h.stop();
}

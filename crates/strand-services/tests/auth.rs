//! `auth` against fake helpers (shell scripts speaking strand-auth's
//! protocol; the real helper and PAM are strand-auth's tests and the
//! lock VM's): `busy` while a password is checked, `failed` after a
//! refusal or a failure, an [`UnlockToken`] to the sink only on success,
//! one check at a time, and the `login` fallback's single warning.
//!
//! One test: `auth::configure` is process-wide.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::auth::{self, AuthAction, AuthConfig, FailureSink, UnlockSink};
use strand_services::{Buses, ServiceDiagnostic};
use support::*;

/// A fake helper: says hello (with `service`: 0 `strand`, 1 `login`),
/// reads each password's frame and counts it in `dir/<name>.asked`, then
/// after `delay` answers
/// `verdict` (0 success, 1 denied, 2 error).
fn fake(dir: &Path, name: &str, service: u8, verdict: u8, delay: &str) -> PathBuf {
    let path = dir.join(name);
    let asked = dir.join(format!("{name}.asked"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '\\003\\000\\000\\000\\001\\001\\{service:03o}'\n\
             while len=$(head -c 4 | od -An -tu4 | tr -d ' ') && [ -n \"$len\" ]; do\n  \
             head -c \"$len\" >/dev/null\n  echo . >> {asked}\n  sleep {delay}\n  \
             printf '\\002\\000\\000\\000\\003\\{verdict:03o}'\ndone\n",
            asked = asked.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

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
    /// What the failure sink was told.
    failures: Arc<Mutex<Vec<String>>>,
    diagnostics: Mutex<Vec<ServiceDiagnostic>>,
}

impl Harness {
    /// Starts `auth` with `helper`.
    fn start(helper: PathBuf) -> Harness {
        let unlocks = Arc::new(AtomicUsize::new(0));
        let u = unlocks.clone();
        let sink: UnlockSink = Arc::new(move |_token| {
            u.fetch_add(1, Ordering::SeqCst);
        });
        let failures = Arc::new(Mutex::new(Vec::new()));
        let f = failures.clone();
        let failed: FailureSink = Arc::new(move |why| f.lock().unwrap().push(why.to_string()));
        auth::configure(Some(AuthConfig {
            helper: Some(helper),
            timeout: Duration::from_secs(5),
            sink: Some(sink),
            failed: Some(failed),
            client_hook: None,
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
    auth::configure(None);
}

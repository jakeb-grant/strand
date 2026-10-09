//! Tier C (docs/m4-plan.md): the helper against the real PAM stack
//! (`pam_unix` through the setuid `unix_chkpwd`), **inside the lock VM
//! only** (`scripts/lockvm/scenarios/pam.sh` runs it as the test user,
//! `tester`, password `strand-test`). Anywhere else it skips: it must
//! never touch the host's PAM.
//!
//! The scenario points `STRAND_LOCK_VM_HELPER` at a helper built with
//! the default features (the one that ships), and says in
//! `STRAND_LOCK_VM_SERVICE` which service the guest offers: `strand`
//! (the image's `/etc/pam.d/strand`), or `login` after the scenario
//! removed it.

use std::path::PathBuf;
use std::time::Duration;

use strand_auth::{Client, Password, Service, Verdict, take_service_warning};

fn no_hook() {}

/// The helper to test, in the lock VM's guest; `None` (skip) elsewhere.
fn in_lock_vm(test: &str) -> Option<PathBuf> {
    let asked = std::env::var_os("STRAND_LOCK_VM").is_some_and(|v| v == "1");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let init = std::fs::read("/proc/1/cmdline").unwrap_or_default();
    let vm = host.trim() == "lockvm" && init.windows(11).any(|w| w == b"lockvm-init");
    if !(asked && vm) {
        eprintln!("skipping {test}: real-PAM tests run only inside the lock VM");
        return None;
    }
    let helper = std::env::var_os("STRAND_LOCK_VM_HELPER")
        .map(PathBuf::from)
        .expect("the scenario names the helper in STRAND_LOCK_VM_HELPER");
    assert!(helper.is_file(), "{}: no helper", helper.display());
    Some(helper)
}

fn check(client: &mut Client, password: &str) -> Verdict {
    client.submit(Password::from(password.to_string()))
}

#[test]
fn pam_unix_takes_only_the_right_password() {
    let Some(helper) = in_lock_vm("pam_unix_takes_only_the_right_password") else {
        return;
    };
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    assert_eq!(uid, 1000, "the scenario runs this as `tester`");
    let expect = match std::env::var("STRAND_LOCK_VM_SERVICE").as_deref() {
        Ok("login") => Service::Login,
        Ok("strand") => Service::Strand,
        other => panic!("STRAND_LOCK_VM_SERVICE is strand or login, not {other:?}"),
    };

    let mut client = Client::new(helper, no_hook).with_timeout(Duration::from_secs(20));
    // pam_unix delays a failure by about two seconds; each refusal below
    // is checked before the next password.
    let v = check(&mut client, "wrong-password");
    assert!(matches!(v, Verdict::Denied { .. }), "wrong: {v:?}");
    let v = check(&mut client, "");
    assert!(matches!(v, Verdict::Denied { .. }), "empty: {v:?}");
    let v = check(&mut client, "strand-test\n");
    assert!(matches!(v, Verdict::Denied { .. }), "with a newline: {v:?}");
    let v = check(&mut client, "strand-test");
    assert!(v.is_unlocked(), "right: {v:?}");
    // The same helper, still running, keeps refusing a wrong one.
    let v = check(&mut client, "strand-tes");
    assert!(
        matches!(v, Verdict::Denied { .. }),
        "after a success: {v:?}"
    );
    assert_eq!(client.spawns(), 1, "one helper served every check");
    assert_eq!(client.service(), Some(expect));

    let warning = take_service_warning();
    match expect {
        Service::Login => {
            let w = warning.expect("the login fallback warns");
            assert!(w.contains("/etc/pam.d/strand"), "{w}");
            assert_eq!(take_service_warning(), None, "once");
        }
        Service::Strand => assert_eq!(warning, None),
    }
}

//! Tier C for (services-round) `polkit` and `greetd`: the real polkitd,
//! its setuid `polkit-agent-helper-1` and PAM, and the real greetd,
//! **inside the lock VM only**. Anywhere else each test skips: they must
//! never touch the host's polkit, PAM or login.
//!
//! - `polkit_agent_answers_polkitd` (scripts/lockvm/scenarios/polkit.sh
//!   runs it as `tester`, password `strand-test`, in logind-shaped
//!   session `1`, with polkitd on the guest's system bus and two
//!   administrators in group `sudo`): another agent holding the session
//!   is left be until it goes; then `pkcheck` asks for the test actions
//!   and Strand's agent answers it through the helper, a wrong password
//!   refused and asked again, the right one granting; a request dismissed
//!   by the user, one withdrawn when its program goes, another
//!   administrator chosen; and once nothing reads `polkit`, the session
//!   is free for another agent.
//! - `greetd_greeter` is greetd's greeter (scripts/lockvm/scenarios/
//!   greetd.sh names it in /etc/greetd/config.toml): it logs `tester` in
//!   with a wrong password, then the right one, starts a session that
//!   touches a file, and writes what it saw for the scenario to check.

mod support;

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::polkit::{
    self, AUTHORITY, AUTHORITY_IFACE, AUTHORITY_PATH, HELD, PolkitRequest,
};
use strand_services::{Builtin, Bus, Buses, Data, Services};
use support::*;
use zbus::zvariant::{OwnedValue, Value};

/// The actions polkit.sh installs: `auth_self` and `auth_admin`.
const SELF_ACTION: &str = "org.strand.test.self";
const ADMIN_ACTION: &str = "org.strand.test.admin";
const OTHER_PATH: &str = "/org/strand/test/OtherAgent";

/// Whether this runs in the lock VM's guest, asked for by the scenario
/// through `var`; says why not otherwise.
fn in_lock_vm(test: &str, var: &str) -> bool {
    let asked = std::env::var_os("STRAND_LOCK_VM").is_some_and(|v| v == "1");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let init = std::fs::read("/proc/1/cmdline").unwrap_or_default();
    let vm = host.trim() == "lockvm" && init.windows(11).any(|w| w == b"lockvm-init");
    if !(asked && vm && std::env::var_os(var).is_some()) {
        eprintln!("skipping {test}: real polkit and greetd tests run only inside the lock VM");
        return false;
    }
    true
}

/// Another agent, as a desktop's own would be: it answers nothing.
struct OtherAgent;

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl OtherAgent {
    async fn begin_authentication(
        &self,
        _action_id: &str,
        _message: &str,
        _icon: &str,
        _details: HashMap<String, String>,
        _cookie: &str,
        _identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::Failed(
            "the other agent answers nothing".into(),
        ))
    }

    async fn cancel_authentication(&self, _cookie: &str) {}
}

/// A connection serving [`OtherAgent`], registered for session `1`:
/// polkitd's answer.
fn other_agent(tokio: &tokio::runtime::Runtime) -> (zbus::Connection, zbus::Result<()>) {
    tokio.block_on(async {
        let conn = zbus::connection::Builder::system()
            .unwrap()
            .serve_at(OTHER_PATH, OtherAgent)
            .unwrap()
            .build()
            .await
            .unwrap();
        let subject = (
            "unix-session",
            HashMap::from([("session-id", Value::from("1"))]),
        );
        let r = conn
            .call_method(
                Some(AUTHORITY),
                AUTHORITY_PATH,
                Some(AUTHORITY_IFACE),
                "RegisterAuthenticationAgent",
                &(subject, "C", OTHER_PATH),
            )
            .await
            .map(|_| ());
        (conn, r)
    })
}

struct Polkit {
    rt: Runtime,
    services: Services,
    b: Builtin,
}

impl Polkit {
    fn until(&self, what: &str, secs: u64, cond: impl Fn(&Polkit) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.services.pump(&self.rt);
            self.rt.flush();
            if cond(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never: {what} (now {:?})",
                self.state()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn state(&self) -> (Option<PolkitRequest>, bool, bool, bool, String) {
        let c = self.b.polkit.cells();
        let rt = &self.rt;
        (
            c.request.get_untracked(rt).unwrap(),
            c.busy.get_untracked(rt).unwrap(),
            c.failed.get_untracked(rt).unwrap(),
            c.available.get_untracked(rt).unwrap(),
            c.error.get_untracked(rt).unwrap(),
        )
    }

    fn request(&self) -> Option<PolkitRequest> {
        self.state().0
    }

    fn act(&self, name: &str, args: &[Data]) {
        self.b
            .polkit
            .dynamic()
            .action(&self.rt, name, None, args, Default::default())
            .unwrap();
        self.rt.flush();
    }
}

/// `pkcheck` asking for `action` for itself, as a program asks.
fn pkcheck(action: &str) -> Child {
    Command::new("sh")
        .arg("-c")
        .arg(format!(
            "exec pkcheck --action-id {action} --process $$ --allow-user-interaction"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// `child`'s exit code, 30 s at most.
fn exit_code(mut child: Child) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            eprintln!(
                "pkcheck: {s:?}\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return s.code().unwrap_or(-1);
        }
        assert!(Instant::now() < deadline, "pkcheck never ended");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text(s: &str) -> Data {
    Data::Text(s.into())
}

#[test]
fn polkit_agent_answers_polkitd() {
    if !in_lock_vm("polkit_agent_answers_polkitd", "STRAND_LOCK_VM_POLKIT") {
        return;
    }
    // SAFETY: getuid cannot fail.
    assert_eq!(
        unsafe { libc::getuid() },
        1000,
        "polkit.sh runs this as tester"
    );
    let tokio = tokio();
    polkit::configure(None);
    let rt = Runtime::new();
    let (services, b) = services(
        &rt,
        Buses {
            session: Bus::Disabled,
            system: Bus::Default,
        },
    );
    let w = Polkit { rt, services, b };

    // Another agent holds the session: Strand leaves it be and says so.
    let (other, registered) = other_agent(&tokio);
    registered.expect("the other agent registers for session 1");
    w.b.polkit.acquire(&w.rt);
    w.until("the session held", 20, |w| w.state().4 == HELD);
    assert!(!w.state().3);
    // It goes: polkitd says its agents changed, and Strand registers.
    tokio.block_on(other.close()).unwrap();
    w.until("registered", 20, |w| w.state().3);
    assert_eq!(w.state().4, "");

    // A wrong password is refused and asked again; the right one grants.
    let pk = pkcheck(SELF_ACTION);
    w.until("asked", 20, |w| {
        w.request().is_some_and(|r| !r.prompt.is_empty())
    });
    let r = w.request().unwrap();
    assert_eq!(r.action_id, SELF_ACTION);
    assert_eq!(r.message, "Strand's lock VM wants to test polkit");
    assert_eq!(r.user, "tester");
    assert_eq!(r.users, ["tester"]);
    assert!(r.prompt.to_lowercase().contains("password"), "{r:?}");
    assert!(!r.echo);
    w.act("respond", &[text("wrong-password")]);
    w.until("refused and asked again", 30, |w| {
        let (r, busy, failed, ..) = w.state();
        failed && !busy && r.is_some_and(|r| !r.prompt.is_empty())
    });
    w.act("respond", &[text("strand-test")]);
    assert_eq!(exit_code(pk), 0, "the right password grants");
    w.until("answered", 20, |w| w.request().is_none());
    assert!(!w.state().2);

    // Dismissed by the user: the program is told so.
    let pk = pkcheck(SELF_ACTION);
    w.until("asked again", 20, |w| w.request().is_some());
    w.act("cancel", &[]);
    assert_eq!(exit_code(pk), 3, "pkcheck's code for a dismissed dialog");
    w.until("dismissed", 20, |w| w.request().is_none());

    // Withdrawn: the program goes, polkitd cancels, the dialog goes.
    let mut pk = pkcheck(SELF_ACTION);
    w.until("asked a third time", 20, |w| w.request().is_some());
    pk.kill().unwrap();
    let _ = pk.wait();
    w.until("withdrawn", 20, |w| w.request().is_none());

    // An administrator's password: either of the two, the other chosen.
    let pk = pkcheck(ADMIN_ACTION);
    w.until("an administrator asked", 20, |w| {
        w.request().is_some_and(|r| !r.prompt.is_empty())
    });
    let r = w.request().unwrap();
    assert_eq!(r.action_id, ADMIN_ACTION);
    assert_eq!(r.users, ["admin2", "admin3"]);
    assert_eq!(r.user, "admin2");
    w.act("choose", &[text("admin3")]);
    w.until("admin3 asked", 20, |w| {
        w.request()
            .is_some_and(|r| r.user == "admin3" && !r.prompt.is_empty())
    });
    w.act("respond", &[text("strand-admin3")]);
    assert_eq!(exit_code(pk), 0, "admin3's password grants");
    w.until("answered", 20, |w| w.request().is_none());

    // Nothing reads `polkit`: Strand unregisters, and the session is
    // another agent's to take.
    w.b.polkit.release(&w.rt);
    w.rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        w.services.pump(&w.rt);
        w.rt.flush();
        let (conn, r) = other_agent(&tokio);
        if r.is_ok() {
            drop(conn);
            break;
        }
        assert!(Instant::now() < deadline, "never unregistered: {r:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    w.services.shutdown();
}

/// greetd's greeter in greetd.sh: writes `ok` to
/// `$STRAND_LOCK_VM_GREETD` when the login went as it should, else what
/// went wrong.
#[test]
fn greetd_greeter() {
    if !in_lock_vm("greetd_greeter", "STRAND_LOCK_VM_GREETD") {
        return;
    }
    let result = std::path::PathBuf::from(std::env::var_os("STRAND_LOCK_VM_GREETD").unwrap());
    let outcome = std::panic::catch_unwind(greeter);
    let said = match &outcome {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!(
            "failed: {}\n",
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        ),
    };
    std::fs::write(&result, said).unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}

fn greeter() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    assert!(
        std::env::var_os(strand_services::greetd::SOCKET_VAR).is_some(),
        "greetd names its socket"
    );
    let started = Arc::new(AtomicBool::new(false));
    let flag = started.clone();
    strand_services::greetd::configure(Some(strand_services::greetd::GreetdConfig {
        socket: None,
        server_uid: None,
        started: Some(Arc::new(move || flag.store(true, Ordering::SeqCst))),
    }));
    let rt = Runtime::new();
    let (services, b) = services(&rt, Buses::none());
    b.greetd.acquire(&rt);
    let c = b.greetd.cells();
    let wait = |what: &str, cond: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            services.pump(&rt);
            rt.flush();
            if cond() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never: {what} (prompt {:?}, failed {:?}, error {:?})",
                c.prompt.get_untracked(&rt),
                c.failed.get_untracked(&rt),
                c.error.get_untracked(&rt)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let act = |name: &str, args: &[Data]| {
        b.greetd
            .dynamic()
            .action(&rt, name, None, args, Default::default())
            .unwrap();
        rt.flush();
    };
    wait("greetd answering", &|| {
        c.available.get_untracked(&rt).unwrap()
    });
    act("login", &[text("tester")]);
    wait("asked the password", &|| {
        !c.prompt.get_untracked(&rt).unwrap().is_empty()
    });
    assert!(!c.echo.get_untracked(&rt).unwrap());
    act("submit", &[text("wrong-password")]);
    wait("refused, asked again", &|| {
        c.failed.get_untracked(&rt).unwrap() && !c.prompt.get_untracked(&rt).unwrap().is_empty()
    });
    assert!(!c.error.get_untracked(&rt).unwrap().is_empty());
    act("submit", &[text("strand-test")]);
    act(
        "start",
        &[Data::List(vec![
            text("sh"),
            text("-c"),
            text("id -un > /tmp/strand-greetd-session"),
        ])],
    );
    wait("the session accepted", &|| started.load(Ordering::SeqCst));
    assert!(!c.failed.get_untracked(&rt).unwrap());
    services.shutdown();
}

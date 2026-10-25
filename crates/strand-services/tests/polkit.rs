//! (services-round) `polkit` against python-dbusmock's `polkitd`
//! template (its authority, with methods added for what the template
//! leaves out: the registration calls, and a `Begin`/`Withdraw` pair
//! that calls Strand's agent from the authority's own bus name, as
//! polkitd does) and logind's, on a private bus, with fake helpers
//! speaking polkit-agent-helper-1's line protocol (the real helper,
//! polkitd and PAM run in the lock VM: `tests/vm_auth.rs`).
//!
//! Registration for this process's session (logind's `c2`) only while
//! read and unregistration when it stops; a request shown with its
//! users, prompt and texts; a refused answer asking again and a right
//! one answering the authority; answers wiped and never cut; requests
//! queued, dismissed by the user and withdrawn by the program; choosing
//! another user; another agent holding the session left be until polkit
//! says its agents changed; the helper's socket used before the setuid
//! program; and no registration without a helper.
//!
//! `polkit::configure` is process-wide: the tests take [`LOCK`].

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::polkit::{
    self, AGENT_IFACE, AGENT_PATH, AUTHORITY, AUTHORITY_IFACE, AUTHORITY_PATH, HELD, PolkitConfig,
    PolkitRequest,
};
use strand_services::session::{LOGIND, MANAGER};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Data, Services};
use support::*;
use zbus::zvariant::OwnedValue;

static LOCK: Mutex<()> = Mutex::new(());

/// The mock authority's additions. `Begin(agent, cookie, action, uids)`
/// calls the agent's `BeginAuthentication` from the authority's bus name
/// with one `unix-user` identity per uid, and records how it ended
/// (`Outcome(cookie)`: "ok", or the error's name; "" while it runs).
/// `Withdraw(agent, cookie)` is polkitd's `CancelAuthentication`.
const BEGIN: &str = "
self.outcomes = getattr(self, 'outcomes', {})
self.outcomes[args[1]] = ''
ids = []
for u in args[3]:
    ids.append(('unix-user', {'uid': dbus.UInt32(u)}))
ok = lambda *a, s=self, c=args[1]: s.outcomes.__setitem__(c, 'ok')
err = lambda e, s=self, c=args[1]: s.outcomes.__setitem__(c, e.get_dbus_name())
self.connection.call_async(args[0], '/org/freedesktop/PolicyKit1/AuthenticationAgent', \
    'org.freedesktop.PolicyKit1.AuthenticationAgent', 'BeginAuthentication', 'sssa{ss}sa(sa{sv})', \
    (args[2], 'Strand wants to test', 'dialog-password', {}, args[1], ids), ok, err, timeout=600)
";
const OUTCOME: &str = "ret = getattr(self, 'outcomes', {}).get(args[0], 'never begun')";
const WITHDRAW: &str = "
self.connection.call_async(args[0], '/org/freedesktop/PolicyKit1/AuthenticationAgent', \
    'org.freedesktop.PolicyKit1.AuthenticationAgent', 'CancelAuthentication', 's', \
    (args[1],), lambda *a: None, lambda *e: None)
";
const MOCK_IFACE: &str = "org.freedesktop.DBus.Mock";

/// A fake polkit-agent-helper-1 in `dir`: logs its argument and the
/// cookie line (`helper.log`), says a text (escaped as the real one
/// escapes it), asks for the password and logs each answer it reads
/// (`answers.log`), `SUCCESS` for `right`, else an error and `FAILURE`.
fn fake_helper(dir: &Path) -> PathBuf {
    let path = dir.join("polkit-agent-helper-1");
    let log = dir.join("helper.log");
    let answers = dir.join("answers.log");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nread -r cookie\necho \"$1 $cookie\" >> {log}\n\
             echo 'PAM_TEXT_INFO Touch\\040the\\040key\\303\\251'\n\
             echo 'PAM_PROMPT_ECHO_OFF Password: '\n\
             IFS= read -r answer\nprintf '%s\\n' \"$answer\" >> {answers}\n\
             if [ \"$answer\" = right ]; then echo SUCCESS; \
             else echo 'PAM_ERROR_MSG Authentication failure'; echo FAILURE; fi\n",
            log = log.display(),
            answers = answers.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn me() -> String {
    polkit::user_name(uid()).unwrap()
}

/// A private bus with logind (this process in session `c2`) and polkit's
/// authority, its registrations recorded; the services; the test's own
/// connection.
struct World {
    rt: Runtime,
    services: Services,
    b: Builtin,
    conn: zbus::Connection,
    tokio: tokio::runtime::Runtime,
    _logind: DbusMock,
    _polkit: DbusMock,
    _bus: PrivateBus,
}

impl World {
    fn start(config: PolkitConfig) -> Option<World> {
        let bus = PrivateBus::start()?;
        let tokio = tokio();
        let conn = connect(&tokio, &bus.address);
        let logind = DbusMock::start(&bus, "logind", true, None, LOGIND)?;
        mock(
            &tokio,
            &conn,
            LOGIND,
            MANAGER,
            "AddSession",
            &("c2", "seat0", uid(), me().as_str(), true),
        );
        mock(
            &tokio,
            &conn,
            LOGIND,
            MANAGER,
            "AddMethod",
            &(
                "org.freedesktop.login1.Manager",
                "GetSession",
                "s",
                "o",
                "ret = '/org/freedesktop/login1/session/c2'",
            ),
        );
        let polkit = DbusMock::start(&bus, "polkitd", true, None, AUTHORITY)?;
        let add = |iface: &str, name: &str, ins: &str, outs: &str, code: &str| {
            mock(
                &tokio,
                &conn,
                AUTHORITY,
                AUTHORITY_PATH,
                "AddMethod",
                &(iface, name, ins, outs, code),
            );
        };
        add(
            AUTHORITY_IFACE,
            "RegisterAuthenticationAgent",
            "(sa{sv})ss",
            "",
            "",
        );
        add(
            AUTHORITY_IFACE,
            "UnregisterAuthenticationAgent",
            "(sa{sv})s",
            "",
            "",
        );
        add(MOCK_IFACE, "Begin", "sssau", "", BEGIN);
        add(MOCK_IFACE, "Outcome", "s", "s", OUTCOME);
        add(MOCK_IFACE, "Withdraw", "ss", "", WITHDRAW);
        polkit::configure(Some(config));
        let rt = Runtime::new();
        let (services, b) = services(&rt, bus.buses());
        Some(World {
            rt,
            services,
            b,
            conn,
            tokio,
            _logind: logind,
            _polkit: polkit,
            _bus: bus,
        })
    }

    fn until(&self, what: &str, cond: impl Fn(&World) -> bool) {
        until(&self.rt, &self.services, what, || cond(self));
    }

    fn state(&self) -> (Option<PolkitRequest>, i64, bool, bool, bool, String) {
        let c = self.b.polkit.cells();
        let rt = &self.rt;
        (
            c.request.get_untracked(rt).unwrap(),
            c.waiting.get_untracked(rt).unwrap(),
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

    fn calls(&self, method: &str) -> Vec<Vec<OwnedValue>> {
        mock_calls(&self.tokio, &self.conn, AUTHORITY, AUTHORITY_PATH, method)
    }

    /// The unique name serving Strand's agent.
    fn agent(&self) -> String {
        self.serving()
            .unwrap_or_else(|| panic!("no bus name serves Strand's agent"))
    }

    /// The unique name serving Strand's agent, if one does.
    fn serving(&self) -> Option<String> {
        let dbus = self
            .tokio
            .block_on(zbus::fdo::DBusProxy::new(&self.conn))
            .unwrap();
        let names = self.tokio.block_on(dbus.list_names()).unwrap();
        for n in names.iter().filter(|n| n.starts_with(':')) {
            let xml = self.tokio.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    self.conn.call_method(
                        Some(n.as_str()),
                        AGENT_PATH,
                        Some("org.freedesktop.DBus.Introspectable"),
                        "Introspect",
                        &(),
                    ),
                )
                .await
            });
            if let Ok(Ok(m)) = xml
                && m.body()
                    .deserialize::<String>()
                    .is_ok_and(|x| x.contains(AGENT_IFACE))
            {
                return Some(n.to_string());
            }
        }
        None
    }

    fn begin(&self, agent: &str, cookie: &str, uids: &[u32]) {
        mock(
            &self.tokio,
            &self.conn,
            AUTHORITY,
            AUTHORITY_PATH,
            "Begin",
            &(agent, cookie, "org.example.strand.test", uids.to_vec()),
        );
    }

    fn outcome(&self, cookie: &str) -> String {
        mock(
            &self.tokio,
            &self.conn,
            AUTHORITY,
            AUTHORITY_PATH,
            "Outcome",
            &(cookie,),
        )
        .body()
        .deserialize()
        .unwrap()
    }

    fn until_outcome(&self, cookie: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.services.pump(&self.rt);
            self.rt.flush();
            let got = self.outcome(cookie);
            if got == want {
                return;
            }
            assert!(Instant::now() < deadline, "{cookie}: {got:?}, not {want:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn warnings(&self) -> Vec<String> {
        self.services
            .take_diagnostics()
            .into_iter()
            .filter(|d| d.service == "polkit")
            .map(|d| d.message)
            .collect()
    }
}

fn text(s: &str) -> Data {
    Data::Text(s.into())
}

#[test]
fn a_request_is_answered_through_the_helper_and_asks_again_when_refused() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let helper = fake_helper(dir.path());
    let Some(w) = World::start(PolkitConfig {
        helper: Some(helper),
        socket: Some(dir.path().join("no.socket")),
    }) else {
        return;
    };
    // Nothing reads `polkit`: no agent.
    std::thread::sleep(Duration::from_millis(300));
    assert!(w.calls("RegisterAuthenticationAgent").is_empty());

    w.b.polkit.acquire(&w.rt);
    w.until("registered", |w| w.state().4);
    let reg = w.calls("RegisterAuthenticationAgent");
    assert_eq!(reg.len(), 1);
    let (kind, details): (String, std::collections::HashMap<String, OwnedValue>) =
        reg[0][0].clone().try_into().unwrap();
    assert_eq!(kind, "unix-session");
    assert_eq!(
        details["session-id"],
        OwnedValue::try_from(zbus::zvariant::Value::from("c2")).unwrap(),
        "logind's session for this process"
    );
    assert_eq!(
        reg[0][2],
        OwnedValue::try_from(zbus::zvariant::Value::from(AGENT_PATH)).unwrap()
    );

    let agent = w.agent();
    w.begin(&agent, "cookie-1", &[uid()]);
    w.until("shown with its prompt", |w| {
        w.request().is_some_and(|r| r.prompt == "Password: ")
    });
    let r = w.request().unwrap();
    assert_eq!(r.action_id, "org.example.strand.test");
    assert_eq!(r.message, "Strand wants to test");
    assert_eq!(r.icon, "dialog-password");
    assert_eq!(r.user, me());
    assert_eq!(r.users, vec![me()]);
    assert!(!r.echo);
    assert_eq!(w.outcome("cookie-1"), "", "the call waits");
    assert_eq!(
        lines(&dir.path().join("helper.log")),
        vec![format!("{} cookie-1", me())],
        "the helper got the user as its argument and the cookie on stdin"
    );

    // An answer the helper could not read whole is refused, not cut.
    w.act("respond", &[text("two\nlines")]);
    w.until("refused", |w| w.state().3);
    assert!(!w.state().2, "not busy");
    // A wrong answer: refused, and the dialog asks again.
    w.act("respond", &[text("wrong")]);
    w.until("refused by the helper, asking again", |w| {
        let (r, _, busy, failed, ..) = w.state();
        failed && !busy && r.is_some_and(|r| r.prompt == "Password: ")
    });
    assert_eq!(w.outcome("cookie-1"), "");
    w.act("respond", &[text("right")]);
    w.until_outcome("cookie-1", "ok");
    w.until("gone", |w| w.request().is_none());
    let (_, waiting, busy, failed, available, error) = w.state();
    assert_eq!((waiting, busy, failed, available), (0, false, false, true));
    assert_eq!(error, "");
    assert_eq!(
        lines(&dir.path().join("answers.log")),
        vec!["wrong".to_string(), "right".to_string()],
        "each answer went to the helper as one line, the refused one never"
    );
    assert_eq!(
        lines(&dir.path().join("helper.log")).len(),
        2,
        "one helper per try"
    );

    // Only polkit's authority is answered.
    let r = w.tokio.block_on(w.conn.call_method(
        Some(agent.as_str()),
        AGENT_PATH,
        Some(AGENT_IFACE),
        "BeginAuthentication",
        &(
            "x",
            "phish",
            "",
            std::collections::HashMap::<String, String>::new(),
            "c",
            Vec::<(String, std::collections::HashMap<String, OwnedValue>)>::new(),
        ),
    ));
    match r {
        Err(zbus::Error::MethodError(name, _, _)) => {
            assert_eq!(name.as_str(), "org.freedesktop.PolicyKit1.Error.Failed")
        }
        other => panic!("{other:?}"),
    }

    // Read no more: 5 s later it unregisters, its agent taken down.
    w.b.polkit.release(&w.rt);
    w.rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_secs(15);
    while w.calls("UnregisterAuthenticationAgent").is_empty() {
        w.services.pump(&w.rt);
        w.rt.flush();
        assert!(Instant::now() < deadline, "never unregistered");
        std::thread::sleep(Duration::from_millis(50));
    }
    w.until("taken down", |w| w.serving().is_none());
    // Read again: registered again, by a new run.
    w.b.polkit.acquire(&w.rt);
    w.until("registered again", |w| {
        w.calls("RegisterAuthenticationAgent").len() == 2 && w.state().4
    });
    assert!(w.serving().is_some());
    w.services.shutdown();
}

#[test]
fn requests_queue_and_are_dismissed_withdrawn_or_moved_to_another_user() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let helper = fake_helper(dir.path());
    let Some(w) = World::start(PolkitConfig {
        helper: Some(helper),
        socket: Some(dir.path().join("no.socket")),
    }) else {
        return;
    };
    w.b.polkit.acquire(&w.rt);
    w.until("registered", |w| w.state().4);
    let agent = w.agent();
    // Two administrators, this user first among them.
    w.begin(&agent, "c1", &[0, uid()]);
    w.begin(&agent, "c2", &[uid()]);
    w.begin(&agent, "c3", &[uid()]);
    w.until("three, one shown", |w| {
        w.state().1 == 2 && w.request().is_some_and(|r| !r.prompt.is_empty())
    });
    let r = w.request().unwrap();
    assert_eq!(r.users, vec!["root".to_string(), me()]);
    assert_eq!(r.user, me(), "this process's user, when among them");
    assert_eq!(r.info, "", "a prompt clears the text before it");

    // Another user: a new helper, for them.
    w.act("choose", &[text("root")]);
    w.until("asked as root", |w| {
        w.request()
            .is_some_and(|r| r.user == "root" && !r.prompt.is_empty())
    });
    assert_eq!(
        lines(&dir.path().join("helper.log"))
            .last()
            .map(String::as_str),
        Some("root c1")
    );
    // A user polkit did not name is not taken.
    w.act("choose", &[text("nobody-at-all")]);
    w.rt.flush();
    assert_eq!(w.request().unwrap().user, "root");

    // The program withdraws a waiting one.
    w.tokio
        .block_on(w.conn.call_method(
            Some(AUTHORITY),
            AUTHORITY_PATH,
            Some(MOCK_IFACE),
            "Withdraw",
            &(agent.as_str(), "c2"),
        ))
        .unwrap();
    w.until_outcome("c2", "org.freedesktop.PolicyKit1.Error.Cancelled");
    w.until("one waits", |w| w.state().1 == 1);
    // The user dismisses the shown one: the next shows.
    w.act("cancel", &[]);
    w.until_outcome("c1", "org.freedesktop.PolicyKit1.Error.Cancelled");
    w.until("c3 shown", |w| {
        w.state().1 == 0 && w.request().is_some_and(|r| r.users == vec![me()])
    });
    // The program withdraws the shown one: none is left.
    w.tokio
        .block_on(w.conn.call_method(
            Some(AUTHORITY),
            AUTHORITY_PATH,
            Some(MOCK_IFACE),
            "Withdraw",
            &(agent.as_str(), "c3"),
        ))
        .unwrap();
    w.until_outcome("c3", "org.freedesktop.PolicyKit1.Error.Cancelled");
    w.until("none", |w| w.request().is_none());

    // A request waiting when the store stops is refused, not left open.
    w.begin(&agent, "c4", &[uid()]);
    w.until("c4 shown", |w| w.request().is_some());
    w.b.polkit.release(&w.rt);
    w.rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_secs(15);
    while w.outcome("c4").is_empty() {
        w.services.pump(&w.rt);
        w.rt.flush();
        assert!(Instant::now() < deadline, "c4 never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        w.outcome("c4"),
        "org.freedesktop.PolicyKit1.Error.Cancelled"
    );
    w.services.shutdown();
}

#[test]
fn another_agent_is_left_be_until_polkit_says_its_agents_changed() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let helper = fake_helper(dir.path());
    let Some(w) = World::start(PolkitConfig {
        helper: Some(helper),
        socket: Some(dir.path().join("no.socket")),
    }) else {
        return;
    };
    // polkitd's refusal when an agent serves the session already.
    let refuse = "raise dbus.exceptions.DBusException('An authentication agent already exists \
                  for the given subject', name='org.freedesktop.PolicyKit1.Error.Failed')";
    mock(
        &w.tokio,
        &w.conn,
        AUTHORITY,
        AUTHORITY_PATH,
        "AddMethod",
        &(
            AUTHORITY_IFACE,
            "RegisterAuthenticationAgent",
            "(sa{sv})ss",
            "",
            refuse,
        ),
    );
    w.b.polkit.acquire(&w.rt);
    w.until("refused", |w| w.state().5 == HELD);
    assert!(!w.state().4, "not available");
    let warned = w.warnings();
    assert_eq!(warned, vec![HELD.to_string()], "said plainly, once");
    // Nothing more is tried while polkit says nothing changed.
    std::thread::sleep(Duration::from_millis(500));
    w.services.pump(&w.rt);
    assert_eq!(w.calls("RegisterAuthenticationAgent").len(), 1);

    // That agent leaves: polkit's `Changed`, and Strand's registers.
    mock(
        &w.tokio,
        &w.conn,
        AUTHORITY,
        AUTHORITY_PATH,
        "AddMethod",
        &(
            AUTHORITY_IFACE,
            "RegisterAuthenticationAgent",
            "(sa{sv})ss",
            "",
            "",
        ),
    );
    mock(
        &w.tokio,
        &w.conn,
        AUTHORITY,
        AUTHORITY_PATH,
        "EmitSignal",
        &(AUTHORITY_IFACE, "Changed", "", Vec::<OwnedValue>::new()),
    );
    w.until("registered", |w| w.state().4 && w.state().5.is_empty());
    assert_eq!(w.calls("RegisterAuthenticationAgent").len(), 2);
    assert!(w.warnings().is_empty(), "no new warning");
    w.b.polkit.release(&w.rt);
    w.services.shutdown();
}

#[test]
fn the_helpers_socket_comes_first_and_without_a_helper_no_agent_registers() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // A helper socket as polkit 127's: the user's name, then the cookie,
    // then the same lines both ways.
    let socket = dir.path().join("agent-helper.socket");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let served = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut w = s;
        let mut user = String::new();
        let mut cookie = String::new();
        r.read_line(&mut user).unwrap();
        r.read_line(&mut cookie).unwrap();
        w.write_all(b"PAM_PROMPT_ECHO_ON Your\\040name: \n")
            .unwrap();
        let mut answer = String::new();
        r.read_line(&mut answer).unwrap();
        w.write_all(b"SUCCESS\n").unwrap();
        (user, cookie, answer)
    });
    let Some(w) = World::start(PolkitConfig {
        helper: Some(dir.path().join("no-such-helper")),
        socket: Some(socket),
    }) else {
        return;
    };
    w.b.polkit.acquire(&w.rt);
    w.until("registered", |w| w.state().4);
    let agent = w.agent();
    w.begin(&agent, "sock-1", &[uid()]);
    w.until("a visible question", |w| {
        w.request()
            .is_some_and(|r| r.prompt == "Your name: " && r.echo)
    });
    w.act("respond", &[text("me")]);
    w.until_outcome("sock-1", "ok");
    let (user, cookie, answer) = served.join().unwrap();
    assert_eq!(user, format!("{}\n", me()));
    assert_eq!(cookie, "sock-1\n");
    assert_eq!(answer, "me\n");
    w.b.polkit.release(&w.rt);
    w.services.shutdown();

    // No helper at all: Strand does not register, so another agent can.
    let Some(w) = World::start(PolkitConfig {
        helper: Some(dir.path().join("no-such-helper")),
        socket: Some(dir.path().join("no.socket")),
    }) else {
        return;
    };
    w.b.polkit.acquire(&w.rt);
    w.until("says why", |w| {
        w.state().5.contains("polkit-agent-helper-1")
    });
    assert!(!w.state().4);
    assert!(w.calls("RegisterAuthenticationAgent").is_empty());
    w.b.polkit.release(&w.rt);
    w.services.shutdown();
}

//! (services-round) `greetd` against a fake greetd: a socket speaking
//! greetd's protocol (a native-endian `u32` length, then JSON) and
//! keeping its rules (one session under configuration; a question per
//! answer; `start_session` only once the login succeeded; a failed login
//! leaving its dead session until `cancel_session`). The real greetd and
//! PAM run in the lock VM (`tests/vm_auth.rs`).
//!
//! A login shown question by question, texts passed on at once, a
//! refused answer failing the login, cancelling its session and asking
//! the same user again, an
//! answer typed before greetd asked, `start` held until the login
//! succeeds and then ending the greeter, a second login cancelling the
//! first, `cancel`, the actions' refusals, and no `$GREETD_SOCK`.
//!
//! `greetd::configure` is process-wide: the tests take [`LOCK`].

mod support;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use strand_core::Runtime;
use strand_services::greetd::{self, GreetdConfig};
use strand_services::{Builtin, Buses, Data, Services};
use support::*;

static LOCK: Mutex<()> = Mutex::new(());

/// What a user's PAM conversation asks, in order: `(type, text)`.
fn script(user: &str) -> Vec<(&'static str, &'static str)> {
    match user {
        "alice" => vec![("info", "Welcome, alice"), ("secret", "Password: ")],
        "bob" => vec![("visible", "Token: "), ("secret", "Password: ")],
        _ => vec![("secret", "Password: ")],
    }
}

/// The right answer to each question.
fn right(text: &str) -> &'static str {
    match text {
        "Token: " => "123",
        "Password: " => "right",
        _ => "",
    }
}

/// The session under configuration.
struct Configuring {
    user: String,
    step: usize,
    /// PAM refused an answer: the session is dead until cancelled.
    dead: bool,
}

fn error(kind: &str, description: &str) -> Value {
    json!({"type": "error", "error_type": kind, "description": description})
}

/// The question at `c.step`, or success when none is left.
fn question(c: &Configuring) -> Value {
    match script(&c.user).get(c.step) {
        Some((kind, text)) => {
            json!({"type": "auth_message", "auth_message_type": kind, "auth_message": text})
        }
        None => json!({"type": "success"}),
    }
}

fn read_frame(s: &mut UnixStream) -> Option<Value> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let mut body = vec![0u8; u32::from_ne_bytes(len) as usize];
    s.read_exact(&mut body).ok()?;
    Some(serde_json::from_slice(&body).unwrap())
}

fn write_frame(s: &mut UnixStream, v: &Value) {
    let body = serde_json::to_vec(v).unwrap();
    let _ = s.write_all(&(body.len() as u32).to_ne_bytes());
    let _ = s.write_all(&body);
}

/// A fake greetd: every request it reads is recorded in `seen`.
struct FakeGreetd {
    path: PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    _dir: tempfile::TempDir,
}

impl FakeGreetd {
    fn start() -> FakeGreetd {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("greetd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(s) = s else { return };
                let log = log.clone();
                std::thread::spawn(move || serve(s, &log));
            }
        });
        FakeGreetd {
            path,
            seen,
            _dir: dir,
        }
    }

    fn seen(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }

    fn types(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|v| v["type"].as_str().unwrap().to_string())
            .collect()
    }
}

/// greetd's handling of one greeter connection (server.rs and
/// context.rs of greetd 0.9, with PAM replaced by [`script`]).
fn serve(mut s: UnixStream, log: &Mutex<Vec<Value>>) {
    let mut configuring: Option<Configuring> = None;
    while let Some(req) = read_frame(&mut s) {
        log.lock().unwrap().push(req.clone());
        let resp = match req["type"].as_str().unwrap() {
            "create_session" => {
                // Starting PAM takes a moment: the store shows `busy`.
                std::thread::sleep(Duration::from_millis(100));
                if configuring.is_some() {
                    error("error", "a session is already being configured")
                } else {
                    let c = Configuring {
                        user: req["username"].as_str().unwrap().to_string(),
                        step: 0,
                        dead: false,
                    };
                    let q = question(&c);
                    configuring = Some(c);
                    q
                }
            }
            "post_auth_message_response" => match &mut configuring {
                None => error("error", "no session under configuration"),
                Some(c) if c.dead => error("error", "session worker is gone"),
                Some(c) => {
                    let (kind, text) = script(&c.user)[c.step];
                    let ok = match kind {
                        "info" | "error" => req["response"].is_null(),
                        _ => req["response"].as_str() == Some(right(text)),
                    };
                    if ok {
                        c.step += 1;
                        question(c)
                    } else {
                        // PAM's delay after a failure.
                        std::thread::sleep(Duration::from_millis(100));
                        c.dead = true;
                        error("auth_error", "pam_authenticate: AUTH_ERR")
                    }
                }
            },
            "start_session" => match configuring.take() {
                None => error("error", "no session active"),
                Some(c) if c.step < script(&c.user).len() || c.dead => {
                    error("error", "session is not ready")
                }
                Some(_) => json!({"type": "success"}),
            },
            "cancel_session" => {
                configuring = None;
                json!({"type": "success"})
            }
            other => panic!("greetd has no request {other}"),
        };
        write_frame(&mut s, &resp);
    }
}

struct World {
    rt: Runtime,
    services: Services,
    b: Builtin,
    started: Arc<AtomicUsize>,
}

impl World {
    fn start(socket: Option<PathBuf>) -> World {
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        greetd::configure(Some(GreetdConfig {
            socket,
            started: Some(Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })),
        }));
        let rt = Runtime::new();
        let (services, b) = services(&rt, Buses::none());
        b.greetd.acquire(&rt);
        World {
            rt,
            services,
            b,
            started,
        }
    }

    fn until(&self, what: &str, cond: impl Fn(&World) -> bool) {
        until(&self.rt, &self.services, what, || cond(self));
    }

    fn state(&self) -> Greeter {
        let c = self.b.greetd.cells();
        let rt = &self.rt;
        Greeter {
            available: c.available.get_untracked(rt).unwrap(),
            user: c.user.get_untracked(rt).unwrap(),
            prompt: c.prompt.get_untracked(rt).unwrap(),
            echo: c.echo.get_untracked(rt).unwrap(),
            message: c.message.get_untracked(rt).unwrap(),
            busy: c.busy.get_untracked(rt).unwrap(),
            failed: c.failed.get_untracked(rt).unwrap(),
            error: c.error.get_untracked(rt).unwrap(),
        }
    }

    fn act(&self, name: &str, args: &[Data]) {
        self.b
            .greetd
            .dynamic()
            .action(&self.rt, name, None, args, Default::default())
            .unwrap();
        self.rt.flush();
    }

    /// Pumps for a while, for what must not happen.
    fn settle(&self) {
        for _ in 0..40 {
            self.services.pump(&self.rt);
            self.rt.flush();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct Greeter {
    available: bool,
    user: String,
    prompt: String,
    echo: bool,
    message: String,
    busy: bool,
    failed: bool,
    error: String,
}

fn text(s: &str) -> Data {
    Data::Text(s.into())
}

fn command(args: &[&str]) -> Data {
    Data::List(args.iter().map(|a| text(a)).collect())
}

#[test]
fn a_login_is_asked_question_by_question_and_its_session_started() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let g = FakeGreetd::start();
    let w = World::start(Some(g.path.clone()));
    w.until("connected", |w| w.state().available);
    assert_eq!(
        w.state(),
        Greeter {
            available: true,
            ..Greeter::default()
        }
    );

    // The actions' refusals: nothing to start yet, an empty command.
    w.act("start", &[command(&["sway"])]);
    w.until("start refused", |w| w.state().error.contains("login"));
    w.act("start", &[command(&[])]);
    w.until("empty refused", |w| {
        w.state().error == "a session needs a command"
    });
    // An answer with nothing asking is dropped.
    w.act("submit", &[text("stray")]);
    w.settle();
    assert!(g.seen().is_empty());

    // alice: a text passed on at once, then the password, refused.
    w.act("login", &[text("alice")]);
    w.until("logging in", |w| {
        w.state().busy && w.state().user == "alice"
    });
    w.until("asked", |w| w.state().prompt == "Password: ");
    let s = w.state();
    assert_eq!(
        (
            s.message.as_str(),
            s.echo,
            s.busy,
            s.failed,
            s.error.as_str()
        ),
        ("Welcome, alice", false, false, false, "")
    );
    w.act("submit", &[text("wrong")]);
    w.until("checking", |w| {
        w.state().busy && w.state().prompt.is_empty()
    });
    w.until("refused", |w| w.state().failed);
    let s = w.state();
    assert_eq!(s.error, "pam_authenticate: AUTH_ERR");
    assert!(s.prompt.is_empty());
    // The dead session is cancelled and alice asked again, `failed`
    // staying until she answers.
    w.until("asked again", |w| w.state().prompt == "Password: ");
    let s = w.state();
    assert!(s.failed && !s.busy && s.user == "alice", "{s:?}");

    // `start` sent before the login succeeded waits for it.
    w.act("start", &[command(&["sway", "--config", "/etc/my sway"])]);
    w.settle();
    assert_eq!(
        g.types().last().map(String::as_str),
        Some("post_auth_message_response")
    );
    assert_eq!(w.started.load(Ordering::SeqCst), 0);
    w.act("submit", &[text("right")]);
    w.until("answered", |w| {
        !w.state().failed && w.state().error.is_empty()
    });
    w.until("started", |w| w.started.load(Ordering::SeqCst) == 1);
    let s = w.state();
    assert!(!s.failed && s.error.is_empty() && s.prompt.is_empty());
    assert_eq!(
        g.seen(),
        vec![
            json!({"type": "create_session", "username": "alice"}),
            json!({"type": "post_auth_message_response", "response": null}),
            json!({"type": "post_auth_message_response", "response": "wrong"}),
            json!({"type": "cancel_session"}),
            json!({"type": "create_session", "username": "alice"}),
            json!({"type": "post_auth_message_response", "response": null}),
            json!({"type": "post_auth_message_response", "response": "right"}),
            // Each word quoted for the shell line greetd makes of them.
            json!({"type": "start_session", "cmd": ["sway", "--config", "'/etc/my sway'"], "env": []}),
        ]
    );
    w.services.shutdown();
}

#[test]
fn answers_typed_early_logins_replaced_and_cancelled() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let g = FakeGreetd::start();
    let w = World::start(Some(g.path.clone()));
    w.until("connected", |w| w.state().available);

    // bob types his token before greetd asks for it: it answers the
    // first question, and the next one is shown.
    w.act("login", &[text("bob")]);
    w.act("submit", &[text("123")]);
    w.until("asked the password", |w| w.state().prompt == "Password: ");
    assert!(!w.state().echo);
    assert_eq!(
        g.seen()[1],
        json!({"type": "post_auth_message_response", "response": "123"})
    );

    // A visible question shows as one.
    w.act("login", &[text("bob")]);
    w.until("asked the token", |w| w.state().prompt == "Token: ");
    assert!(w.state().echo);
    // The login under way was cancelled before the new one.
    let types = g.types();
    assert_eq!(
        types[2..],
        ["cancel_session", "create_session"].map(String::from)
    );

    // Another user while a login is in flight: the first is cancelled.
    w.act("login", &[text("carol")]);
    w.act("login", &[text("alice")]);
    w.until("alice asked", |w| {
        w.state().prompt == "Password: " && w.state().user == "alice"
    });
    assert_eq!(w.state().message, "Welcome, alice");
    let seen = g.seen();
    let creates: Vec<&Value> = seen
        .iter()
        .filter(|v| v["type"] == "create_session")
        .collect();
    assert_eq!(creates.last().unwrap()["username"], "alice");
    // greetd never saw two sessions under configuration at once.
    let mut open = false;
    for v in &seen {
        match v["type"].as_str().unwrap() {
            "create_session" => {
                assert!(!open, "a second create_session without a cancel: {seen:?}");
                open = true;
            }
            "cancel_session" => open = false,
            _ => {}
        }
    }

    // cancel: greetd drops the session, the store its login.
    w.act("cancel", &[]);
    w.until("cancelled", |_| {
        g.types().last().map(String::as_str) == Some("cancel_session")
    });
    let s = w.state();
    assert_eq!(
        s,
        Greeter {
            available: true,
            ..Greeter::default()
        }
    );
    // A `start` after a cancel has no login to wait for.
    w.act("start", &[command(&["sway"])]);
    w.until("start refused", |w| w.state().error.contains("login"));
    assert_eq!(w.started.load(Ordering::SeqCst), 0);
    w.services.shutdown();
}

#[test]
fn without_greetd_every_action_says_why() {
    let _l = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert!(std::env::var_os(greetd::SOCKET_VAR).is_none());
    let w = World::start(None);
    w.settle();
    assert_eq!(w.state(), Greeter::default());
    w.act("login", &[text("alice")]);
    w.until("said why", |w| w.state().error.contains("GREETD_SOCK"));
    assert!(!w.state().available);
    w.services.shutdown();
}

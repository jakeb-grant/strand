//! The niri adapter against a fake niri: one Unix socket that replays
//! niri 26.04's traffic as reconstructed from its source
//! (`tests/fixtures/niri-26.04`). Like niri before
//! 25.05 (`src/ipc/server.rs`, `handle_client`), the fake answers one
//! request per connection and then closes it, so a client that sends two
//! on one connection fails here.
#![cfg(feature = "niri")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Collector, bursts, fixture};
use strand_services::wm::{self, Backend, WmAction, WmConfig, WmError, WmRequest};
use strand_watch::{ChangeEvent, CompositorEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tokio::sync::mpsc::unbounded_channel;

#[derive(Clone, Debug)]
enum Ev {
    Burst(String),
    Drop,
}

struct FakeNiri {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    events: broadcast::Sender<Ev>,
    /// Replies that replace the fixture's, by request line (a niri that
    /// changed, or a broken one).
    overrides: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeNiri {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("niri.wayland-1.4242.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let overrides: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let (events, _) = broadcast::channel(64);
        let start = bursts("niri-26.04/events.txt")["stream-start"].clone();
        {
            let requests = requests.clone();
            let events = events.clone();
            let overrides = overrides.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        return;
                    };
                    let requests = requests.clone();
                    let mut sub = events.subscribe();
                    let overrides = overrides.clone();
                    let start = start.clone();
                    tokio::spawn(async move {
                        let (r, mut w) = conn.into_split();
                        let mut lines = BufReader::new(r).lines();
                        if let Ok(Some(req)) = lines.next_line().await {
                            requests.lock().unwrap().push(req.clone());
                            let file = |n: &str| {
                                std::fs::read_to_string(fixture("niri-26.04").join(n)).unwrap()
                            };
                            let overridden = overrides
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|(p, _)| req.starts_with(p.as_str()))
                                .map(|(_, r)| r.clone());
                            let reply = match req.as_str() {
                                _ if overridden.is_some() => overridden.unwrap_or_default(),
                                "\"EventStream\"" => {
                                    // The reply, the replicated state, then
                                    // events until told to drop.
                                    if w.write_all(start.as_bytes()).await.is_err() {
                                        return;
                                    }
                                    loop {
                                        match sub.recv().await {
                                            Ok(Ev::Burst(b)) => {
                                                if w.write_all(b.as_bytes()).await.is_err() {
                                                    return;
                                                }
                                            }
                                            Ok(Ev::Drop) | Err(_) => return,
                                        }
                                    }
                                }
                                "\"Workspaces\"" => file("reply-workspaces.json"),
                                "\"Windows\"" => file("reply-windows.json"),
                                "\"FocusedOutput\"" => file("reply-focused-output.json"),
                                "\"Version\"" => {
                                    "{\"Ok\":{\"Version\":\"26.04 (8ed0da4)\"}}\n".into()
                                }
                                r if r.starts_with("{\"Action\"") => {
                                    "{\"Ok\":\"Handled\"}\n".into()
                                }
                                _ => "{\"Err\":\"error parsing request\"}\n".into(),
                            };
                            // One request per connection: reply, then
                            // close (dropping the halves).
                            let _ = w.write_all(reply.as_bytes()).await;
                        }
                    });
                }
            });
        }
        Self {
            _dir: dir,
            socket,
            requests,
            events,
            overrides,
        }
    }

    fn send(&self, burst: &str) {
        self.events.send(Ev::Burst(burst.to_string())).unwrap();
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Answers request lines starting with `req` (`"Windows"`, `{"Action"`) with
    /// `reply` (a line) from now on; `None`: the fixture's again.
    fn set_reply(&self, req: &str, reply: Option<&str>) {
        let mut o = self.overrides.lock().unwrap();
        o.retain(|(p, _)| p != req);
        if let Some(r) = reply {
            o.push((req.to_string(), r.to_string()));
        }
    }
}

#[tokio::test]
async fn niri_adapter_follows_replayed_traffic() {
    let fake = FakeNiri::start();
    let bursts = bursts("niri-26.04/events.txt");
    let (sink, mut c) = Collector::new();
    let (events, changes) = strand_watch::channel();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(Backend::Niri {
            socket: fake.socket.clone(),
        }),
        wayland: None,
        events: Some(events),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));

    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    let m = &c.mirror;
    assert_eq!(m.name, "niri");
    assert_eq!(
        m.workspace_names(),
        ["1", "chat", "3", "1"],
        "per output, by index; unnamed ones by index"
    );
    let first = m.workspace("chat").unwrap();
    assert_eq!(first.id, 2);
    assert!(first.occupied && !first.focused);
    assert!(m.workspaces[0].1.focused && m.workspaces[0].1.active);
    assert_eq!(m.workspaces[3].1.screen, "HDMI-A-1");
    assert!(m.workspaces[3].1.active && !m.workspaces[3].1.focused);
    assert_eq!(m.focused_window.as_ref().unwrap().app_id, "Alacritty");
    assert_eq!(m.focused_window.as_ref().unwrap().id, "10");
    assert_eq!(m.focused_screen.as_deref(), Some("DP-1"));
    assert!(
        m.reloads.is_empty(),
        "the stream's first ConfigLoaded is no reload"
    );
    let reqs = fake.requests();
    assert_eq!(
        reqs,
        [
            "\"EventStream\"",
            "\"Workspaces\"",
            "\"Windows\"",
            "\"FocusedOutput\""
        ]
    );

    fake.send(&bursts["focus"]);
    c.until("focus", |m| {
        m.focused_workspace
            .as_ref()
            .is_some_and(|w| w.name == "chat")
            && m.focused_window
                .as_ref()
                .is_some_and(|w| w.app_id == "signal")
    })
    .await;
    assert!(
        !c.mirror.workspaces[0].1.active,
        "deactivated on its output"
    );
    assert!(
        c.mirror.workspaces[3].1.active,
        "the other output keeps its own"
    );

    fake.send(&bursts["open"]);
    c.until("open", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.app_id == "firefox")
    })
    .await;
    assert_eq!(c.mirror.workspace("chat").unwrap().windows.len(), 2);
    assert!(!c.mirror.window_by_app("signal").unwrap().focused);

    fake.send(&bursts["urgent"]);
    c.until("urgent", |m| m.workspaces[0].1.urgent).await;
    assert!(c.mirror.window_by_app("Alacritty").unwrap().urgent);

    fake.send(&bursts["reorder"]);
    c.until("reorder", |m| m.workspaces.len() == 5).await;
    assert_eq!(
        c.mirror
            .workspaces
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [1, 2, 5, 3, 4]
    );

    fake.send(&bursts["unknown"]);
    assert_eq!(c.quiet_for(Duration::from_millis(150)).await, 0);

    fake.send(&bursts["reload-ok"]);
    c.until("reload ok", |m| m.reloads == [Some(false)]).await;
    fake.send(&bursts["reload-failed"]);
    c.until("reload failed", |m| m.reloads == [Some(false), Some(true)])
        .await;
    let got = tokio::task::spawn_blocking(move || {
        [
            changes.recv_timeout(Duration::from_secs(5)).unwrap(),
            changes.recv_timeout(Duration::from_secs(5)).unwrap(),
        ]
    })
    .await
    .unwrap();
    assert_eq!(
        got,
        [
            ChangeEvent::Compositor(CompositorEvent::ConfigReloaded {
                failed: Some(false)
            }),
            ChangeEvent::Compositor(CompositorEvent::ConfigReloaded { failed: Some(true) })
        ]
    );

    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(5));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let (r, done) = WmRequest::new(WmAction::CloseWindow("12".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    // `win.maximize()`, `win.fullscreen()`: niri's toggles by window id.
    for action in [
        WmAction::MaximizeWindow("12".into()),
        WmAction::FullscreenWindow("12".into()),
    ] {
        let (r, done) = WmRequest::new(action);
        req_tx.send(r).unwrap();
        assert_eq!(done.await, Ok(()));
    }
    let (r, done) = WmRequest::new(WmAction::FocusWindow("99".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::UnknownWindow("99".into())));
    let reqs = fake.requests();
    assert!(
        reqs.contains(&r#"{"Action":{"FocusWorkspace":{"reference":{"Id":5}}}}"#.to_string()),
        "{reqs:?}"
    );
    assert!(reqs.contains(&r#"{"Action":{"CloseWindow":{"id":12}}}"#.to_string()));
    assert!(reqs.contains(&r#"{"Action":{"MaximizeWindowToEdges":{"id":12}}}"#.to_string()));
    assert!(reqs.contains(&r#"{"Action":{"FullscreenWindow":{"id":12}}}"#.to_string()));

    fake.send(&bursts["close"]);
    c.until("close", |m| m.window_by_app("firefox").is_none())
        .await;
    assert_eq!(c.mirror.focused_window.as_ref().unwrap().app_id, "signal");

    let before = fake.requests().len();
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);
    assert_eq!(fake.requests().len(), before, "no polling");
    service.abort();
}

#[tokio::test]
async fn niri_adapter_reconnects_without_a_spurious_reload() {
    let fake = FakeNiri::start();
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(Backend::Niri {
            socket: fake.socket.clone(),
        }),
        wayland: None,
        events: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.events.send(Ev::Drop).unwrap();
    c.until("lost", |m| !m.sources.connected).await;
    c.until("back", |m| m.sources.connected).await;
    assert_eq!(c.quiet_for(Duration::from_millis(200)).await, 0);
    assert!(
        c.mirror.reloads.is_empty(),
        "a new stream's ConfigLoaded is not a reload"
    );
    assert_eq!(
        fake.requests()
            .iter()
            .filter(|r| *r == "\"EventStream\"")
            .count(),
        2
    );
    service.abort();
}

/// The `workspaces` and `wm` stores over the niri adapter: what shells
/// read (the focused workspace, a switch, an action as a niri request)
/// and `wm.config_reloaded(failed)` with niri's answer.
#[test]
fn the_stores_follow_niri() {
    use std::cell::RefCell;
    use std::rc::Rc;
    use strand_core::Runtime;
    use strand_services::wm::WorkspaceAction;
    use strand_services::{Applied, Builtin, Buses, Cells, Data, Services};

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _enter = tokio.enter();
    let fake = FakeNiri::start();
    let bursts = bursts("niri-26.04/events.txt");
    // The only test of this binary that sets the process-wide config.
    wm::configure(Some(WmConfig {
        backend: Some(Backend::Niri {
            socket: fake.socket.clone(),
        }),
        wayland: None,
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let reloads: Rc<RefCell<Vec<Vec<Data>>>> = Rc::default();
    let r = reloads.clone();
    b.wm.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { args, .. } = a {
            r.borrow_mut().push(args.clone());
        }
    }));
    let until = |what: &str, cond: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            s.pump(&rt);
            rt.flush();
            if cond() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let focused = || {
        b.workspaces
            .cells()
            .snapshot(&rt)
            .ok()
            .and_then(|w| w.focused)
            .map(|w| w.name)
    };
    b.workspaces.acquire(&rt);
    b.wm.acquire(&rt);
    until("boot", &|| {
        focused().as_deref() == Some("1")
            && b.wm.cells().snapshot(&rt).is_ok_and(|w| w.name == "niri")
    });
    fake.send(&bursts["focus"]);
    until("focus", &|| focused().as_deref() == Some("chat"));
    let three = b
        .workspaces
        .cells()
        .snapshot(&rt)
        .unwrap()
        .all
        .into_iter()
        .find(|w| w.name == "3")
        .unwrap();
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item: three })
        .unwrap();
    until("the action", &|| {
        fake.requests()
            .iter()
            .any(|r| r.starts_with("{\"Action\"") && r.contains("FocusWorkspace"))
    });
    fake.send(&bursts["reload-failed"]);
    until("the failed reload", &|| {
        reloads.borrow().as_slice() == [vec![Data::Bool(true)]]
    });
    s.shutdown();
    wm::configure(None);
}

fn niri_service(
    fake: &FakeNiri,
) -> (
    WmConfig,
    Collector,
    impl FnMut(Vec<wm::WmChange>) + Send + 'static,
) {
    let (sink, c) = Collector::new();
    let config = WmConfig {
        backend: Some(Backend::Niri {
            socket: fake.socket.clone(),
        }),
        wayland: None,
        events: None,
        ..Default::default()
    };
    (config, c, sink)
}

fn degraded(m: &wm::Mirror) -> Option<&str> {
    m.sources.degraded.as_deref()
}

/// A niri whose reply has another shape (windows without their `id`):
/// an event the adapter knows but cannot read asks for a re-read, the
/// re-read's reply is not understood, and the adapter is degraded: the
/// protocols serve (here no display: nothing), the reason names niri's
/// version. Once niri answers as expected again, its state is back.
#[tokio::test]
async fn a_niri_reply_of_another_shape_degrades_until_it_is_read_again() {
    let fake = FakeNiri::start();
    let (config, mut c, sink) = niri_service(&fake);
    let (_req_tx, req_rx) = unbounded_channel();
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.set_reply(
        "\"Windows\"",
        Some("{\"Ok\":{\"Windows\":[{\"title\":\"no id\"}]}}\n"),
    );
    // A known event whose data has changed shape: read again.
    fake.send("{\"WindowsChanged\":{\"list\":[]}}\n");
    c.until("degraded", |m| degraded(m).is_some()).await;
    let why = degraded(&c.mirror).unwrap().to_string();
    assert!(why.contains("niri 26.04 (8ed0da4)"), "{why}");
    assert!(
        why.contains("the reply to Windows") && why.contains("missing field `id`"),
        "{why}"
    );
    assert!(!c.mirror.sources.connected);
    assert!(c.mirror.workspaces.is_empty() && c.mirror.windows.is_empty());
    assert_eq!(c.mirror.name, "niri");

    fake.set_reply("\"Windows\"", None);
    c.until_within(15, "recovered", |m| {
        m.sources.connected && degraded(m).is_none() && !m.workspaces.is_empty()
    })
    .await;
    assert!(!c.mirror.windows.is_empty());
    service.abort();
}

/// An event niri added that this adapter does not know changes nothing
/// and costs nothing; a known one whose data has another shape costs one
/// read; lines that are no JSON at all are re-read too, and only eight in
/// a row degrade the adapter.
#[tokio::test]
async fn an_unknown_niri_event_is_harmless_but_a_stream_of_no_events_is_not() {
    let fake = FakeNiri::start();
    let (config, mut c, sink) = niri_service(&fake);
    let (_req_tx, req_rx) = unbounded_channel();
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    let before = fake.requests().len();
    fake.send("{\"BrandNewEvent\":{\"id\":1}}\n");
    assert_eq!(c.quiet_for(Duration::from_millis(200)).await, 0);
    assert_eq!(fake.requests().len(), before, "no read");

    fake.send("{\"WindowFocusChanged\":{\"window\":3}}\n");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.requests().len() < before + 3 {
        assert!(tokio::time::Instant::now() < deadline, "no re-read");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    c.quiet_for(Duration::from_millis(200)).await;
    assert_eq!(degraded(&c.mirror), None);
    assert!(c.mirror.sources.connected);

    fake.send(&format!(
        "{}{{\"BrandNewEvent\":{{}}}}\n",
        "not json\n".repeat(7)
    ));
    c.quiet_for(Duration::from_millis(300)).await;
    assert_eq!(degraded(&c.mirror), None, "seven, then an event");
    // From now on every event stream is lines that are no JSON.
    fake.set_reply(
        "\"EventStream\"",
        Some(&format!(
            "{{\"Ok\":\"Handled\"}}\n{}",
            "not json\n".repeat(8)
        )),
    );
    fake.send(&"not json\n".repeat(8));
    c.until("degraded", |m| degraded(m).is_some()).await;
    let why = degraded(&c.mirror).unwrap().to_string();
    assert!(
        why.contains("event stream") && why.contains("niri 26.04"),
        "{why}"
    );
    assert_still_degraded(&fake, &mut c, &why).await;

    // A niri whose stream carries events again comes back up.
    fake.set_reply("\"EventStream\"", None);
    c.until_within(15, "recovered", |m| {
        m.sources.connected && degraded(m).is_none() && !m.workspaces.is_empty()
    })
    .await;
    service.abort();
}

/// Retries of a niri that is still not understood: each reads the state
/// (the replies are fine) and its event stream, but none comes up, so the
/// services never flip back to niri's ids and its reason is raised once.
async fn assert_still_degraded(fake: &FakeNiri, c: &mut Collector, why: &str) {
    let streams = |fake: &FakeNiri| {
        fake.requests()
            .iter()
            .filter(|r| *r == "\"EventStream\"")
            .count()
    };
    let before = streams(fake);
    let up = c.log.len();
    c.quiet_for(Duration::from_millis(1500)).await;
    assert!(streams(fake) >= before + 2, "retried");
    assert_eq!(degraded(&c.mirror), Some(why));
    assert!(c.mirror.workspaces.is_empty() && c.mirror.windows.is_empty());
    for ch in &c.log[up..] {
        match ch {
            wm::WmChange::Sources(s) => {
                assert!(!s.connected, "no flip back to niri's state: {s:?}");
                assert_eq!(s.degraded.as_deref(), Some(why), "raised once");
            }
            other => panic!("the retries changed something: {other:?}"),
        }
    }
}

/// niri 25.08 has no `MaximizeWindowToEdges` and answers it as it answers
/// anything it cannot parse (`error parsing request`): that one action is
/// rejected, naming it, and the adapter stays, its other actions working.
#[tokio::test]
async fn a_niri_without_a_later_action_rejects_only_that_action() {
    let fake = FakeNiri::start();
    let (config, mut c, sink) = niri_service(&fake);
    let (req_tx, req_rx) = unbounded_channel();
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.windows.is_empty())
        .await;
    fake.set_reply(
        "{\"Action\":{\"MaximizeWindowToEdges\"",
        Some("{\"Err\":\"error parsing request\"}\n"),
    );
    let id = c.mirror.windows[0].1.id.clone();
    let (r, done) = WmRequest::new(WmAction::MaximizeWindow(id.clone()));
    req_tx.send(r).unwrap();
    match done.await {
        Err(WmError::Rejected(m)) => assert!(
            m.contains("error parsing request") && m.contains("MaximizeWindowToEdges"),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
    let up = c.log.len();
    c.quiet_for(Duration::from_millis(500)).await;
    assert_eq!(degraded(&c.mirror), None);
    assert!(c.mirror.sources.connected);
    assert!(
        !c.log[up..]
            .iter()
            .any(|ch| matches!(ch, wm::WmChange::Sources(_))),
        "the adapter stays"
    );
    let (r, done) = WmRequest::new(WmAction::FocusWindow(id));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    service.abort();
}

/// A niri that cannot parse the adapter's actions (`error parsing
/// request`, its answer to a request it does not know) refuses the one
/// syntax it has: the adapter degrades, and stays so for that version.
#[tokio::test]
async fn a_niri_refusing_the_action_syntax_degrades_until_its_version_changes() {
    let fake = FakeNiri::start();
    let (config, mut c, sink) = niri_service(&fake);
    let (req_tx, req_rx) = unbounded_channel();
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.set_reply("{\"Action\"", Some("{\"Err\":\"error parsing request\"}\n"));
    let id = c.mirror.workspaces[0].0;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(id));
    req_tx.send(r).unwrap();
    assert_eq!(
        done.await,
        Err(WmError::Rejected("error parsing request".into()))
    );
    c.until("degraded", |m| degraded(m).is_some()).await;
    assert!(degraded(&c.mirror).unwrap().contains("action syntax"));
    let up = c.log.len();
    c.quiet_for(Duration::from_millis(1000)).await;
    assert!(degraded(&c.mirror).is_some());
    assert!(
        !c.log[up..]
            .iter()
            .any(|ch| matches!(ch, wm::WmChange::Sources(s) if s.connected)),
        "the same niri stays degraded"
    );
    fake.set_reply("{\"Action\"", None);
    fake.set_reply(
        "\"Version\"",
        Some("{\"Ok\":{\"Version\":\"26.10 (0000000)\"}}\n"),
    );
    c.until_within(15, "recovered", |m| {
        m.sources.connected && degraded(m).is_none() && !m.workspaces.is_empty()
    })
    .await;
    service.abort();
}

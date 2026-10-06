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
}

impl FakeNiri {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("niri.wayland-1.4242.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (events, _) = broadcast::channel(64);
        let start = bursts("niri-26.04/events.txt")["stream-start"].clone();
        {
            let requests = requests.clone();
            let events = events.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((conn, _)) = listener.accept().await else {
                        return;
                    };
                    let requests = requests.clone();
                    let mut sub = events.subscribe();
                    let start = start.clone();
                    tokio::spawn(async move {
                        let (r, mut w) = conn.into_split();
                        let mut lines = BufReader::new(r).lines();
                        if let Ok(Some(req)) = lines.next_line().await {
                            requests.lock().unwrap().push(req.clone());
                            let file = |n: &str| {
                                std::fs::read_to_string(fixture("niri-26.04").join(n)).unwrap()
                            };
                            let reply = match req.as_str() {
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
        }
    }

    fn send(&self, burst: &str) {
        self.events.send(Ev::Burst(burst.to_string())).unwrap();
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
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
    assert_eq!(done.await.unwrap(), Ok(()));
    let (r, done) = WmRequest::new(WmAction::CloseWindow("12".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Ok(()));
    let (r, done) = WmRequest::new(WmAction::FocusWindow("99".into()));
    req_tx.send(r).unwrap();
    assert_eq!(
        done.await.unwrap(),
        Err(WmError::UnknownWindow("99".into()))
    );
    let reqs = fake.requests();
    assert!(
        reqs.contains(&r#"{"Action":{"FocusWorkspace":{"reference":{"Id":5}}}}"#.to_string()),
        "{reqs:?}"
    );
    assert!(reqs.contains(&r#"{"Action":{"CloseWindow":{"id":12}}}"#.to_string()));

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

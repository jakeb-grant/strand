//! The Hyprland adapter against a fake Hyprland: two Unix sockets that
//! replay Hyprland 0.56.2's traffic (`tests/fixtures/hyprland-0.56.2`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Collector, bursts, fixture};
use strand_services::wm::{self, Backend, WmAction, WmConfig, WmError, WmRequest};
use strand_watch::{ChangeEvent, CompositorEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

enum Ev {
    Burst(String),
    /// Close the event connection (Hyprland restarting, a lost socket).
    Drop,
}

/// A fake Hyprland instance answering from one fixture directory.
struct FakeHyprland {
    _dir: tempfile::TempDir,
    backend: Backend,
    scene: Arc<Mutex<&'static str>>,
    requests: Arc<Mutex<Vec<String>>>,
    events: UnboundedSender<Ev>,
}

impl FakeHyprland {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::hyprland_in(dir.path(), "a1b2c3_1759780000_123456789");
        let Backend::Hyprland { requests, events } = backend.clone() else {
            unreachable!()
        };
        std::fs::create_dir_all(requests.parent().unwrap()).unwrap();
        let scene = Arc::new(Mutex::new("boot"));
        let log = Arc::new(Mutex::new(Vec::new()));
        let s1 = UnixListener::bind(&requests).unwrap();
        let s2 = UnixListener::bind(&events).unwrap();
        {
            let scene = scene.clone();
            let log = log.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = s1.accept().await else {
                        return;
                    };
                    // Hyprland reads one request of up to 1023 bytes.
                    let mut buf = vec![0u8; 1023];
                    let n = conn.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let reply = answer(*scene.lock().unwrap(), &req);
                    log.lock().unwrap().push(req);
                    let _ = conn.write_all(reply.as_bytes()).await;
                    // Then closes.
                }
            });
        }
        let (tx, mut rx) = unbounded_channel::<Ev>();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = s2.accept().await else {
                    return;
                };
                loop {
                    match rx.recv().await {
                        Some(Ev::Burst(b)) => {
                            // One write: the burst arrives together.
                            if conn.write_all(b.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        Some(Ev::Drop) => break,
                        None => return,
                    }
                }
            }
        });
        Self {
            _dir: dir,
            backend,
            scene,
            requests: log,
            events: tx,
        }
    }

    fn set_scene(&self, s: &'static str) {
        *self.scene.lock().unwrap() = s;
    }

    fn send(&self, burst: &str) {
        self.events.send(Ev::Burst(burst.to_string())).unwrap();
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn answer(scene: &str, req: &str) -> String {
    let dir: PathBuf = fixture("hyprland-0.56.2").join(scene);
    let file = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    match req {
        "j/monitors" => file("monitors.json"),
        "j/workspaces" => file("workspaces.json"),
        "j/clients" => file("clients.json"),
        "j/activewindow" => file("activewindow.json"),
        r if r.starts_with("dispatch ") => "ok".into(),
        _ => "unknown request".into(),
    }
}

fn names(m: &wm::Mirror) -> Vec<String> {
    m.workspace_names()
}

#[tokio::test]
async fn hyprland_adapter_follows_replayed_traffic() {
    let fake = FakeHyprland::start();
    let bursts = bursts("hyprland-0.56.2/events.txt");
    let (sink, mut c) = Collector::new();
    let (events, changes) = strand_watch::channel();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        events: Some(events),
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));

    // Boot: one read of the whole state.
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    let m = &c.mirror;
    assert_eq!(m.name, "Hyprland");
    assert_eq!(names(m), ["1", "2", "3"], "sorted; special:magic left out");
    let ws1 = m.workspace("1").unwrap();
    assert!(ws1.focused && ws1.active && ws1.occupied);
    assert_eq!(ws1.screen, "DP-1");
    assert_eq!(ws1.windows.len(), 1);
    let ws3 = m.workspace("3").unwrap();
    assert!(ws3.active && !ws3.focused && !ws3.occupied);
    assert_eq!(ws3.screen, "HDMI-A-1");
    let kitty = m.window_by_app("kitty").unwrap();
    assert_eq!(kitty.id, "0x55d0c0a1b2c0");
    assert!(kitty.focused);
    assert_eq!(kitty.workspace, Some(1));
    let pavu = m.window_by_app("org.pulseaudio.pavucontrol").unwrap();
    assert!(pavu.minimized && pavu.workspace.is_none());
    assert_eq!(m.focused_screen.as_deref(), Some("DP-1"));
    assert_eq!(m.focused_window.as_ref().unwrap().app_id, "kitty");
    assert_eq!(
        fake.requests(),
        ["j/monitors", "j/workspaces", "j/clients", "j/activewindow"]
    );

    // Switching workspaces patches in place: no request.
    fake.send(&bursts["switch"]);
    c.until("switch", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2") && m.focused_window.is_none()
    })
    .await;
    assert!(!c.mirror.workspace("1").unwrap().active);
    assert_eq!(fake.requests().len(), 4, "patched without a request");

    // A window opens: one re-read for the whole burst.
    fake.set_scene("opened");
    fake.send(&bursts["open"]);
    c.until("open", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.app_id == "foot")
    })
    .await;
    assert_eq!(fake.requests().len(), 8, "one re-read for the burst");
    let ws2 = c.mirror.workspace("2").unwrap();
    assert!(ws2.occupied && ws2.focused);
    assert_eq!(ws2.windows[0].id, "0x55d0c0a1c3d0");

    // Title: patched, titles with commas kept whole.
    fake.send(&bursts["title"]);
    c.until("title", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "~/src, strand")
    })
    .await;
    assert_eq!(fake.requests().len(), 8);

    // Urgency lands on the window and its workspace.
    fake.send(&bursts["urgent"]);
    c.until("urgent", |m| m.workspace("1").is_some_and(|w| w.urgent))
        .await;
    assert!(c.mirror.window_by_app("kitty").unwrap().urgent);

    // Events the services do not show change nothing.
    fake.send(&bursts["layout"]);
    assert_eq!(c.quiet_for(Duration::from_millis(150)).await, 0);

    // configreloaded: wm.config_reloaded (failed unknown) and strand-watch.
    fake.send(&bursts["reload"]);
    c.until("reload", |m| m.reloads == [None]).await;
    let ev = tokio::task::spawn_blocking(move || changes.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ev,
        ChangeEvent::Compositor(CompositorEvent::ConfigReloaded { failed: None })
    );

    // Actions go out as dispatches.
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Ok(()));
    let (r, done) = WmRequest::new(WmAction::CloseWindow("0x55d0c0a1c3d0".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Ok(()));
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow("0x55d0c0a1c3d0".into()));
    req_tx.send(r).unwrap();
    assert!(matches!(done.await.unwrap(), Err(WmError::Unsupported(_))));
    let reqs = fake.requests();
    assert!(
        reqs.contains(&"dispatch workspace 3".to_string()),
        "{reqs:?}"
    );
    assert!(reqs.contains(&"dispatch closewindow address:0x55d0c0a1c3d0".to_string()));

    // A window closes.
    fake.set_scene("closed");
    fake.send(&bursts["close"]);
    c.until("close", |m| m.window_by_app("foot").is_none())
        .await;
    assert!(!c.mirror.workspace("2").unwrap().occupied);

    // Idle: nothing more happens, no request, no batch.
    let before = fake.requests().len();
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);
    assert_eq!(fake.requests().len(), before, "no polling");
    service.abort();
}

#[tokio::test]
async fn hyprland_adapter_reconnects_after_losing_its_socket() {
    let fake = FakeHyprland::start();
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        events: None,
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;

    // The event socket goes away; a window opened meanwhile.
    fake.events.send(Ev::Drop).unwrap();
    fake.set_scene("opened");
    c.until("lost", |m| !m.sources.connected).await;
    // The last state stays while away.
    assert_eq!(c.mirror.workspace_names(), ["1", "2", "3"]);
    c.until("reconnected", |m| m.sources.connected).await;
    c.until("caught up", |m| m.window_by_app("foot").is_some())
        .await;
    // One fresh read on reconnecting, and only the difference went out.
    assert_eq!(fake.requests().len(), 8);
    let resets = c
        .log
        .iter()
        .filter(|ch| {
            matches!(ch, wm::WmChange::Windows(d)
                if d.iter().any(|d| matches!(d, strand_core::keyed::VecDiff::Reset { .. })))
        })
        .count();
    assert_eq!(resets, 1, "only the first publish resets the list");
    service.abort();
}

#[tokio::test]
async fn a_missing_hyprland_is_retried_with_backoff_not_a_busy_loop() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::hyprland_in(dir.path(), "gone");
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(backend),
        wayland: None,
        events: None,
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    // Nothing to publish: no state ever arrived.
    assert_eq!(c.quiet_for(Duration::from_millis(300)).await, 0);
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(1));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Err(WmError::NotConnected));
    service.abort();
    assert!(!Path::new(&dir.path().join("hypr/gone/.socket2.sock")).exists());
}

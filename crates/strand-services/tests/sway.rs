//! The sway adapter and the protocol client against a real headless sway,
//! and the sway adapter against a fake sway serving captured replies.

mod common;

use std::time::Duration;

use common::window::TestWindow;
use common::{Collector, Sway, fixture};
use strand_services::wm::{
    self, Backend, ProtocolClient, ProtocolState, WaylandTarget, WmAction, WmConfig, WmRequest,
};
use tokio::sync::mpsc::unbounded_channel;

fn sway_version() -> (u32, u32) {
    let out = std::process::Command::new("sway")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    // "sway version 1.9"
    let v = out.split_whitespace().nth(2).unwrap_or("0.0");
    let mut it = v.split(['.', '-']).map(|p| p.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[tokio::test]
async fn protocol_client_reports_what_sway_offers() {
    let Some(sway) = Sway::start("protocol_client_reports_what_sway_offers") else {
        return;
    };
    let (tx, mut rx) = unbounded_channel::<ProtocolState>();
    let client = ProtocolClient::spawn(WaylandTarget::Socket(sway.socket()), tx).unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    eprintln!("sway {:?}: {first:?}", sway_version());
    assert!(first.connected);
    let version = sway_version();
    if version == (1, 9) {
        // sway 1.9 (wlroots 0.17) advertises neither: foreign-toplevel-list
        // arrived in 1.10, ext-workspace later still. Windows and
        // workspaces then come from IPC alone.
        assert!(!first.toplevel_list && !first.workspace_manager);
    }
    if first.toplevel_list {
        let _w = TestWindow::open(&sway.socket(), "strand-proto", "hello");
        loop {
            let s = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Some(t) = s.toplevels.iter().find(|t| t.app_id == "strand-proto") {
                assert_eq!(t.title, "hello");
                assert!(!t.identifier.is_empty());
                break;
            }
        }
    }
    drop(client);
    // The thread stops and says so.
    let last = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(s) if !s.connected => return s,
                Some(_) => {}
                None => return ProtocolState::default(),
            }
        }
    })
    .await
    .unwrap();
    assert!(!last.connected);
}

#[tokio::test]
async fn sway_adapter_follows_a_real_sway() {
    let Some(sway) = Sway::start("sway_adapter_follows_a_real_sway") else {
        return;
    };
    let (sink, mut c) = Collector::new();
    let (events, changes) = strand_watch::channel();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(Backend::Sway {
            socket: sway.ipc.clone(),
        }),
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        events: Some(events),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));

    c.until("boot", |m| {
        m.sources.connected && m.focused_workspace.as_ref().is_some_and(|w| w.name == "1")
    })
    .await;
    assert_eq!(c.mirror.name, "sway");
    assert_eq!(c.mirror.focused_screen.as_deref(), Some("HEADLESS-1"));
    let ws1 = c.mirror.workspace("1").unwrap();
    assert!(ws1.active && !ws1.occupied);
    assert_eq!(ws1.screen, "HEADLESS-1");
    eprintln!("sources: {:?}", c.mirror.sources);

    // Switching: sway drops the empty workspace 1.
    sway.msg(&["workspace", "3"]);
    c.until("switch", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "3") && m.workspace("1").is_none()
    })
    .await;

    // A real window opens on 3 and takes the focus.
    let win = TestWindow::open(&sway.socket(), "strand-test", "hello");
    c.until("open", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.app_id == "strand-test")
    })
    .await;
    let w = c.mirror.window_by_app("strand-test").unwrap().clone();
    assert_eq!(w.title, "hello");
    let ws3 = c.mirror.workspace("3").unwrap().clone();
    assert_eq!(w.workspace, Some(ws3.id));
    assert!(ws3.occupied && ws3.windows.len() == 1);

    // A title change is patched from the event.
    win.set_title("hello, world");
    c.until("title", |m| {
        m.window_by_app("strand-test")
            .is_some_and(|w| w.title == "hello, world")
    })
    .await;

    // Another workspace, then back through the adapter.
    sway.msg(&["workspace", "2"]);
    c.until("ws 2", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert!(c.mirror.focused_window.is_none());
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws3.id));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("back to 3", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "3")
    })
    .await;

    // Minimise: into the scratchpad, off its workspace.
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow(w.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("minimised", |m| {
        m.window_by_app("strand-test")
            .is_some_and(|w| w.minimized && w.workspace.is_none())
    })
    .await;
    // Focus brings it back.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(w.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("restored", |m| {
        m.window_by_app("strand-test")
            .is_some_and(|w| !w.minimized && w.focused)
    })
    .await;

    // A second monitor brings its own workspace.
    sway.msg(&["create_output"]);
    c.until("second output", |m| {
        m.workspaces
            .iter()
            .any(|(_, w)| w.screen == "HEADLESS-2" && w.active)
    })
    .await;

    // `swaymsg reload` is wm.config_reloaded.
    sway.msg(&["reload"]);
    c.until("reload", |m| m.reloads == [None]).await;
    let ev = tokio::task::spawn_blocking(move || changes.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ev,
        strand_watch::ChangeEvent::Compositor(strand_watch::CompositorEvent::ConfigReloaded {
            failed: None
        })
    );

    // Close: sway asks the client, which unmaps.
    let (r, done) = WmRequest::new(WmAction::CloseWindow(w.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("closed", |m| m.window_by_app("strand-test").is_none())
        .await;
    assert!(win.closed.load(std::sync::atomic::Ordering::SeqCst));

    // Idle: nothing changes, nothing is sent.
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);

    // sway exits: the adapter notices.
    drop(sway);
    c.until("gone", |m| !m.sources.connected).await;
    drop(win);
    service.abort();
}

/// A fake sway: answers `subscribe`, `get_workspaces`, `get_tree` and
/// `run_command` from tests/fixtures/sway-1.9 (window title in Latin-1),
/// and sends what the test queues as events.
struct FakeSway {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    events: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn frame(ty: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = b"i3-ipc".to_vec();
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(&ty.to_ne_bytes());
    out.extend_from_slice(payload);
    out
}

/// `text` with raw `bytes` where `placeholder` was.
fn with_raw(text: &str, placeholder: &str, bytes: &[u8]) -> Vec<u8> {
    let (head, tail) = text.split_once(placeholder).unwrap();
    let mut out = head.as_bytes().to_vec();
    out.extend_from_slice(bytes);
    out.extend_from_slice(tail.as_bytes());
    out
}

impl FakeSway {
    fn start(title: &'static [u8]) -> Self {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sway-ipc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (tx, rx) = unbounded_channel::<Vec<u8>>();
        let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = connections.clone();
        let tree = std::fs::read_to_string(fixture("sway-1.9/tree.json")).unwrap();
        let tree = with_raw(&tree, "TITLE_HERE", title);
        let workspaces = std::fs::read(fixture("sway-1.9/workspaces.json")).unwrap();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let (mut r, w) = conn.into_split();
                let w = std::sync::Arc::new(tokio::sync::Mutex::new(w));
                let (tree, workspaces, rx) = (tree.clone(), workspaces.clone(), rx.clone());
                tokio::spawn(async move {
                    loop {
                        let mut header = [0u8; 14];
                        if r.read_exact(&mut header).await.is_err() {
                            return;
                        }
                        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap());
                        let ty = u32::from_ne_bytes(header[10..14].try_into().unwrap());
                        let mut payload = vec![0u8; len as usize];
                        r.read_exact(&mut payload).await.unwrap();
                        let reply: Vec<u8> = match ty {
                            0 => br#"[{"success": true}]"#.to_vec(),
                            1 => workspaces.clone(),
                            4 => tree.clone(),
                            2 => {
                                // The subscriber: reply, then forward events.
                                let w = w.clone();
                                let rx = rx.clone();
                                w.lock()
                                    .await
                                    .write_all(&frame(2, br#"{"success": true}"#))
                                    .await
                                    .unwrap();
                                tokio::spawn(async move {
                                    while let Some(ev) = rx.lock().await.recv().await {
                                        if w.lock().await.write_all(&ev).await.is_err() {
                                            return;
                                        }
                                    }
                                });
                                continue;
                            }
                            _ => br#"{"success": false}"#.to_vec(),
                        };
                        if w.lock().await.write_all(&frame(ty, &reply)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            _dir: dir,
            socket,
            events: tx,
            connections,
        }
    }

    /// A `window` `title` event for the fixture's window, its new title in
    /// raw bytes.
    fn retitle(&self, title: &[u8]) {
        let tree: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(fixture("sway-1.9/tree.json")).unwrap())
                .unwrap();
        fn find(n: &serde_json::Value) -> Option<serde_json::Value> {
            if n["name"] == "TITLE_HERE" {
                return Some(n.clone());
            }
            n["nodes"].as_array()?.iter().find_map(find)
        }
        let mut node = find(&tree).unwrap();
        node["name"] = "NEW_TITLE".into();
        let event = serde_json::json!({ "change": "title", "container": node }).to_string();
        let payload = with_raw(&event, "NEW_TITLE", title);
        self.events.send(frame(0x8000_0003, &payload)).unwrap();
    }
}

/// A title that is not UTF-8 (an XWayland Latin-1 `WM_NAME`, which sway
/// passes through raw) in `get_tree` and in a `window` event shows U+FFFD
/// and costs nothing: no reconnect, the state follows, actions run.
#[tokio::test]
async fn sway_titles_that_are_not_utf8_keep_the_connection() {
    let fake = FakeSway::start(b"caf\xe9");
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(Backend::Sway {
            socket: fake.socket.clone(),
        }),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.sources.connected
            && m.window_by_app("foot")
                .is_some_and(|w| w.title == "caf\u{fffd}")
    })
    .await;
    assert_eq!(c.mirror.workspace_names(), ["1"]);
    fake.retitle(b"na\xefve");
    c.until("retitled", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "na\u{fffd}ve")
    })
    .await;
    let id = c.mirror.window_by_app("foot").unwrap().id.clone();
    let (r, done) = WmRequest::new(WmAction::FocusWindow(id));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let up = c
        .log
        .iter()
        .position(|ch| matches!(ch, wm::WmChange::Sources(s) if s.connected))
        .unwrap();
    assert!(
        !c.log[up..]
            .iter()
            .any(|ch| matches!(ch, wm::WmChange::Sources(s) if !s.connected)),
        "never disconnected once up"
    );
    assert_eq!(
        fake.connections.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one event and one request connection: no reconnect"
    );
    service.abort();
}

/// sway's IPC adapter turned off: the standard protocols alone, as on a
/// compositor without an adapter (labwc, wayfire, river). sway offers
/// `zwlr_foreign_toplevel_management_v1` (1.9 has no
/// `ext-foreign-toplevel-list-v1` or `ext-workspace-v1`), so windows,
/// `windows.focused`, the fullscreen state and the window actions come
/// from it.
#[tokio::test]
async fn wlr_management_serves_sway_without_its_adapter() {
    let Some(sway) = Sway::start("wlr_management_serves_sway_without_its_adapter") else {
        return;
    };
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        desktop: Some("sway".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.toplevel_management).await;
    assert_eq!(c.mirror.sources.ipc, None);
    assert!(c.mirror.windows.is_empty() && c.mirror.focused_window.is_none());

    // Two windows: the newer one takes the focus.
    let a = TestWindow::open(&sway.socket(), "strand-a", "alpha");
    c.until("a focused", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.app_id == "strand-a")
    })
    .await;
    let b = TestWindow::open(&sway.socket(), "strand-b", "beta");
    c.until("b focused", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.app_id == "strand-b")
            && m.windows.len() == 2
    })
    .await;
    let wa = c.mirror.window_by_app("strand-a").unwrap().clone();
    let wb = c.mirror.window_by_app("strand-b").unwrap().clone();
    assert!(
        wa.id.starts_with("wlr-") && wb.id.starts_with("wlr-"),
        "{wa:?}"
    );
    assert!(!wa.focused && wb.focused);
    assert_eq!(wa.title, "alpha");
    assert_eq!(wa.workspace, None, "the protocols place no window");
    // The focused window's output is the keyboard's screen (its
    // `output_enter` may come in a later `done`).
    c.until("the keyboard's screen", |m| {
        m.focused_screen.as_deref() == Some("HEADLESS-1")
    })
    .await;

    // Focus changed from outside follows.
    sway.msg(&["[app_id=strand-a]", "focus"]);
    c.until("a focused again", |m| {
        m.focused_window.as_ref().is_some_and(|w| w.id == wa.id)
    })
    .await;
    assert!(!c.mirror.window_by_app("strand-b").unwrap().focused);

    // A title change patches the window in place.
    b.set_title("beta, retitled");
    c.until("retitled", |m| {
        m.window_by_app("strand-b")
            .is_some_and(|w| w.title == "beta, retitled" && w.id == wb.id)
    })
    .await;

    // `win.focus()`: `activate` on sway's seat.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(wb.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("b activated", |m| {
        m.focused_window.as_ref().is_some_and(|w| w.id == wb.id)
    })
    .await;
    let tree = sway.msg(&["-t", "get_tree"]);
    let focused_app = |v: &serde_json::Value| -> Option<String> {
        fn find(n: &serde_json::Value) -> Option<String> {
            if n["focused"] == true {
                return n["app_id"].as_str().map(str::to_string);
            }
            n["nodes"].as_array()?.iter().find_map(find)
        }
        find(v)
    };
    let tree: serde_json::Value = serde_json::from_str(&tree).unwrap();
    assert_eq!(focused_app(&tree).as_deref(), Some("strand-b"));

    // The fullscreen state.
    sway.msg(&["fullscreen", "enable"]);
    c.until("fullscreen", |m| {
        m.window_by_app("strand-b").is_some_and(|w| w.fullscreen)
    })
    .await;
    sway.msg(&["fullscreen", "disable"]);
    c.until("not fullscreen", |m| {
        m.window_by_app("strand-b").is_some_and(|w| !w.fullscreen)
    })
    .await;

    // `win.minimize()` is sent (sway has no minimize and ignores it).
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow(wb.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));

    // `win.close()`: sway asks the client, which unmaps.
    let (r, done) = WmRequest::new(WmAction::CloseWindow(wa.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("a closed", |m| m.window_by_app("strand-a").is_none())
        .await;
    assert!(a.closed.load(std::sync::atomic::Ordering::SeqCst));

    // A window that is gone is unknown.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(wa.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(wm::WmError::UnknownWindow(wa.id.clone())));

    // Idle: nothing changes, nothing is sent.
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);
    drop(a);
    drop(b);
    service.abort();
}

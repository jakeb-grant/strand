//! The sway adapter and the protocol client against a real headless sway.

mod common;

use std::time::Duration;

use common::window::TestWindow;
use common::{Collector, Sway};
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
    assert_eq!(done.await.unwrap(), Ok(()));
    c.until("back to 3", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "3")
    })
    .await;

    // Minimise: into the scratchpad, off its workspace.
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow(w.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Ok(()));
    c.until("minimised", |m| {
        m.window_by_app("strand-test")
            .is_some_and(|w| w.minimized && w.workspace.is_none())
    })
    .await;
    // Focus brings it back.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(w.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await.unwrap(), Ok(()));
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
    assert_eq!(done.await.unwrap(), Ok(()));
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

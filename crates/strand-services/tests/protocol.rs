//! The protocol client against a fake compositor (common/fake_wlr.rs)
//! that implements `ext-foreign-toplevel-list-v1` and `ext-workspace-v1`,
//! since the sway in CI (1.9) offers neither, and optionally
//! `zwlr_foreign_toplevel_management_v1` with a seat: the shape of labwc,
//! a compositor with no IPC adapter.

mod common;
#[path = "common/fake_wlr.rs"]
mod fake_wlr;

use std::time::Duration;

use common::Collector;
use fake_wlr::{Cmd, Fake};
use strand_services::wm::{
    self, ProtocolClient, ProtocolState, WaylandTarget, WmAction, WmConfig, WmError, WmRequest,
};
use tokio::sync::mpsc::unbounded_channel;

async fn next_matching(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ProtocolState>,
    what: &str,
    f: impl Fn(&ProtocolState) -> bool,
) -> ProtocolState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = rx.recv().await.expect("the client stopped");
            if f(&s) {
                return s;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: timed out"))
}

#[tokio::test]
async fn protocol_client_follows_toplevels_and_workspaces() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    // Let the fake apply them before the client connects.
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| s.connected).await;
    assert!(s.toplevel_list && s.workspace_manager);
    assert_eq!(s.toplevels.len(), 1);
    assert_eq!(s.toplevels[0].identifier, "tl-1");
    assert_eq!(s.toplevels[0].app_id, "foot");
    assert_eq!(
        s.workspaces
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>(),
        ["1", "2"]
    );
    assert!(s.workspaces[0].active && s.workspaces[0].can_activate);
    assert_eq!(s.workspaces[0].id.as_deref(), Some("fake-1"));
    assert_eq!(s.workspaces[1].coordinates, [1]);
    // The group's output, named by wl_output v4, may come in a later done.
    let named = |s: &ProtocolState| s.workspaces.iter().all(|w| w.screens == ["FAKE-1"]);
    let s = if named(&s) {
        s
    } else {
        next_matching(&mut rx, "screens", named).await
    };
    let key2 = s.workspaces[1].key;

    // A new toplevel, a title change, a close: one state per `done`.
    fake.cmd(Cmd::AddToplevel("tl-2", "Firefox", "firefox"));
    next_matching(&mut rx, "open", |s| s.toplevels.len() == 2).await;
    fake.cmd(Cmd::SetTitle("tl-2", "Strand — Firefox"));
    next_matching(&mut rx, "title", |s| {
        s.toplevels.iter().any(|t| t.title == "Strand — Firefox")
    })
    .await;
    fake.cmd(Cmd::CloseToplevel("tl-1"));
    let s = next_matching(&mut rx, "close", |s| s.toplevels.len() == 1).await;
    assert_eq!(s.toplevels[0].identifier, "tl-2");

    // Workspace state, a new one, a removal.
    fake.cmd(Cmd::SetUrgent("2", true));
    next_matching(&mut rx, "urgent", |s| {
        s.workspaces.iter().any(|w| w.name == "2" && w.urgent)
    })
    .await;
    fake.cmd(Cmd::AddWorkspace("3", false));
    next_matching(&mut rx, "added", |s| s.workspaces.len() == 3).await;
    fake.cmd(Cmd::RemoveWorkspace("3"));
    let s = next_matching(&mut rx, "removed", |s| s.workspaces.len() == 2).await;
    assert_eq!(s.workspaces[1].key, key2, "keys are stable");

    // Idle: nothing is sent.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err()
    );
    drop(client);
    next_matching(&mut rx, "stopped", |s| !s.connected).await;
}

#[tokio::test]
async fn the_protocols_alone_serve_workspaces_and_windows() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        events: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.sources.workspace_protocol
            && m.workspaces.len() == 2
            && m.workspaces.iter().all(|(_, w)| w.screen == "FAKE-1")
    })
    .await;
    assert!(c.mirror.sources.toplevel_list && c.mirror.sources.ipc.is_none());
    assert_eq!(c.mirror.name, "");
    assert_eq!(c.mirror.windows[0].1.id, "tl-1");
    assert_eq!(c.mirror.windows[0].1.icon, "foot");
    assert_eq!(c.mirror.focused_workspace.as_ref().unwrap().name, "1");
    let ws2 = c.mirror.workspace("2").unwrap().id;

    // ws.focus() activates through ext-workspace-v1.
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert_eq!(*fake.activated.lock().unwrap(), ["2"]);

    // The list protocol has no window actions.
    let (r, done) = WmRequest::new(WmAction::CloseWindow("tl-1".into()));
    req_tx.send(r).unwrap();
    assert!(matches!(done.await, Err(WmError::Unsupported(_))));
    service.abort();
}

/// Two outputs, each showing a workspace: `ext-workspace-v1` says what
/// each output shows, not which has the keyboard, so with the protocols
/// alone no workspace is `focused` (each shown one is `active`).
#[tokio::test]
async fn two_outputs_alone_mark_active_workspaces_not_focus() {
    let fake = Fake::start_with(true, &["FAKE-1", "FAKE-2"]);
    fake.cmd(Cmd::AddWorkspaceOn("1", true, 0));
    fake.cmd(Cmd::AddWorkspaceOn("2", false, 0));
    fake.cmd(Cmd::AddWorkspaceOn("3", true, 1));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        desktop: Some("labwc".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.workspaces.len() == 3 && m.workspaces.iter().all(|(_, w)| !w.screen.is_empty())
    })
    .await;
    let m = &c.mirror;
    assert_eq!(m.name, "labwc", "wm.name falls back to XDG_CURRENT_DESKTOP");
    assert_eq!(m.workspace("1").unwrap().screen, "FAKE-1");
    assert_eq!(m.workspace("3").unwrap().screen, "FAKE-2");
    assert!(m.workspace("1").unwrap().active && m.workspace("3").unwrap().active);
    assert!(m.workspaces.iter().all(|(_, w)| !w.focused), "{m:#?}");
    assert_eq!(m.focused_workspace, None);
    assert_eq!(m.focused_screen, None);

    // Activating 2 changes what FAKE-1 shows, not FAKE-2.
    let ws2 = m.workspace("2").unwrap().id;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| m.workspace("2").is_some_and(|w| w.active))
        .await;
    assert!(!c.mirror.workspace("1").unwrap().active);
    assert!(c.mirror.workspace("3").unwrap().active);
    assert_eq!(c.mirror.focused_workspace, None);
    service.abort();
}

/// An adapter that cannot connect (here: Hyprland's sockets are gone)
/// does not hide the standard protocols: their state is published, the
/// sources say the adapter is down, and what the protocol can do works.
#[tokio::test]
async fn a_broken_adapter_does_not_hide_the_protocols() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let gone = tempfile::tempdir().unwrap();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(wm::Backend::hyprland_in(gone.path(), "stale")),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("protocol state", |m| m.workspaces.len() == 2).await;
    let m = &c.mirror;
    assert_eq!(m.sources.ipc, Some(wm::CompositorKind::Hyprland));
    assert!(!m.sources.connected && m.sources.workspace_protocol);
    assert_eq!(m.name, "Hyprland");
    assert_eq!(m.windows[0].1.id, "tl-1");
    assert!(
        matches!(
            &c.log[0],
            wm::WmChange::Sources(s) if s.ipc == Some(wm::CompositorKind::Hyprland) && !s.connected
        ),
        "the first batch says which adapter is meant to run: {:?}",
        c.log[0]
    );
    let ws2 = c.mirror.workspace("2").unwrap().id;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    // Window actions need the adapter.
    let (r, done) = WmRequest::new(WmAction::CloseWindow("tl-1".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::NotConnected));
    service.abort();
}

#[tokio::test]
async fn a_compositor_without_ext_workspace_gives_windows_only() {
    let fake = Fake::start(false);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| s.connected).await;
    assert!(s.toplevel_list && !s.workspace_manager);
    assert!(s.workspaces.is_empty());
    assert_eq!(s.toplevels.len(), 1);
}

#[tokio::test]
async fn no_display_reports_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = unbounded_channel();
    let _client =
        ProtocolClient::spawn(WaylandTarget::Socket(dir.path().join("nothing")), tx).unwrap();
    let s = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s, ProtocolState::default());
}

/// A compositor that accepts the connection and never answers: the
/// startup waits in the same poll loop as the rest, so dropping the client
/// still ends its thread and closes the connection (no leaked thread per
/// subscribe while a compositor hangs).
#[tokio::test]
async fn a_hung_compositor_does_not_keep_the_thread() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayland-hung");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let client = ProtocolClient::spawn(WaylandTarget::Socket(path), tx).unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no state from a compositor that never answered"
    );
    drop(client);
    let last = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the thread did not stop");
    assert_eq!(last, Some(ProtocolState::default()));
    let end = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    assert_eq!(end, Ok(None), "the thread has ended");
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut sent = Vec::new();
    conn.read_to_end(&mut sent)
        .expect("the connection is closed, not left open");
    assert!(!sent.is_empty(), "it had asked for the registry");
}

/// A workspace transaction split across reads (a toplevel's `done` read
/// between its halves) is published only whole, at the manager's `done`:
/// no state ever shows the old workspace inactive and the new one not yet
/// active.
#[tokio::test]
async fn workspace_changes_apply_at_the_managers_done() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    next_matching(&mut rx, "first", |s| s.connected && s.workspaces.len() == 2).await;
    while rx.try_recv().is_ok() {}

    // The first half, then a toplevel's update, in one flush.
    fake.cmd(Cmd::SetActive("1", false));
    fake.cmd(Cmd::SetTitle("tl-1", "vim"));
    let s = next_matching(&mut rx, "title", |s| s.toplevels[0].title == "vim").await;
    let active = |s: &ProtocolState| -> Vec<String> {
        s.workspaces
            .iter()
            .filter(|w| w.active)
            .map(|w| w.name.clone())
            .collect()
    };
    assert_eq!(active(&s), ["1"], "the half-applied switch is not shown");
    std::thread::sleep(Duration::from_millis(100));
    // The second half and the manager's `done`.
    fake.cmd(Cmd::SetActive("2", true));
    fake.cmd(Cmd::Done);
    let s = next_matching(&mut rx, "switched", |s| active(s) == ["2"]).await;
    assert_eq!(s.toplevels[0].title, "vim");
}

/// Hyprland reports each window's `ext-foreign-toplevel-list-v1`
/// identifier as `stableId` in `j/clients`: the protocol's title wins for
/// the windows it joins; the others keep the IPC's.
#[tokio::test]
async fn hyprland_windows_join_the_toplevel_list_by_stable_id() {
    let hypr = common::hyprland::FakeHyprland::start();
    let fake = Fake::start(true);
    // kitty's stableId in the fixture is "18000001" (8 hex digits, as a
    // live Hyprland 0.56.2 sends it); pavucontrol's "18000002" has no
    // toplevel here.
    fake.cmd(Cmd::AddToplevel("18000001", "~ (protocol)", "kitty"));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(hypr.backend.clone()),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("joined", |m| {
        m.sources.connected
            && m.window_by_app("kitty")
                .is_some_and(|w| w.title == "~ (protocol)")
    })
    .await;
    let kitty = c.mirror.window_by_app("kitty").unwrap();
    assert_eq!(kitty.id, "0x55d0c0a1b2c0", "IPC ids stand");
    assert_eq!(
        c.mirror
            .window_by_app("org.pulseaudio.pavucontrol")
            .unwrap()
            .title,
        "Volume Control"
    );
    fake.cmd(Cmd::SetTitle("18000001", "vim (protocol)"));
    c.until("retitled", |m| {
        m.window_by_app("kitty")
            .is_some_and(|w| w.title == "vim (protocol)")
    })
    .await;
    service.abort();
}

/// Dropping the hub while a store still holds a subscription stops the
/// service (its protocol connection goes) and ends the stream.
#[tokio::test]
async fn dropping_the_hub_stops_the_service_its_subscriptions_held() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddWorkspace("1", true));
    std::thread::sleep(Duration::from_millis(50));
    let hub = wm::WmHub::new(
        WmConfig {
            wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
            ..Default::default()
        },
        tokio::runtime::Handle::current(),
    );
    let mut sub = hub.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.clients() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(hub);
    while fake.clients() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the protocol connection outlived the hub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while sub.recv().await.is_some() {}
    });
    assert!(end.await.is_ok(), "the stream ends");
}

/// An adapter that comes up after the protocols' state went out replaces
/// it with a `Reset` of both lists, not a keyed diff: the protocol's
/// workspace 1 and Hyprland's workspace 1 are not the same item.
#[tokio::test]
async fn a_late_adapter_resets_the_lists() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("x", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("web", true));
    std::thread::sleep(Duration::from_millis(50));
    let late = tempfile::tempdir().unwrap();
    let backend = wm::Backend::hyprland_in(late.path(), "late");
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(backend.clone()),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("protocol state", |m| m.workspace_names() == ["web"])
        .await;
    assert_eq!(c.mirror.workspaces[0].0, 1, "the protocol's key 1");

    let _hypr = common::hyprland::FakeHyprland::start_at(backend);
    c.until_within(10, "adapter up", |m| {
        m.sources.connected && m.workspace_names() == ["1", "2", "3"]
    })
    .await;
    let resets = |which: fn(&wm::WmChange) -> bool| c.log.iter().filter(|ch| which(ch)).count();
    let ws_resets = resets(|ch| {
        matches!(ch, wm::WmChange::Workspaces(d)
            if d.iter().any(|d| matches!(d, strand_core::keyed::VecDiff::Reset { .. })))
    });
    let win_resets = resets(|ch| {
        matches!(ch, wm::WmChange::Windows(d)
            if d.iter().any(|d| matches!(d, strand_core::keyed::VecDiff::Reset { .. })))
    });
    assert_eq!((ws_resets, win_resets), (2, 2), "{:#?}", c.log);
    service.abort();
}

/// labwc's shape: no IPC adapter; `ext-foreign-toplevel-list-v1`,
/// `zwlr_foreign_toplevel_management_v1` and `ext-workspace-v1` on two
/// outputs. Windows come from the wlr protocol (ids, focus, minimized),
/// each joined to its list identifier; the activated window says which
/// output has the keyboard, so the workspace shown there is focused; the
/// window actions reach the compositor as wlr requests.
#[tokio::test]
async fn wlr_management_serves_focus_state_and_window_actions() {
    let fake = Fake::start_opts(true, true, &["FAKE-1", "FAKE-2"]);
    fake.cmd(Cmd::AddWorkspaceOn("1", true, 0));
    fake.cmd(Cmd::AddWorkspaceOn("2", true, 1));
    fake.cmd(Cmd::AddToplevelOn("tl-1", "~", "foot", 0));
    fake.cmd(Cmd::AddToplevelOn("tl-2", "Firefox", "firefox", 1));
    fake.cmd(Cmd::Activate("tl-1"));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        desktop: Some("labwc".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.windows.len() == 2
            && m.focused_screen.as_deref() == Some("FAKE-1")
            && m.windows.iter().all(|(_, w)| w.toplevel.is_some())
    })
    .await;
    let m = &c.mirror;
    assert!(m.sources.toplevel_management && m.sources.toplevel_list);
    assert_eq!(m.sources.ipc, None);
    assert_eq!(m.name, "labwc");
    let foot = m.window_by_app("foot").unwrap().clone();
    let firefox = m.window_by_app("firefox").unwrap().clone();
    assert!(foot.id.starts_with("wlr-"), "{foot:?}");
    assert_eq!(foot.toplevel.as_deref(), Some("tl-1"));
    assert_eq!(firefox.toplevel.as_deref(), Some("tl-2"));
    assert_eq!(m.focused_window.as_ref().map(|w| &w.id), Some(&foot.id));
    assert_eq!(
        m.focused_workspace.as_ref().map(|w| w.name.as_str()),
        Some("1"),
        "the workspace shown on the keyboard's output"
    );

    // Focus moves from outside: the other output's workspace is focused.
    fake.cmd(Cmd::Activate("tl-2"));
    c.until("firefox focused", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.id == firefox.id)
            && m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert_eq!(c.mirror.focused_screen.as_deref(), Some("FAKE-2"));
    assert!(!c.mirror.window_by_app("foot").unwrap().focused);

    // `win.focus()`, `win.minimize()`, `win.close()`.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(foot.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("foot activated", |m| {
        m.focused_window.as_ref().is_some_and(|w| w.id == foot.id)
    })
    .await;
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow(foot.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("foot minimized", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.minimized && !w.focused)
    })
    .await;
    assert_eq!(c.mirror.focused_window, None);

    // `win.maximize()` and `win.fullscreen()` toggle from the state the
    // compositor sent: set, then unset.
    for (action, what) in [
        (
            WmAction::MaximizeWindow as fn(String) -> WmAction,
            "maximized",
        ),
        (WmAction::FullscreenWindow, "fullscreen"),
    ] {
        for on in [true, false] {
            let (r, done) = WmRequest::new(action(firefox.id.clone()));
            req_tx.send(r).unwrap();
            assert_eq!(done.await, Ok(()));
            c.until(what, |m| {
                m.window_by_app("firefox").is_some_and(|w| {
                    let state = if what == "maximized" {
                        w.maximized
                    } else {
                        w.fullscreen
                    };
                    state == on
                })
            })
            .await;
        }
    }

    let (r, done) = WmRequest::new(WmAction::CloseWindow(firefox.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("firefox closed", |m| m.window_by_app("firefox").is_none())
        .await;
    assert_eq!(
        *fake.wlr_requests.lock().unwrap(),
        [
            "activate tl-1",
            "minimize tl-1",
            "maximize tl-2",
            "unmaximize tl-2",
            "fullscreen tl-2",
            "unfullscreen tl-2",
            "close tl-2"
        ]
    );

    // A title change is one keyed update; an unknown window is refused.
    fake.cmd(Cmd::SetTitle("tl-1", "vim"));
    c.until("retitled", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "vim" && w.id == foot.id)
    })
    .await;
    let (r, done) = WmRequest::new(WmAction::CloseWindow(firefox.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::UnknownWindow(firefox.id.clone())));

    // Idle: nothing is sent.
    assert_eq!(c.quiet_for(Duration::from_millis(300)).await, 0);
    service.abort();
}

/// The protocol client reads the wlr state: activated, minimized, the
/// outputs by name, and a closed toplevel leaves.
#[tokio::test]
async fn protocol_client_follows_wlr_toplevels() {
    let fake = Fake::start_opts(false, true, &["FAKE-1"]);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| {
        s.connected && s.managed.first().is_some_and(|m| m.screens == ["FAKE-1"])
    })
    .await;
    assert!(s.toplevel_management && s.toplevel_list && !s.workspace_manager);
    assert_eq!(s.managed.len(), 1);
    assert_eq!(
        (s.managed[0].app_id.as_str(), s.managed[0].title.as_str()),
        ("foot", "~")
    );
    assert!(!s.managed[0].activated);
    let key = s.managed[0].key;
    fake.cmd(Cmd::Activate("tl-1"));
    next_matching(&mut rx, "activated", |s| s.managed[0].activated).await;
    fake.cmd(Cmd::AddToplevel("tl-2", "x", "xterm"));
    let s = next_matching(&mut rx, "second", |s| s.managed.len() == 2).await;
    assert_ne!(s.managed[1].key, key);
    fake.cmd(Cmd::CloseToplevel("tl-1"));
    let s = next_matching(&mut rx, "closed", |s| s.managed.len() == 1).await;
    assert_eq!(s.managed[0].app_id, "xterm");
}

/// Every seat is bound: when the seat `activate` names goes away,
/// `win.focus()` falls back to a seat announced before it, and answers
/// `Unsupported` only once no seat is left.
#[tokio::test]
async fn win_focus_falls_back_to_another_seat_when_one_is_removed() {
    let fake = Fake::start_seats(false, true, 2, &["FAKE-1"]);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        desktop: Some("labwc".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.windows.len() == 1).await;
    let foot = c.mirror.window_by_app("foot").unwrap().clone();
    let focus = || {
        let (r, done) = WmRequest::new(WmAction::FocusWindow(foot.id.clone()));
        req_tx.send(r).unwrap();
        done
    };

    // Both seats offered: the first one announced is named.
    assert_eq!(focus().await, Ok(()));
    // The first seat goes. The retitle is sent after the global's removal
    // on the same connection, so once it shows the removal was read.
    fake.cmd(Cmd::RemoveSeat(0));
    fake.cmd(Cmd::SetTitle("tl-1", "one seat"));
    c.until("retitled", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "one seat")
    })
    .await;
    assert_eq!(focus().await, Ok(()));
    // No seat left: refused.
    fake.cmd(Cmd::RemoveSeat(1));
    fake.cmd(Cmd::SetTitle("tl-1", "no seat"));
    c.until("retitled again", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "no seat")
    })
    .await;
    assert_eq!(
        focus().await,
        Err(WmError::Unsupported("the compositor offers no seat"))
    );
    assert_eq!(
        *fake.wlr_requests.lock().unwrap(),
        ["activate tl-1", "activate tl-1 on seat 1"]
    );
    service.abort();
}

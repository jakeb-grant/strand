//! The Hyprland adapter against a fake Hyprland: two Unix sockets that
//! replay Hyprland 0.56.2's traffic as reconstructed from its source
//! (`tests/fixtures/hyprland-0.56.2`).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::hyprland::FakeHyprland;
use common::{Collector, bursts};
use strand_services::wm::{self, Backend, WmAction, WmConfig, WmError, WmRequest};
use strand_watch::{ChangeEvent, CompositorEvent};
use tokio::net::UnixListener;
use tokio::sync::mpsc::unbounded_channel;

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
        ..Default::default()
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
    assert_eq!(done.await, Ok(()));
    let (r, done) = WmRequest::new(WmAction::CloseWindow("0x55d0c0a1c3d0".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow("0x55d0c0a1c3d0".into()));
    req_tx.send(r).unwrap();
    assert!(matches!(done.await, Err(WmError::Unsupported(_))));
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
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;

    // The event socket goes away; a window opened meanwhile.
    fake.drop_events();
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

/// Titles that are not UTF-8 (XWayland Latin-1) in events and replies are
/// shown with U+FFFD and cost nothing: no reconnect, no lost state.
#[tokio::test]
async fn titles_that_are_not_utf8_keep_the_connection() {
    let fake = FakeHyprland::start();
    let bursts = bursts("hyprland-0.56.2/events.txt");
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;

    // A window opens whose title j/clients reports in Latin-1.
    fake.set_scene("latin1");
    fake.send(&bursts["open"]);
    c.until("open", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "caf\u{fffd} \u{fffd}")
    })
    .await;

    // Its title changes, and the event carries Latin-1 too.
    let mut line = b"windowtitle>>55d0c0a1c3d0\nwindowtitlev2>>55d0c0a1c3d0,".to_vec();
    line.extend_from_slice(b"na\xefve\n");
    fake.send_bytes(&line);
    c.until("title", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "na\u{fffd}ve")
    })
    .await;
    assert!(c.mirror.sources.connected);
    let up = c
        .log
        .iter()
        .position(|ch| matches!(ch, wm::WmChange::Sources(s) if s.connected))
        .unwrap();
    assert!(
        !c.log[up..]
            .iter()
            .any(|ch| matches!(ch, wm::WmChange::Sources(s) if !s.connected)),
        "the connection never dropped once up"
    );
    assert_eq!(
        fake.requests().len(),
        8,
        "boot and one re-read; no reconnect"
    );
    service.abort();
}

/// A Hyprland whose event socket accepts and hangs up at once (and whose
/// request socket is gone) is retried on the backoff schedule, not in a
/// loop: about 4 attempts in a second (0, 100, 300, 700 ms). Meanwhile
/// the service says the adapter is down and publishes what it has.
#[tokio::test]
async fn a_broken_hyprland_is_retried_with_backoff_not_a_busy_loop() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::hyprland_in(dir.path(), "broken");
    let Backend::Hyprland { events, .. } = backend.clone() else {
        unreachable!()
    };
    std::fs::create_dir_all(events.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&events).unwrap();
    let attempts = Arc::new(Mutex::new(0u32));
    let counted = attempts.clone();
    let acceptor = tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            *counted.lock().unwrap() += 1;
            drop(conn);
        }
    });
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(backend),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.quiet_for(Duration::from_millis(1000)).await;
    let n = *attempts.lock().unwrap();
    assert!((2..=5).contains(&n), "{n} attempts in 1 s");
    assert_eq!(c.mirror.sources.ipc, Some(wm::CompositorKind::Hyprland));
    assert!(!c.mirror.sources.connected);
    assert_eq!(c.mirror.name, "Hyprland");
    assert!(c.mirror.workspaces.is_empty());
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(1));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::NotConnected));
    service.abort();
    acceptor.abort();
}

/// Several stores share one service through a hub: one adapter
/// connection, a late subscriber starts from the current state, and the
/// last one to leave stops it.
#[tokio::test]
async fn the_hub_shares_one_adapter_between_stores() {
    let fake = FakeHyprland::start();
    let hub = wm::WmHub::new(
        WmConfig {
            backend: Some(fake.backend.clone()),
            wayland: None,
            ..Default::default()
        },
        tokio::runtime::Handle::current(),
    );
    let mut a = hub.subscribe();
    let mut ma = wm::Mirror::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(ma.sources.connected && !ma.workspaces.is_empty()) {
        let batch = tokio::time::timeout_at(deadline, a.recv())
            .await
            .unwrap()
            .unwrap();
        for ch in &batch {
            ma.apply(ch).unwrap();
        }
    }
    // A second store joins: no second connection, the state at once.
    let mut b = hub.subscribe();
    let mut mb = wm::Mirror::default();
    for ch in &b.try_recv().expect("the current state is queued") {
        mb.apply(ch).unwrap();
    }
    assert_eq!(mb, ma);
    assert_eq!(hub.starts(), 1);
    assert_eq!(fake.requests().len(), 4, "one read for both");

    // Both follow the stream; actions go through either.
    let bursts = bursts("hyprland-0.56.2/events.txt");
    fake.send(&bursts["switch"]);
    for (s, m) in [(&mut a, &mut ma), (&mut b, &mut mb)] {
        while !m.focused_workspace.as_ref().is_some_and(|w| w.name == "2") {
            let batch = tokio::time::timeout(Duration::from_secs(5), s.recv())
                .await
                .unwrap()
                .unwrap();
            for ch in &batch {
                m.apply(ch).unwrap();
            }
        }
    }
    assert_eq!(b.request(WmAction::FocusWorkspace(3)).await, Ok(()));

    // The last one to leave stops it; the next one starts it afresh.
    drop(a);
    assert!(hub.running());
    drop(b);
    assert!(!hub.running());
    let mut c = hub.subscribe();
    assert!(c.try_recv().is_none(), "no stale state from the last run");
    assert_eq!(hub.starts(), 2);
    let batch = tokio::time::timeout(Duration::from_secs(5), c.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!batch.is_empty());
    drop(c);
}

/// Hyprland cuts event data at 1024 bytes, so a long title's
/// `windowtitlev2` may arrive cut (here inside a UTF-8 sequence): the
/// adapter re-reads `j/clients` instead of showing the cut title.
#[tokio::test]
async fn titles_cut_at_hyprlands_event_cap_are_reread() {
    let fake = FakeHyprland::start();
    let bursts = bursts("hyprland-0.56.2/events.txt");
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.set_scene("opened");
    fake.send(&bursts["open"]);
    c.until("open", |m| m.window_by_app("foot").is_some()).await;
    let before = fake.requests().len();

    // `<address>,` and a title of 2-byte `é`s, cut by Hyprland at 1024
    // bytes of data: the last `é` loses its second byte.
    let mut data = b"55d0c0a1c3d0,".to_vec();
    while data.len() < 1024 {
        data.extend_from_slice("é".as_bytes());
    }
    data.truncate(1024);
    let mut line = b"windowtitle>>55d0c0a1c3d0\nwindowtitlev2>>".to_vec();
    line.extend_from_slice(&data);
    line.push(b'\n');
    fake.send_bytes(&line);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.requests().len() < before + 4 {
        assert!(tokio::time::Instant::now() < deadline, "no re-read");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    c.quiet_for(Duration::from_millis(200)).await;
    let foot = c.mirror.window_by_app("foot").unwrap();
    assert_eq!(foot.title, "foot", "j/clients' title, not the cut one");
    assert!(c.mirror.sources.connected);
    service.abort();
}

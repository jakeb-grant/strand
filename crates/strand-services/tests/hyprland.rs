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
    // Hyprland sends the new window's title and an urgent hint before
    // `openwindow` (as captured): the unknown window costs no extra read,
    // and the hint does not outlive the focus that follows it.
    assert_eq!(fake.requests().len(), 8, "one re-read for the burst");
    let ws2 = c.mirror.workspace("2").unwrap();
    assert!(ws2.occupied && ws2.focused);
    assert!(!ws2.urgent, "the focus ends the urgent hint");
    assert!(!c.mirror.window_by_app("foot").unwrap().urgent);
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
    // `win.maximize()`, `win.fullscreen()`: the classic `fullscreen`
    // dispatcher acts on the focused window, so one batch focuses first;
    // Hyprland answers each command (`ok`, "\n\n\n", `ok`).
    for action in [
        WmAction::MaximizeWindow("0x55d0c0a1c3d0".into()),
        WmAction::FullscreenWindow("0x55d0c0a1c3d0".into()),
    ] {
        let (r, done) = WmRequest::new(action);
        req_tx.send(r).unwrap();
        assert_eq!(done.await, Ok(()));
    }
    let (r, done) = WmRequest::new(WmAction::FullscreenWindow("0xdead".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::UnknownWindow("0xdead".into())));
    // This fake reads dispatches as a classic Hyprland (before 0.55, or a
    // hyprlang config): the first, in Lua, is refused (`Invalid
    // dispatcher`) and said again in the classic dialect, which the
    // connection then keeps.
    let sent: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| !r.starts_with("j/"))
        .collect();
    assert_eq!(
        sent,
        [
            r#"dispatch hl.dsp.focus({ workspace = "3" })"#,
            "dispatch workspace 3",
            "dispatch closewindow address:0x55d0c0a1c3d0",
            "[[BATCH]]dispatch focuswindow address:0x55d0c0a1c3d0;dispatch fullscreen 1",
            "[[BATCH]]dispatch focuswindow address:0x55d0c0a1c3d0;dispatch fullscreen 0",
        ]
    );

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

/// Hyprland with a Lua config (0.55 on; the only kind from 0.56) gets
/// dispatcher objects from the first action on: Lua is the dialect the
/// adapter starts in, so nothing is refused first.
#[tokio::test]
async fn a_lua_config_hyprland_gets_lua_dispatches() {
    let fake = FakeHyprland::start();
    fake.set_lua(true);
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        events: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.set_scene("opened");
    fake.send("openwindow>>55d0c0a1c3d0,2,foot,foot\n");
    c.until("the window", |m| m.window_by_app("foot").is_some())
        .await;

    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let (r, done) = WmRequest::new(WmAction::CloseWindow("0x55d0c0a1c3d0".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    for action in [
        WmAction::MaximizeWindow("0x55d0c0a1c3d0".into()),
        WmAction::FullscreenWindow("0x55d0c0a1c3d0".into()),
    ] {
        let (r, done) = WmRequest::new(action);
        req_tx.send(r).unwrap();
        assert_eq!(done.await, Ok(()));
    }
    let dispatches: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| !r.starts_with("j/"))
        .collect();
    assert_eq!(
        dispatches,
        [
            r#"dispatch hl.dsp.focus({ workspace = "3" })"#,
            r#"dispatch hl.dsp.window.close({ window = "address:0x55d0c0a1c3d0" })"#,
            r#"dispatch hl.dsp.window.fullscreen({ mode = "maximized", window = "address:0x55d0c0a1c3d0" })"#,
            r#"dispatch hl.dsp.window.fullscreen({ mode = "fullscreen", window = "address:0x55d0c0a1c3d0" })"#,
        ]
    );
    service.abort();
}

/// The first action on a classic Hyprland (before 0.55, or a hyprlang
/// config) may be `win.fullscreen()`: its Lua form is refused (`Invalid
/// dispatcher`), so the classic form follows, a batch that focuses the
/// window first; the connection keeps the classic dialect, so the next
/// action is classic at once. A new connection starts in Lua again.
#[tokio::test]
async fn a_classic_hyprland_gets_the_batch_after_refusing_lua() {
    let fake = FakeHyprland::start();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        events: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    let (r, done) = WmRequest::new(WmAction::FullscreenWindow("0x55d0c0a1b2c0".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    let sent = |fake: &FakeHyprland| -> Vec<String> {
        fake.requests()
            .into_iter()
            .filter(|r| !r.starts_with("j/"))
            .collect()
    };
    assert_eq!(
        sent(&fake),
        [
            r#"dispatch hl.dsp.window.fullscreen({ mode = "fullscreen", window = "address:0x55d0c0a1b2c0" })"#,
            "[[BATCH]]dispatch focuswindow address:0x55d0c0a1b2c0;dispatch fullscreen 0",
            "dispatch workspace 2",
        ]
    );

    // Hyprland restarts with a Lua config: the new connection is Lua.
    fake.set_lua(true);
    fake.drop_events();
    c.until("lost", |m| !m.sources.connected).await;
    c.until("back", |m| m.sources.connected).await;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    assert_eq!(
        sent(&fake).last().map(String::as_str),
        Some(r#"dispatch hl.dsp.focus({ workspace = "3" })"#)
    );
    assert_eq!(sent(&fake).len(), 4, "no classic attempt first");
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

/// Whether the adapter is degraded, and why.
fn degraded(m: &wm::Mirror) -> Option<&str> {
    m.sources.degraded.as_deref()
}

/// A reply of another shape (a newer Hyprland renamed `address`) is not
/// understood: the adapter is degraded at once, and the service serves the
/// standard protocols exactly as when the adapter cannot connect (here no
/// display, so nothing: not the last, frozen state), saying why with
/// Hyprland's version. It is retried on the backoff schedule, not in a
/// loop, and the state comes back, ids reset, once Hyprland is read again.
#[tokio::test]
async fn a_hyprland_reply_of_another_shape_degrades_until_it_is_read_again() {
    let fake = FakeHyprland::start();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    assert_eq!(degraded(&c.mirror), None);

    fake.set_reply(
        "j/clients",
        br#"[{"addr": "0x55d0c0a1c3d0", "mapped": true, "workspace": {"id": 2, "name": "2"}}]"#,
    );
    fake.send("openwindow>>55d0c0a1c3d0,2,foot,foot\n");
    c.until("degraded", |m| degraded(m).is_some()).await;
    let why = degraded(&c.mirror).unwrap().to_string();
    assert!(why.contains("Hyprland 0.56.2"), "{why}");
    assert!(
        why.contains("j/clients") && why.contains("missing field `address`"),
        "{why}"
    );
    assert!(why.contains("standard Wayland protocols"), "{why}");
    assert!(!c.mirror.sources.connected);
    assert!(
        c.mirror.workspaces.is_empty() && c.mirror.windows.is_empty(),
        "the protocols' state, not the last one: {:?}",
        c.mirror
    );
    assert_eq!(c.mirror.name, "Hyprland");
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(1));
    req_tx.send(r).unwrap();
    assert_eq!(
        done.await,
        Err(WmError::NotConnected),
        "no IPC ids to act on"
    );

    // Retried on the backoff (100, 200, 400, 800 ms…), not in a loop.
    let reads = |fake: &FakeHyprland| {
        fake.requests()
            .iter()
            .filter(|r| *r == "j/monitors")
            .count()
    };
    let before = reads(&fake);
    c.quiet_for(Duration::from_millis(1000)).await;
    let attempts = reads(&fake) - before;
    assert!((1..=5).contains(&attempts), "{attempts} attempts in 1 s");
    assert_eq!(degraded(&c.mirror), Some(why.as_str()), "said once");
    let said = c
        .log
        .iter()
        .filter(|ch| matches!(ch, wm::WmChange::Sources(s) if s.degraded.is_some()))
        .count();
    assert_eq!(said, 1, "every failed retry says the same: no new batch");

    // Hyprland is read again (fixed, or upgraded): its state is back.
    fake.clear_replies();
    c.until_within(15, "recovered", |m| {
        m.sources.connected && degraded(m).is_none() && !m.workspaces.is_empty()
    })
    .await;
    assert!(c.mirror.window_by_app("kitty").is_some());
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));

    // Then idle again: no polling.
    let before = fake.requests().len();
    assert_eq!(c.quiet_for(Duration::from_millis(500)).await, 0);
    assert_eq!(fake.requests().len(), before);
    service.abort();
}

/// An event name this adapter does not know (a newer Hyprland's) is
/// ignored, as before: no read, no degradation. A line that is no event
/// at all (no `>>`) costs a re-read; only `MAX_STRIKES` (8) of them in a
/// row, with no event between, mean the stream is not Hyprland's.
#[tokio::test]
async fn an_unknown_hyprland_event_is_harmless_but_a_stream_of_no_events_is_not() {
    let fake = FakeHyprland::start();
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
    let before = fake.requests().len();
    fake.send("brandnewevent>>1,2,3\nanothernewone>>\n");
    assert_eq!(c.quiet_for(Duration::from_millis(200)).await, 0);
    assert_eq!(fake.requests().len(), before, "no read");

    // Seven lines that are no event, then an event: re-read, harmless.
    fake.send(&format!(
        "{}activelayout>>kb,us\n",
        "not an event\n".repeat(7)
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.requests().len() < before + 4 {
        assert!(tokio::time::Instant::now() < deadline, "no re-read");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    c.quiet_for(Duration::from_millis(200)).await;
    assert_eq!(degraded(&c.mirror), None);
    assert!(c.mirror.sources.connected);

    // Eight in a row.
    fake.send(&"not an event\n".repeat(8));
    c.until("degraded", |m| degraded(m).is_some()).await;
    let why = degraded(&c.mirror).unwrap();
    assert!(
        why.contains("Hyprland 0.56.2") && why.contains("event stream"),
        "{why}"
    );
    assert!(why.contains("`not an event`"), "{why}");

    // A Hyprland whose stream stays that way: every retry reads the state
    // (its replies are fine) and the stream, but none comes up, so the
    // services never flip back to its ids, and the reason is raised once.
    let why = why.to_string();
    let versions =
        |fake: &FakeHyprland| fake.requests().iter().filter(|r| *r == "j/version").count();
    let before = versions(&fake);
    let up = c.log.len();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < deadline {
        fake.send(&"not an event\n".repeat(8));
        c.quiet_for(Duration::from_millis(50)).await;
    }
    assert!(versions(&fake) >= before + 2, "retried");
    assert_eq!(degraded(&c.mirror), Some(why.as_str()));
    assert!(c.mirror.workspaces.is_empty());
    for ch in &c.log[up..] {
        match ch {
            wm::WmChange::Sources(s) => {
                assert!(!s.connected, "no flip back to the IPC state: {s:?}");
                assert_eq!(s.degraded.as_deref(), Some(why.as_str()), "raised once");
            }
            other => panic!("the retries changed something: {other:?}"),
        }
    }

    // Its stream carries events again: up, with its state.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(c.mirror.sources.connected
        && degraded(&c.mirror).is_none()
        && !c.mirror.workspaces.is_empty())
    {
        assert!(tokio::time::Instant::now() < deadline, "not recovered");
        fake.send("activelayout>>kb,us\n");
        c.quiet_for(Duration::from_millis(100)).await;
    }
    service.abort();
}

/// A Hyprland refusing a later action (a fullscreen form) in both dialects
/// may just not have it: that action is rejected, and the adapter stays.
#[tokio::test]
async fn a_hyprland_refusing_a_later_action_in_both_dialects_rejects_only_it() {
    let fake = FakeHyprland::start();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.windows.is_empty())
        .await;
    fake.set_reply("dispatch ", b"Invalid dispatcher");
    let (r, done) = WmRequest::new(WmAction::FullscreenWindow("0x55d0c0a1b2c0".into()));
    req_tx.send(r).unwrap();
    assert_eq!(
        done.await,
        Err(WmError::Rejected("Invalid dispatcher".into()))
    );
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
    fake.clear_reply("dispatch ");
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    service.abort();
}

/// A Hyprland that refuses both dispatch dialects' syntax (here every
/// dispatch is `Invalid dispatcher`, as if both were renamed): the action
/// fails, the adapter degrades, and the protocols take the actions. The
/// refusal holds for that version: retries that find it stay degraded
/// (the state does not flip back and forth); another version is tried
/// again, and recovers.
#[tokio::test]
async fn a_hyprland_refusing_both_dispatch_dialects_degrades_until_its_version_changes() {
    let fake = FakeHyprland::start();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.sources.connected && !m.workspaces.is_empty())
        .await;
    fake.set_reply("dispatch ", b"Invalid dispatcher");
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(
        done.await,
        Err(WmError::Rejected("Invalid dispatcher".into()))
    );
    c.until("degraded", |m| degraded(m).is_some()).await;
    let why = degraded(&c.mirror).unwrap().to_string();
    assert!(why.contains("Hyprland 0.56.2"), "{why}");
    assert!(why.contains("both dialects"), "{why}");
    let dispatches: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("dispatch"))
        .collect();
    assert_eq!(
        dispatches,
        [
            r#"dispatch hl.dsp.focus({ workspace = "3" })"#,
            "dispatch workspace 3"
        ]
    );
    assert!(c.mirror.workspaces.is_empty());

    // The same Hyprland, retried: still degraded, never up in between.
    let versions =
        |fake: &FakeHyprland| fake.requests().iter().filter(|r| *r == "j/version").count();
    let before = versions(&fake);
    let up = c.log.len();
    c.quiet_for(Duration::from_millis(1500)).await;
    assert!(versions(&fake) > before, "the retries ask for the version");
    assert_eq!(degraded(&c.mirror), Some(why.as_str()));
    assert!(
        !c.log[up..]
            .iter()
            .any(|ch| matches!(ch, wm::WmChange::Sources(s) if s.connected)),
        "no flip back to the IPC state"
    );

    // Upgraded: another version, which understands the dispatches.
    fake.clear_reply("dispatch ");
    fake.set_reply("j/version", br#"{"version": "0.57.0", "tag": "v0.57.0"}"#);
    c.until_within(15, "recovered", |m| {
        m.sources.connected && degraded(m).is_none() && !m.workspaces.is_empty()
    })
    .await;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(3));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    service.abort();
}

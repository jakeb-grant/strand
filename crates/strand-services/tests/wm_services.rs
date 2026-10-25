//! The `workspaces`, `windows` and `wm` stores on the service contract,
//! against a real headless sway: three services to the language, one
//! compositor service (adapter and protocol thread) underneath, started
//! with the first reader and stopped 5 s (logic clock) after the last;
//! keyed diffs stay keyed diffs; actions reach sway; `swaymsg reload` is
//! `wm.config_reloaded`; an idle compositor wakes nothing.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::Sway;
use common::window::TestWindow;
use strand_core::{Runtime, VecDiff};
use strand_services::wm::{
    self, Backend, WaylandTarget, WindowAction, WmConfig, WorkspaceAction, WorkspaceItem,
};
use strand_services::{Applied, Builtin, Buses, Cells, Data, STOP_GRACE, Services};

/// The tests configure the process-wide compositor config: one at a time,
/// each linking windows to no desktop entry unless it sets its own
/// (`common::no_desktop_entries`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    common::no_desktop_entries();
    guard
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn workspace(b: &Builtin, rt: &Runtime, name: &str) -> Option<WorkspaceItem> {
    let s = b.workspaces.cells().snapshot(rt).ok()?;
    s.all.into_iter().find(|w| w.name == name)
}

fn focused(b: &Builtin, rt: &Runtime) -> Option<String> {
    let s = b.workspaces.cells().snapshot(rt).ok()?;
    s.focused.map(|w| w.name)
}

/// sway's own idea of the focused workspace.
fn sway_focused(sway: &Sway) -> String {
    let out = sway.msg(&["-t", "get_workspaces", "-r"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .find(|w| w["focused"] == true)
        .and_then(|w| w["name"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn the_compositor_stores_follow_sway_through_one_hub() {
    let _serial = serial();
    let Some(sway) = Sway::start("the_compositor_stores_follow_sway_through_one_hub") else {
        return;
    };
    wm::configure(Some(WmConfig {
        backend: Some(Backend::Sway {
            socket: sway.ipc.clone(),
        }),
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        ..Default::default()
    }));
    let rt = Runtime::new();
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    let s = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let b = Builtin::register(&s, &rt);
    let applied: Rc<RefCell<Vec<(&'static str, Applied)>>> = Rc::default();
    for (name, svc) in [
        ("wm", b.wm.dynamic()),
        ("windows", b.windows.dynamic()),
        ("workspaces", b.workspaces.dynamic()),
    ] {
        let a = applied.clone();
        svc.observe(Box::new(move |_, x| a.borrow_mut().push((name, x.clone()))));
    }
    let runs = wm::live_runs();

    // Three readers, three services, one compositor service.
    b.workspaces.acquire(&rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "boot", || {
        focused(&b, &rt).as_deref() == Some("1")
            && b.wm.cells().snapshot(&rt).is_ok_and(|w| w.name == "sway")
    });
    assert_eq!(wm::live_runs(), runs + 1, "one hub run for three stores");
    let ws1 = workspace(&b, &rt, "1").unwrap();
    assert!(ws1.focused && ws1.active && !ws1.occupied);
    assert_eq!(ws1.screen, "HEADLESS-1");

    // `workspaces.on(screen)`: a `fn` method on the logic thread.
    let screen = |name: &str| Data::Record {
        ty: "Screen".into(),
        fields: vec![("name".into(), Data::text(name))],
    };
    let on = b
        .workspaces
        .dynamic()
        .call(&rt, "on", &[screen("HEADLESS-1")])
        .unwrap()
        .unwrap();
    assert!(matches!(&on, Data::List(l) if l.len() == 1), "{on:?}");
    let off = b
        .workspaces
        .dynamic()
        .call(&rt, "on", &[screen("DP-9")])
        .unwrap()
        .unwrap();
    assert_eq!(off, Data::List(vec![]));

    // Switching from outside.
    sway.msg(&["workspace", "3"]);
    until(&rt, &s, "switch", || {
        focused(&b, &rt).as_deref() == Some("3")
    });

    // A real window: `windows.all`, `windows.focused`, the workspace's
    // windows.
    let win = TestWindow::open(&sway.socket(), "strand-test", "hello");
    until(&rt, &s, "a window", || {
        b.windows.cells().snapshot(&rt).is_ok_and(|w| {
            w.focused
                .as_ref()
                .is_some_and(|f| f.app_id == "strand-test")
                && w.all.iter().any(|x| x.title == "hello")
        })
    });
    let ws3 = workspace(&b, &rt, "3").unwrap();
    assert!(ws3.occupied && ws3.windows.len() == 1);
    let window = b.windows.cells().snapshot(&rt).unwrap().all[0].clone();
    assert_eq!(window.workspace, Some(ws3.id));

    // A title change is one keyed update of the window, not a new list.
    applied.borrow_mut().clear();
    win.set_title("hello, world");
    until(&rt, &s, "the title", || {
        b.windows
            .cells()
            .snapshot(&rt)
            .is_ok_and(|w| w.all.iter().any(|x| x.title == "hello, world"))
    });
    let diffs: Vec<VecDiff<Data, Data>> = applied
        .borrow()
        .iter()
        .filter_map(|(n, a)| match a {
            Applied::Keyed { diffs, .. } if *n == "windows" => Some(diffs.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        !diffs.is_empty() && diffs.iter().all(|d| matches!(d, VecDiff::Update { .. })),
        "{diffs:?}"
    );

    // `ws.focus()` reaches sway.
    sway.msg(&["workspace", "2"]);
    until(&rt, &s, "ws 2", || focused(&b, &rt).as_deref() == Some("2"));
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item: ws3.clone() })
        .unwrap();
    until(&rt, &s, "back to 3", || {
        focused(&b, &rt).as_deref() == Some("3")
    });
    assert_eq!(sway_focused(&sway), "3");

    // `swaymsg reload` is `wm.config_reloaded` (sway does not say whether
    // it failed: null).
    applied.borrow_mut().clear();
    sway.msg(&["reload"]);
    until(&rt, &s, "the reload event", || {
        applied.borrow().iter().any(|(n, a)| {
            *n == "wm" && matches!(a, Applied::Event { event: 0, args } if args == &[Data::Null])
        })
    });

    // `win.close()`: sway asks the client.
    b.windows
        .act(&rt, WindowAction::Close { item: window })
        .unwrap();
    until(&rt, &s, "closed", || {
        b.windows
            .cells()
            .snapshot(&rt)
            .is_ok_and(|w| w.all.is_empty() && w.focused.is_none())
    });
    assert!(win.closed.load(Ordering::SeqCst));

    // Idle: nothing changes, nothing wakes the logic thread.
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    let before = wakes.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        before,
        "an idle compositor woke the host"
    );

    // A binding that toggles (read, unread, read again within 5 s)
    // rebuilds nothing: the store keeps running, and the hub keeps its
    // one run (the adapter connection) and its one protocol thread.
    let starts = b.workspaces.starts();
    assert_eq!(threads_named("strand-toplevel"), 1);
    b.workspaces.release(&rt);
    let t0 = Duration::from_secs(2);
    rt.tick(t0);
    b.workspaces.acquire(&rt);
    s.pump(&rt);
    assert!(b.workspaces.running());
    assert_eq!(b.workspaces.starts(), starts, "the store restarted");
    assert_eq!(wm::live_runs(), runs + 1, "the hub's run restarted");
    assert_eq!(threads_named("strand-toplevel"), 1);

    // The last readers go: 5 s on the logic clock later every store
    // stops, and with them the compositor service.
    b.workspaces.release(&rt);
    b.windows.release(&rt);
    rt.tick(t0 + STOP_GRACE + Duration::from_millis(1));
    assert!(!b.workspaces.running() && !b.windows.running());
    assert!(b.wm.running(), "a store still read keeps the hub");
    assert_eq!(wm::live_runs(), runs + 1);
    b.wm.release(&rt);
    rt.tick(t0 + 2 * STOP_GRACE + Duration::from_millis(2));
    assert!(!b.wm.running());
    let deadline = Instant::now() + Duration::from_secs(5);
    while wm::live_runs() != runs || threads_named("strand-toplevel") != 0 {
        assert!(
            Instant::now() < deadline,
            "the compositor service or its protocol thread outlived its stores"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The stop told the protocol thread to end without waiting for it on
    // the shared runtime; it ended on its own (and is joined at the next
    // start, or at the shutdown).

    // Read again (a new run and protocol thread), then shut the services
    // down while it is read: the shutdown joins the protocol thread too.
    b.workspaces.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the second run");
    until(&rt, &s, "the second run's protocol thread", || {
        threads_named("strand-toplevel") == 1 && wm::live_runs() == runs + 1
    });
    s.shutdown();
    assert_eq!(
        threads_named("strand-toplevel"),
        0,
        "after the shutdown; this process's threads: {:?}",
        thread_names()
    );
    assert_eq!(wm::live_runs(), runs);
    wm::configure(None);
}

/// The names of this process's threads (a failure's witness).
fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .map(|comm| comm.trim_end().to_string())
        .collect()
}

/// This process's threads named `name`.
fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.trim_end() == name)
        .count()
}

/// The same stores over the Hyprland adapter (a fake Hyprland replaying
/// 0.56.2's traffic): the hub is adapter-agnostic, so what the sway test
/// shows holds for Hyprland too; this checks the store side of its
/// workspaces, a switch, a dispatch and `configreloaded`.
#[test]
fn the_compositor_stores_follow_hyprland() {
    let _serial = serial();
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _enter = tokio.enter();
    let fake = common::hyprland::FakeHyprland::start();
    wm::configure(Some(WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let runs = wm::live_runs();
    let reloads: Rc<RefCell<Vec<Vec<Data>>>> = Rc::default();
    let r = reloads.clone();
    b.wm.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { args, .. } = a {
            r.borrow_mut().push(args.clone());
        }
    }));
    b.workspaces.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "boot", || {
        focused(&b, &rt).as_deref() == Some("1")
            && b.wm
                .cells()
                .snapshot(&rt)
                .is_ok_and(|w| w.name == "Hyprland")
    });
    let bursts = common::bursts("hyprland-0.56.2/events.txt");
    fake.send(&bursts["switch"]);
    until(&rt, &s, "switch", || {
        focused(&b, &rt).as_deref() == Some("2")
    });
    let one = workspace(&b, &rt, "1").unwrap();
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item: one })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fake.requests().iter().any(|r| r == "dispatch workspace 1") {
        assert!(Instant::now() < deadline, "{:?}", fake.requests());
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Gaps 3 (m6-w2-services): the rules' slots not made yet, and a
    // screen's slots in number order with its workspaces.
    let configured: Vec<i64> = b
        .workspaces
        .cells()
        .configured
        .get_untracked(&rt)
        .unwrap()
        .items()
        .iter()
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(configured, [4, 6]);
    let screen = |name: &str| Data::Record {
        ty: "Screen".into(),
        fields: vec![("name".into(), Data::Text(name.into()))],
    };
    let slots = |name: &str| -> Vec<String> {
        let Some(Ok(Data::List(items))) =
            b.workspaces.dynamic().call(&rt, "slots", &[screen(name)])
        else {
            panic!("slots({name}) is no list");
        };
        items
            .iter()
            .map(
                |w| match strand_services::record_field(w, "Workspace", "name") {
                    Ok(Data::Text(t)) => t.to_string(),
                    other => panic!("{other:?}"),
                },
            )
            .collect()
    };
    assert_eq!(slots("DP-1"), ["1", "2", "4"]);
    assert_eq!(slots("HDMI-A-1"), ["3", "6"]);
    // Gaps 2: `workspaces.focus(4)` makes a slot, `focus("chat")` a new
    // named workspace.
    for (target, sent) in [
        (Data::Int(4), "dispatch workspace 4"),
        (Data::Text("chat".into()), "dispatch workspace name:chat"),
    ] {
        b.workspaces
            .dynamic()
            .action(&rt, "focus", None, &[target], Default::default())
            .unwrap();
        while !fake.requests().iter().any(|r| r == sent) {
            assert!(Instant::now() < deadline, "{:?}", fake.requests());
            s.pump(&rt);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // (services-round) `wm.mode` is the submap; a window's `rect` its
    // place on its monitor; `win.move_to(ws)` a silent move.
    fake.send("submap>>resize\n");
    until(&rt, &s, "the submap", || {
        b.wm.cells()
            .snapshot(&rt)
            .is_ok_and(|w| w.mode.as_deref() == Some("resize"))
    });
    b.windows.acquire(&rt);
    until(&rt, &s, "the windows", || {
        b.windows
            .cells()
            .all
            .get_untracked(&rt)
            .is_ok_and(|v| !v.items().is_empty())
    });
    let kitty = b
        .windows
        .cells()
        .all
        .get_untracked(&rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, w)| w.clone())
        .find(|w| w.app_id == "kitty")
        .unwrap();
    assert_eq!(
        kitty.rect,
        Some(wm::WindowRect {
            x: 10.0,
            y: 52.0,
            width: 1260.0,
            height: 1376.0
        })
    );
    let three = workspace(&b, &rt, "3").unwrap();
    b.windows
        .act(
            &rt,
            WindowAction::MoveTo {
                item: kitty.clone(),
                ws: three,
            },
        )
        .unwrap();
    let sent = format!("dispatch movetoworkspacesilent 3,address:{}", kitty.id);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fake.requests().contains(&sent) {
        assert!(Instant::now() < deadline, "{:?}", fake.requests());
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(10));
    }
    b.windows.release(&rt);
    fake.send(&bursts["reload"]);
    until(&rt, &s, "configreloaded", || {
        reloads.borrow().as_slice() == [vec![Data::Null]]
    });
    assert_eq!(wm::live_runs(), runs + 1);
    // The readers go: 5 s later the stores stop, and the hub's run too.
    b.workspaces.release(&rt);
    b.wm.release(&rt);
    rt.tick(STOP_GRACE + Duration::from_millis(1));
    assert!(!b.workspaces.running() && !b.wm.running() && !b.windows.running());
    let deadline = Instant::now() + Duration::from_secs(5);
    while wm::live_runs() != runs {
        assert!(
            Instant::now() < deadline,
            "the Hyprland run outlived its stores"
        );
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(10));
    }
    s.shutdown();
    wm::configure(None);
}

/// The `windows` store with no IPC adapter (sway's turned off, as on
/// labwc, wayfire or river): `windows.focused` and `win.focus()`/
/// `win.close()` come from `zwlr_foreign_toplevel_management_v1` on the
/// `strand-toplevel` thread.
#[test]
fn the_windows_store_follows_wlr_management_without_an_adapter() {
    let _serial = serial();
    let Some(sway) = Sway::start("windows_store_wlr") else {
        return;
    };
    wm::configure(Some(WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        desktop: Some("labwc".into()),
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "boot", || {
        b.wm.cells().snapshot(&rt).is_ok_and(|w| w.name == "labwc")
    });
    let focused_app = || {
        b.windows
            .cells()
            .snapshot(&rt)
            .ok()
            .and_then(|w| w.focused)
            .map(|f| f.app_id)
    };
    // One at a time: each maps (and takes the focus) on its own thread,
    // so two opened together may map in either order.
    let a = TestWindow::open(&sway.socket(), "strand-a", "alpha");
    until(&rt, &s, "a focused", || {
        focused_app().as_deref() == Some("strand-a")
    });
    let _b = TestWindow::open(&sway.socket(), "strand-b", "beta");
    until(&rt, &s, "b focused", || {
        focused_app().as_deref() == Some("strand-b")
            && b.windows
                .cells()
                .snapshot(&rt)
                .is_ok_and(|w| w.all.len() == 2)
    });
    let item = |app: &str| {
        b.windows
            .cells()
            .snapshot(&rt)
            .unwrap()
            .all
            .into_iter()
            .find(|w| w.app_id == app)
            .unwrap()
    };
    b.windows
        .act(
            &rt,
            WindowAction::Focus {
                item: item("strand-a"),
            },
        )
        .unwrap();
    until(&rt, &s, "a focused", || {
        focused_app().as_deref() == Some("strand-a")
    });
    b.windows
        .act(
            &rt,
            WindowAction::Close {
                item: item("strand-a"),
            },
        )
        .unwrap();
    until(&rt, &s, "a closed", || {
        b.windows
            .cells()
            .snapshot(&rt)
            .is_ok_and(|w| w.all.len() == 1 && w.all[0].app_id == "strand-b")
    });
    assert!(a.closed.load(Ordering::SeqCst));
    s.shutdown();
    wm::configure(None);
}

/// The stores over a Hyprland adapter that stops understanding Hyprland
/// (a `j/clients` of another shape, as a newer release might send), on a
/// display that offers the standard protocols (a headless sway's
/// wlr-foreign-toplevel-management): the windows store switches to the
/// protocol's windows, exactly as with no adapter, one
/// `ServiceDiagnostic` (not an overlay notice) names Hyprland, its
/// version and what was not understood, and once Hyprland is understood
/// again the adapter's windows come back without a restart.
#[test]
fn a_hyprland_the_adapter_cannot_read_falls_back_to_the_protocols_with_a_diagnostic() {
    let _serial = serial();
    let Some(sway) = Sway::start("hyprland_degraded_to_wlr") else {
        return;
    };
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _enter = tokio.enter();
    let fake = common::hyprland::FakeHyprland::start();
    wm::configure(Some(WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let windows = || {
        b.windows
            .cells()
            .snapshot(&rt)
            .map(|w| {
                w.all
                    .into_iter()
                    .map(|w| (w.id, w.app_id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    b.windows.acquire(&rt);
    b.workspaces.acquire(&rt);
    b.wm.acquire(&rt);
    let diagnostics = RefCell::new(Vec::new());
    let until = |what: &str, cond: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            s.pump(&rt);
            rt.flush();
            diagnostics.borrow_mut().extend(s.take_diagnostics());
            if cond() {
                return;
            }
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    until("boot", &|| {
        windows().iter().any(|(id, _)| id == "0x55d0c0a1b2c0")
    });
    let _w = TestWindow::open(&sway.socket(), "strand-fallback", "fallback");

    fake.set_reply(
        "j/clients",
        br#"[{"addr": "0x55d0c0a1c3d0", "mapped": true, "workspace": {"id": 2, "name": "2"}}]"#,
    );
    fake.send("openwindow>>55d0c0a1c3d0,2,foot,foot\n");
    until("the protocol's windows", &|| {
        let w = windows();
        w.iter()
            .any(|(id, app)| id.starts_with("wlr-") && app == "strand-fallback")
            && !w.iter().any(|(id, _)| id.starts_with("0x"))
    });
    assert_eq!(
        b.wm.cells().snapshot(&rt).unwrap().name,
        "Hyprland",
        "wm.name stays the adapter's, as when it cannot connect"
    );
    let diagnostics = diagnostics.borrow().clone();
    let degraded: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("does not understand"))
        .collect();
    assert_eq!(degraded.len(), 1, "one store reports it: {diagnostics:?}");
    let d = degraded[0];
    assert!(!d.notice && !d.resolved, "{d:?}");
    assert!(
        ["windows", "workspaces", "wm"].contains(&d.service.as_str()),
        "{d:?}"
    );
    assert!(
        d.message.contains("Hyprland 0.56.2")
            && d.message.contains("j/clients")
            && d.message.contains("missing field `address`"),
        "{d:?}"
    );

    // Hyprland is understood again: its windows (and ids) are back.
    fake.clear_replies();
    until("the adapter's windows", &|| {
        let w = windows();
        w.iter().any(|(id, _)| id == "0x55d0c0a1b2c0")
            && !w.iter().any(|(id, _)| id.starts_with("wlr-"))
    });
    s.shutdown();
    wm::configure(None);
}

/// Bugs 3 and gaps 9 (m6-w2-services): a window is linked to its desktop
/// entry, `win.app`, and draws the entry's icon: the owner's Chromium
/// (`Chromium-browser`) and Spotify (`Spotify`, XWayland) on Hyprland,
/// in `windows.all` and in their workspaces' copies. An app installed
/// while the shell runs is linked once the entries are read again.
#[test]
fn windows_are_linked_to_their_apps() {
    let _serial = serial();
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _enter = tokio.enter();
    let root = std::env::temp_dir().join(format!("strand-wm-apps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    let icons = root.join("icons/hicolor");
    let write = |p: &std::path::Path, text: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    write(
        &icons.join("index.theme"),
        "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\nType=Fixed\n",
    );
    write(&icons.join("48x48/apps/chromium.png"), "png");
    write(&icons.join("48x48/apps/spotify-client.png"), "png");
    write(
        &dir.join("chromium.desktop"),
        "[Desktop Entry]\nType=Application\nName=Chromium\nExec=chromium %U\n\
         Icon=chromium\nStartupWMClass=chromium\nCategories=Network;WebBrowser;\n",
    );
    strand_icons::set_base_dirs(Some(vec![root.join("icons")]));
    strand_services::apps::set_config(Some(strand_services::apps::Config {
        dirs: vec![dir.clone()],
        ..Default::default()
    }));
    let fake = common::hyprland::FakeHyprland::start();
    let client = |address: &str, ws: i64, class: &str, xwayland: bool| {
        serde_json::json!({
            "address": address, "mapped": true, "hidden": false,
            "workspace": {"id": ws, "name": ws.to_string()},
            "class": class, "title": class, "xwayland": xwayland,
            "fullscreen": 0, "stableId": address.trim_start_matches("0x"),
        })
    };
    let clients = serde_json::json!([
        client("0xa1", 1, "Chromium-browser", false),
        client("0xb2", 2, "Spotify", true),
    ]);
    fake.set_reply("j/clients", clients.to_string().as_bytes());
    wm::configure(Some(WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.windows.acquire(&rt);
    b.workspaces.acquire(&rt);
    let window = |class: &str| {
        b.windows
            .cells()
            .snapshot(&rt)
            .ok()?
            .all
            .into_iter()
            .find(|w| w.app_id == class)
    };
    until(&rt, &s, "the windows", || {
        window("Chromium-browser").is_some() && window("Spotify").is_some()
    });
    let chromium = window("Chromium-browser").unwrap();
    assert_eq!(chromium.icon, "chromium");
    let app = chromium.app.clone().unwrap();
    assert_eq!(app.id, "chromium");
    assert!(app.categories.contains(&"WebBrowser".to_string()));
    // Not installed: the app id as it is, no app.
    let spotify = window("Spotify").unwrap();
    assert_eq!(
        (spotify.icon.as_str(), spotify.app.as_ref()),
        ("Spotify", None)
    );
    // The workspace's copy says the same.
    until(&rt, &s, "workspace 1's copy", || {
        workspace(&b, &rt, "1").is_some_and(|w| w.windows.first() == Some(&chromium))
    });

    // Spotify is installed while the shell runs: its window is linked.
    std::fs::write(
        dir.join("spotify.desktop"),
        "[Desktop Entry]\nType=Application\nName=Spotify\nExec=spotify %U\n\
         Icon=spotify-client\nCategories=Audio;Music;Player;AudioVideo;\n",
    )
    .unwrap();
    strand_services::apps::changed();
    until(&rt, &s, "Spotify linked", || {
        window("Spotify").is_some_and(|w| {
            w.icon == "spotify-client" && w.app.as_ref().is_some_and(|a| a.id == "spotify")
        })
    });
    s.shutdown();
    wm::configure(None);
    common::no_desktop_entries();
    let _ = std::fs::remove_dir_all(&root);
}

/// Review minor 10 (m6-w2-services): the compositor's state does not wait
/// for the desktop entries. A FIFO among them holds the read open: the
/// windows are published with their app ids meanwhile, and patched with
/// their entries' icons and apps once the read ends.
#[test]
fn windows_are_published_before_the_desktop_entries_are_read() {
    use std::os::unix::fs::OpenOptionsExt;
    /// Ends the held read whatever happens (a writer opened and closed
    /// without blocking: the reader sees the end of the file).
    struct Release(std::path::PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            // O_NONBLOCK: fails (no reader) rather than waits.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(0o4000)
                .open(&self.0);
        }
    }
    let _serial = serial();
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _enter = tokio.enter();
    let root = std::env::temp_dir().join(format!("strand-wm-slow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("chromium.desktop"),
        "[Desktop Entry]\nType=Application\nName=Chromium\nExec=chromium\nIcon=chromium\n",
    )
    .unwrap();
    let fifo = dir.join("zz-slow.desktop");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let release = Release(fifo.clone());
    let theme = root.join("icons/hicolor");
    std::fs::create_dir_all(theme.join("48x48/apps")).unwrap();
    std::fs::write(
        theme.join("index.theme"),
        "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\nType=Fixed\n",
    )
    .unwrap();
    std::fs::write(theme.join("48x48/apps/chromium.png"), "png").unwrap();
    strand_icons::set_base_dirs(Some(vec![root.join("icons")]));
    strand_services::apps::set_config(Some(strand_services::apps::Config {
        dirs: vec![dir.clone()],
        ..Default::default()
    }));
    let fake = common::hyprland::FakeHyprland::start();
    let clients = serde_json::json!([{
        "address": "0xa1", "mapped": true, "hidden": false,
        "workspace": {"id": 1, "name": "1"},
        "class": "Chromium-browser", "title": "Chromium", "xwayland": false,
        "fullscreen": 0, "stableId": "a1",
    }]);
    fake.set_reply("j/clients", clients.to_string().as_bytes());
    wm::configure(Some(WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.windows.acquire(&rt);
    let window = || {
        b.windows
            .cells()
            .snapshot(&rt)
            .ok()?
            .all
            .into_iter()
            .find(|w| w.app_id == "Chromium-browser")
    };
    until(&rt, &s, "the window, while the entries are read", || {
        window().is_some_and(|w| w.icon == "Chromium-browser" && w.app.is_none())
    });
    drop(release);
    until(&rt, &s, "the window linked", || {
        window().is_some_and(|w| {
            w.icon == "chromium" && w.app.as_ref().is_some_and(|a| a.id == "chromium")
        })
    });
    s.shutdown();
    wm::configure(None);
    common::no_desktop_entries();
    let _ = std::fs::remove_dir_all(&root);
}

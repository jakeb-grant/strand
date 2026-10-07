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

/// The tests configure the process-wide compositor config: one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
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

    // The last readers go: 5 s on the logic clock later every store
    // stops, and with them the compositor service.
    b.workspaces.release(&rt);
    b.windows.release(&rt);
    rt.tick(STOP_GRACE + Duration::from_millis(1));
    assert!(!b.workspaces.running() && !b.windows.running());
    assert!(b.wm.running(), "a store still read keeps the hub");
    assert_eq!(wm::live_runs(), runs + 1);
    b.wm.release(&rt);
    rt.tick(2 * STOP_GRACE + Duration::from_millis(2));
    assert!(!b.wm.running());
    let deadline = Instant::now() + Duration::from_secs(5);
    while wm::live_runs() != runs {
        assert!(
            Instant::now() < deadline,
            "the compositor service outlived its stores"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    s.shutdown();
    wm::configure(None);
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
    fake.send(&bursts["reload"]);
    until(&rt, &s, "configreloaded", || {
        reloads.borrow().as_slice() == [vec![Data::Null]]
    });
    s.shutdown();
    wm::configure(None);
}

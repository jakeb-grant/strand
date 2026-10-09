//! The compositor matrix (M3 exit: "runs on Hyprland, niri and sway"):
//! the `workspaces`, `windows` and `wm` stores, and design.md's bar in
//! `strand run`, against a live compositor that someone else started:
//! `scripts/compositor-matrix.sh` (sway headless; Hyprland on a virtual
//! KMS device; niri nested in headless sway), which the CI job
//! `compositors` runs in an Arch Linux container for all three.
//!
//! What the compositor itself reports (`swaymsg -r`, `hyprctl -j`,
//! `niri msg --json`) is the truth both tests compare against: the
//! workspaces of the output in the compositor's own order, which one is
//! focused and which hold windows, and the focused window. The stores
//! must say exactly that, through switches made from outside, a real
//! window, its title change, `ws.focus()`, the compositor's reload
//! (`wm.config_reloaded`) and `win.close()`; the bar must draw it (one
//! dot per workspace, the focused one the accent pill, occupied dots
//! darker than empty ones, the focused window's title), and a click on
//! a dot must switch the compositor.
//!
//! labwc (`STRAND_MATRIX=labwc`) is the matrix's compositor without an
//! IPC adapter: no CLI reports its state, so the two tests above skip it
//! and `the_stores_follow_a_compositor_without_ipc` runs instead, with the
//! test windows' own `xdg_toplevel` configures (activated or not) and
//! close requests as the truth, and the workspaces labwc reports over
//! `ext-workspace-v1`.
//!
//! Environment: `STRAND_MATRIX` names the compositor (`sway`, `hyprland`
//! or `niri`); `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and its IPC variable
//! (`SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`, `NIRI_SOCKET`) point at
//! it; `STRAND_MATRIX_NIRI_CONFIG` is the config file niri watches (the
//! reload edits it). Without `STRAND_MATRIX` the tests print that they
//! were skipped.
//! Screenshots go to `$STRAND_SHOTS` when it is set.

mod support;

#[allow(dead_code)]
#[path = "../../strand-services/tests/common/window.rs"]
mod window;

use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use strand_core::Runtime;
use strand_services::testing::PrivateBus;
use strand_services::wm::{WindowAction, WorkspaceAction, WorkspaceItem};
use strand_services::{Applied, Builtin, Buses, Cells, Data, Services};
use window::TestWindow;

const FILES: [(&str, &str); 2] = [
    (
        "theme.strand",
        include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
    ),
    (
        "bar.strand",
        include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
    ),
];

/// The bar's vertical middle: 8 px margin, 36 px high.
const BAR_Y: usize = 26;
/// The first workspace dot's left edge: the bar's 8 px margin and the
/// split's 12 px padding.
const DOTS_X: usize = 20;
/// How long a state may take to show.
const PATIENCE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Sway,
    Hyprland,
    Niri,
}

/// The compositor under test, and its output the bar is checked on.
struct Live {
    kind: Kind,
    output: String,
    /// The output's logical size (the virtual pointer's layout).
    w: u32,
    h: u32,
    socket: PathBuf,
}

/// One workspace as the compositor reports it.
#[derive(Clone, Debug, PartialEq)]
struct RealWs {
    name: String,
    focused: bool,
    occupied: bool,
}

/// The compositor's state on the output under test.
#[derive(Clone, Debug, PartialEq)]
struct Real {
    workspaces: Vec<RealWs>,
    /// `(app_id, title)` of the focused window.
    window: Option<(String, String)>,
    /// The focused workspace's name (`workspaces.focused`), on any output.
    focused: Option<String>,
}

/// `STRAND_MATRIX` names a compositor with no IPC adapter (labwc).
fn without_ipc() -> bool {
    std::env::var("STRAND_MATRIX").is_ok_and(|m| m == "labwc")
}

fn live() -> Option<Live> {
    if without_ipc() {
        eprintln!(
            "\n*** labwc has no IPC to compare with: this test is for sway, niri and Hyprland; \
             the_stores_follow_a_compositor_without_ipc covers labwc ***\n"
        );
        return None;
    }
    let kind = match std::env::var("STRAND_MATRIX").ok()?.as_str() {
        "sway" => Kind::Sway,
        "hyprland" => Kind::Hyprland,
        "niri" => Kind::Niri,
        other => panic!("STRAND_MATRIX={other}: sway, hyprland, niri or labwc"),
    };
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let display = std::env::var_os("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY");
    let socket = runtime.join(display);
    let mut live = Live {
        kind,
        output: String::new(),
        w: 0,
        h: 0,
        socket,
    };
    let (output, w, h) = live.output();
    live.output = output;
    live.w = w;
    live.h = h;
    eprintln!(
        "matrix: {kind:?} on {} ({}x{})",
        live.output, live.w, live.h
    );
    Some(live)
}

fn skipped(what: &str) {
    if without_ipc() {
        // `live()` said why.
        eprintln!("{what}: skipped on labwc");
        return;
    }
    eprintln!(
        "\n*** SKIPPED: STRAND_MATRIX is not set; {what} did not run (scripts/compositor-matrix.sh) ***\n"
    );
}

fn run(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json(program: &str, args: &[&str]) -> Value {
    let text = run(program, args);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{program} {args:?}: {e}: {text}"))
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

impl Live {
    /// The output the bar is checked on (the focused one) and its
    /// logical size.
    fn output(&self) -> (String, u32, u32) {
        let logical = |w: &Value, h: &Value, scale: &Value| {
            let s = scale.as_f64().unwrap_or(1.0).max(0.1);
            (
                (w.as_f64().unwrap_or(0.0) / s).round() as u32,
                (h.as_f64().unwrap_or(0.0) / s).round() as u32,
            )
        };
        match self.kind {
            Kind::Sway => {
                let v = json("swaymsg", &["-t", "get_outputs", "-r"]);
                let all = v.as_array().cloned().unwrap_or_default();
                let o = all
                    .iter()
                    .find(|o| o["focused"] == true)
                    .or(all.first())
                    .expect("an output");
                (
                    text(&o["name"]),
                    o["rect"]["width"].as_u64().unwrap_or(0) as u32,
                    o["rect"]["height"].as_u64().unwrap_or(0) as u32,
                )
            }
            Kind::Hyprland => {
                let v = json("hyprctl", &["-j", "monitors"]);
                let all = v.as_array().cloned().unwrap_or_default();
                let o = all
                    .iter()
                    .find(|o| o["focused"] == true)
                    .or(all.first())
                    .expect("a monitor");
                let (w, h) = logical(&o["width"], &o["height"], &o["scale"]);
                (text(&o["name"]), w, h)
            }
            Kind::Niri => {
                let v = json("niri", &["msg", "--json", "outputs"]);
                let o = v
                    .as_object()
                    .and_then(|m| m.values().next())
                    .expect("an output");
                let l = &o["logical"];
                (
                    text(&o["name"]),
                    l["width"].as_u64().unwrap_or(0) as u32,
                    l["height"].as_u64().unwrap_or(0) as u32,
                )
            }
        }
    }

    /// The compositor's own report, for the output under test.
    fn real(&self) -> Real {
        match self.kind {
            Kind::Sway => {
                let tree = json("swaymsg", &["-t", "get_tree", "-r"]);
                let mut workspaces = Vec::new();
                let mut window = None;
                fn walk(n: &Value, count: &mut usize, window: &mut Option<(String, String)>) {
                    let leaf = n["pid"].is_number();
                    if leaf {
                        *count += 1;
                        if n["focused"] == true {
                            *window = Some((text(&n["app_id"]), text(&n["name"])));
                        }
                    }
                    for key in ["nodes", "floating_nodes"] {
                        for c in n[key].as_array().into_iter().flatten() {
                            walk(c, count, window);
                        }
                    }
                }
                let focused_ws = json("swaymsg", &["-t", "get_workspaces", "-r"])
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|w| w["focused"] == true)
                    .map(|w| text(&w["name"]));
                for o in tree["nodes"].as_array().into_iter().flatten() {
                    for ws in o["nodes"].as_array().into_iter().flatten() {
                        if ws["type"] != "workspace" {
                            continue;
                        }
                        let mut count = 0;
                        walk(ws, &mut count, &mut window);
                        if text(&o["name"]) != self.output {
                            continue;
                        }
                        let name = text(&ws["name"]);
                        workspaces.push((
                            ws["num"].as_i64().unwrap_or(i64::MAX),
                            RealWs {
                                focused: focused_ws.as_deref() == Some(name.as_str()),
                                name,
                                occupied: count > 0,
                            },
                        ));
                    }
                }
                workspaces.sort_by_key(|(num, _)| *num);
                Real {
                    workspaces: workspaces.into_iter().map(|(_, w)| w).collect(),
                    window,
                    focused: focused_ws,
                }
            }
            Kind::Hyprland => {
                let all = json("hyprctl", &["-j", "workspaces"]);
                let active = json("hyprctl", &["-j", "activeworkspace"]);
                let win = json("hyprctl", &["-j", "activewindow"]);
                let mut workspaces: Vec<(i64, RealWs)> = all
                    .as_array()
                    .into_iter()
                    .flatten()
                    // Special workspaces (`special:…`, negative ids) are
                    // not listed; named ones (negative ids too) are.
                    .filter(|w| {
                        let name = text(&w["name"]);
                        !(name == "special" || name.starts_with("special:"))
                    })
                    .filter(|w| text(&w["monitor"]) == self.output)
                    .map(|w| {
                        (
                            w["id"].as_i64().unwrap_or(0),
                            RealWs {
                                name: text(&w["name"]),
                                focused: w["id"] == active["id"],
                                occupied: w["windows"].as_u64().unwrap_or(0) > 0,
                            },
                        )
                    })
                    .collect();
                // Numbered ones by number, then named ones as created.
                workspaces.sort_by_key(|(id, _)| (*id < 0, id.unsigned_abs()));
                let window = win
                    .get("address")
                    .is_some()
                    .then(|| (text(&win["class"]), text(&win["title"])));
                Real {
                    workspaces: workspaces.into_iter().map(|(_, w)| w).collect(),
                    window,
                    focused: active.get("name").map(text),
                }
            }
            Kind::Niri => {
                let all = json("niri", &["msg", "--json", "workspaces"]);
                let win = json("niri", &["msg", "--json", "focused-window"]);
                let name = |w: &Value| {
                    w["name"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| w["idx"].as_u64().unwrap_or(0).to_string())
                };
                let focused = all
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|w| w["is_focused"] == true)
                    .map(name);
                let mut workspaces: Vec<(u64, RealWs)> = all
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|w| text(&w["output"]) == self.output)
                    .map(|w| {
                        (
                            w["idx"].as_u64().unwrap_or(0),
                            RealWs {
                                name: name(w),
                                focused: w["is_focused"] == true,
                                occupied: !w["active_window_id"].is_null(),
                            },
                        )
                    })
                    .collect();
                workspaces.sort_by_key(|(idx, _)| *idx);
                let window = (!win.is_null()).then(|| (text(&win["app_id"]), text(&win["title"])));
                Real {
                    workspaces: workspaces.into_iter().map(|(_, w)| w).collect(),
                    window,
                    focused,
                }
            }
        }
    }

    /// Switches to the workspace named `name` from outside (the
    /// compositor's own CLI).
    fn switch(&self, name: &str) {
        match self.kind {
            Kind::Sway => drop(run("swaymsg", &["workspace", name])),
            Kind::Hyprland => {
                // Classic first; with a Lua config (0.55 on) a dispatch is
                // a dispatcher object.
                let classic = Command::new("hyprctl")
                    .args(["dispatch", "workspace", name])
                    .output()
                    .unwrap_or_else(|e| panic!("hyprctl: {e}"));
                let reply = String::from_utf8_lossy(&classic.stdout);
                if !classic.status.success() || reply.trim_start().starts_with("error") {
                    let lua = format!("hl.dsp.focus({{ workspace = \"{name}\" }})");
                    let out = run("hyprctl", &["dispatch", &lua]);
                    assert!(!out.trim_start().starts_with("error"), "{out}");
                }
            }
            Kind::Niri => drop(run("niri", &["msg", "action", "focus-workspace", name])),
        }
    }

    /// Makes the compositor reload its configuration.
    fn reload(&self) {
        match self.kind {
            Kind::Sway => drop(run("swaymsg", &["reload"])),
            Kind::Hyprland => drop(run("hyprctl", &["reload"])),
            // niri reloads a config file it watches when the file
            // changes (every version); `load-config-file` is newer.
            Kind::Niri => match std::env::var_os("STRAND_MATRIX_NIRI_CONFIG") {
                Some(path) => {
                    let mut text = std::fs::read_to_string(&path).unwrap();
                    text.push_str("// reloaded by the compositor matrix\n");
                    std::fs::write(&path, text).unwrap();
                }
                None => drop(run("niri", &["msg", "action", "load-config-file"])),
            },
        }
    }

    /// `wm.name` for it.
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Sway => "sway",
            Kind::Hyprland => "Hyprland",
            Kind::Niri => "niri",
        }
    }

    /// The workspace to switch to that is not the focused one: the
    /// first empty one after it (niri always keeps one; sway and
    /// Hyprland make one by switching to it).
    fn other(&self, real: &Real) -> String {
        if self.kind == Kind::Niri {
            let empty = real
                .workspaces
                .iter()
                .find(|w| !w.occupied && !w.focused)
                .expect("niri's empty workspace");
            return empty.name.clone();
        }
        let mut n = 2;
        while real.workspaces.iter().any(|w| w.name == n.to_string()) {
            n += 1;
        }
        n.to_string()
    }
}

impl fmt::Display for Real {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for w in &self.workspaces {
            write!(
                f,
                "[{}{}{}] ",
                w.name,
                if w.focused { " focused" } else { "" },
                if w.occupied { " occupied" } else { "" }
            )?;
        }
        write!(
            f,
            "window: {:?} focused workspace: {:?}",
            self.window, self.focused
        )
    }
}

/// Polls `f` until it is `Ok`, at most [`PATIENCE`]; the last `Err`
/// explains a timeout.
fn until(what: &str, mut f: impl FnMut() -> Result<(), String>) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let r = f();
        match r {
            Ok(()) => return,
            Err(e) if Instant::now() >= deadline => panic!("never: {what}\n{e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

// ---- the stores ----------------------------------------------------------

/// What the stores say about the output under test, in their order.
fn stored(b: &Builtin, rt: &Runtime, live: &Live) -> Result<Real, String> {
    let ws = b
        .workspaces
        .cells()
        .snapshot(rt)
        .map_err(|e| format!("{e:?}"))?;
    let win = b
        .windows
        .cells()
        .snapshot(rt)
        .map_err(|e| format!("{e:?}"))?;
    Ok(Real {
        workspaces: ws
            .all
            .iter()
            .filter(|w| w.screen == live.output)
            .map(|w| RealWs {
                name: w.name.clone(),
                focused: w.focused,
                occupied: w.occupied,
            })
            .collect(),
        window: win.focused.map(|w| (w.app_id, w.title)),
        focused: ws.focused.map(|w| w.name),
    })
}

/// The stores agree with the compositor (polled: either may lag).
fn agree(rt: &Runtime, s: &Services, b: &Builtin, live: &Live, what: &str) -> Real {
    let mut last = None;
    until(what, || {
        s.pump(rt);
        rt.flush();
        let real = live.real();
        let ours = stored(b, rt, live)?;
        last = Some(real.clone());
        if ours == real {
            Ok(())
        } else {
            Err(format!("compositor: {real}\nstores:     {ours}"))
        }
    });
    let real = last.unwrap_or_else(|| live.real());
    eprintln!("matrix: {what}: {real}");
    real
}

fn workspace_named(b: &Builtin, rt: &Runtime, name: &str) -> WorkspaceItem {
    let s = b.workspaces.cells().snapshot(rt).unwrap();
    s.all.into_iter().find(|w| w.name == name).unwrap()
}

#[test]
fn the_stores_report_the_live_compositor() {
    let Some(live) = live() else {
        skipped("the live compositor stores test");
        return;
    };
    let rt = Runtime::new();
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    // The real detection path (`WmConfig::from_env`): no `configure`.
    let s = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let b = Builtin::register(&s, &rt);
    let events: Rc<RefCell<Vec<Applied>>> = Rc::default();
    let e = events.clone();
    b.wm.dynamic()
        .observe(Box::new(move |_, x| e.borrow_mut().push(x.clone())));
    b.workspaces.acquire(&rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until("wm.name", || {
        s.pump(&rt);
        rt.flush();
        let name = b.wm.cells().snapshot(&rt).map(|w| w.name);
        match name {
            Ok(n) if n == live.name() => Ok(()),
            other => Err(format!("{other:?}")),
        }
    });
    let boot = agree(&rt, &s, &b, &live, "boot");
    assert!(boot.workspaces.iter().any(|w| w.focused), "{boot}");

    // A real window on the focused workspace.
    let win = TestWindow::open(&live.socket, "strand-matrix", "matrix one");
    let real = agree(&rt, &s, &b, &live, "a window");
    assert_eq!(
        real.window,
        Some(("strand-matrix".into(), "matrix one".into()))
    );
    let home = real
        .workspaces
        .iter()
        .find(|w| w.focused)
        .expect("a focused workspace")
        .name
        .clone();
    let window = b
        .windows
        .cells()
        .snapshot(&rt)
        .unwrap()
        .all
        .into_iter()
        .find(|w| w.app_id == "strand-matrix")
        .expect("the window in windows.all");
    assert_eq!(
        window.workspace,
        Some(workspace_named(&b, &rt, &home).id),
        "the window's workspace"
    );

    // Its title changes.
    win.set_title("matrix two");
    let real = agree(&rt, &s, &b, &live, "the title");
    assert_eq!(
        real.window,
        Some(("strand-matrix".into(), "matrix two".into()))
    );

    // `win.focus()`: a second window takes the focus, the first gets it
    // back through the store.
    let second = TestWindow::open(&live.socket, "strand-matrix-two", "matrix three");
    let real = agree(&rt, &s, &b, &live, "a second window");
    assert_eq!(
        real.window,
        Some(("strand-matrix-two".into(), "matrix three".into()))
    );
    let first = b
        .windows
        .cells()
        .snapshot(&rt)
        .unwrap()
        .all
        .into_iter()
        .find(|w| w.app_id == "strand-matrix")
        .expect("the first window");
    b.windows
        .act(&rt, WindowAction::Focus { item: first })
        .unwrap();
    let real = agree(&rt, &s, &b, &live, "win.focus()");
    assert_eq!(
        real.window,
        Some(("strand-matrix".into(), "matrix two".into()))
    );
    drop(second);
    let real = agree(&rt, &s, &b, &live, "the second window gone");
    assert_eq!(
        real.window,
        Some(("strand-matrix".into(), "matrix two".into()))
    );

    // A switch from outside, to an empty workspace.
    let other = live.other(&real);
    live.switch(&other);
    let real = agree(&rt, &s, &b, &live, "switched from outside");
    assert!(
        real.workspaces.iter().any(|w| w.name == other && w.focused),
        "{real}"
    );
    assert_eq!(real.window, None);

    // `ws.focus()` back to the window's workspace.
    let item = workspace_named(&b, &rt, &home);
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item })
        .unwrap();
    let real = agree(&rt, &s, &b, &live, "ws.focus()");
    assert!(
        real.workspaces.iter().any(|w| w.name == home && w.focused),
        "{real}"
    );

    // Hyprland: a named workspace (a negative id) is focused by
    // `name:<name>`, in the Lua dialect too (`hl.dsp.focus({ workspace =
    // "name:matrix" })`): one made from outside, holding a window so it
    // lives on, then `ws.focus()` away from it and back.
    if live.kind == Kind::Hyprland {
        live.switch("name:matrix");
        let named = TestWindow::open(&live.socket, "strand-matrix-named", "matrix named");
        let real = agree(&rt, &s, &b, &live, "a named workspace");
        assert!(
            real.workspaces
                .iter()
                .any(|w| w.name == "matrix" && w.focused && w.occupied),
            "{real}"
        );
        let item = workspace_named(&b, &rt, &home);
        b.workspaces
            .act(&rt, WorkspaceAction::Focus { item })
            .unwrap();
        agree(&rt, &s, &b, &live, "ws.focus() off the named workspace");
        let item = workspace_named(&b, &rt, "matrix");
        b.workspaces
            .act(&rt, WorkspaceAction::Focus { item })
            .unwrap();
        let real = agree(&rt, &s, &b, &live, "ws.focus() on the named workspace");
        assert!(
            real.workspaces
                .iter()
                .any(|w| w.name == "matrix" && w.focused),
            "{real}"
        );
        drop(named);
        let item = workspace_named(&b, &rt, &home);
        b.workspaces
            .act(&rt, WorkspaceAction::Focus { item })
            .unwrap();
        let real = agree(&rt, &s, &b, &live, "back from the named workspace");
        assert!(
            real.workspaces.iter().any(|w| w.name == home && w.focused),
            "{real}"
        );
    }

    // The compositor's reload is `wm.config_reloaded`.
    events.borrow_mut().clear();
    live.reload();
    until("wm.config_reloaded", || {
        s.pump(&rt);
        rt.flush();
        let heard = events
            .borrow()
            .iter()
            .any(|a| matches!(a, Applied::Event { event: 0, .. }));
        if heard {
            Ok(())
        } else {
            Err(format!("wm events: {:?}", events.borrow()))
        }
    });
    // `failed`: niri says the config loaded; Hyprland and sway do not say.
    let failed = match live.kind {
        Kind::Niri => Data::Bool(false),
        Kind::Sway | Kind::Hyprland => Data::Null,
    };
    for e in events.borrow().iter() {
        if let Applied::Event { event: 0, args } = e {
            assert_eq!(
                args,
                std::slice::from_ref(&failed),
                "config_reloaded's failed"
            );
        }
    }

    // `win.close()`: the compositor asks the client.
    let window = b
        .windows
        .cells()
        .snapshot(&rt)
        .unwrap()
        .all
        .into_iter()
        .find(|w| w.app_id == "strand-matrix")
        .expect("the window");
    b.windows
        .act(&rt, WindowAction::Close { item: window })
        .unwrap();
    until("the window closed", || {
        if win.closed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("not asked to close".into())
        }
    });
    drop(win);
    let real = agree(&rt, &s, &b, &live, "closed");
    assert_eq!(real.window, None);

    // Nothing changes: nothing wakes the logic thread.
    std::thread::sleep(Duration::from_millis(500));
    s.pump(&rt);
    let before = wakes.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        before,
        "an idle {:?} woke the host",
        live.kind
    );
    s.shutdown();
}

/// A compositor without an IPC adapter (labwc): the standard protocols
/// alone. `windows.focused` and the window actions come from
/// `zwlr_foreign_toplevel_management_v1`, workspaces and `ws.focus()`
/// from `ext-workspace-v1`. The truth is each test window's own view (the
/// compositor's `xdg_toplevel` configures say whether it is activated;
/// `close` reaches it) and the workspace state labwc sends.
#[test]
fn the_stores_follow_a_compositor_without_ipc() {
    if !without_ipc() {
        if std::env::var_os("STRAND_MATRIX").is_some() {
            eprintln!("the no-IPC stores test runs on labwc only");
        } else {
            skipped("the no-IPC stores test");
        }
        return;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let socket = runtime.join(std::env::var_os("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY"));
    let rt = Runtime::new();
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    // The real detection path (`WmConfig::from_env`): no adapter is found.
    let s = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let b = Builtin::register(&s, &rt);
    b.workspaces.acquire(&rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    let pump = || {
        s.pump(&rt);
        rt.flush();
    };
    until("wm.name", || {
        pump();
        match b.wm.cells().snapshot(&rt).map(|w| w.name) {
            Ok(n) if n == "labwc" => Ok(()),
            other => Err(format!("{other:?}")),
        }
    });
    let focused = || -> Option<(String, String)> {
        let w = b.windows.cells().snapshot(&rt).ok()?;
        w.focused.map(|f| (f.app_id, f.title))
    };
    let window = |app: &str| {
        b.windows
            .cells()
            .snapshot(&rt)
            .unwrap()
            .all
            .into_iter()
            .find(|w| w.app_id == app)
            .unwrap_or_else(|| panic!("{app} in windows.all"))
    };
    // `windows.focused` is the window the compositor activated, and only
    // it (the client's own view).
    let focus_is = |what: &str, wins: &[(&TestWindow, &str, &str)], app: &str| {
        until(what, || {
            pump();
            let f = focused();
            let activated: Vec<&str> = wins
                .iter()
                .filter(|(w, _, _)| w.activated.load(Ordering::SeqCst))
                .map(|(_, a, _)| *a)
                .collect();
            let title = wins.iter().find(|(_, a, _)| *a == app).map(|(_, _, t)| *t);
            if f.as_ref().map(|(a, t)| (a.as_str(), t.as_str())) == title.map(|t| (app, t))
                && activated == [app]
            {
                Ok(())
            } else {
                Err(format!(
                    "windows.focused {f:?}; activated by the compositor: {activated:?}"
                ))
            }
        });
        eprintln!("matrix: {what}: {app} focused");
    };

    let first = TestWindow::open(&socket, "strand-matrix", "matrix one");
    focus_is(
        "a window",
        &[(&first, "strand-matrix", "matrix one")],
        "strand-matrix",
    );
    first.set_title("matrix two");
    focus_is(
        "the title",
        &[(&first, "strand-matrix", "matrix two")],
        "strand-matrix",
    );
    let second = TestWindow::open(&socket, "strand-matrix-two", "matrix three");
    let both = [
        (&first, "strand-matrix", "matrix two"),
        (&second, "strand-matrix-two", "matrix three"),
    ];
    focus_is("a second window", &both, "strand-matrix-two");
    assert!(
        window("strand-matrix").workspace.is_none(),
        "the protocols place no window"
    );

    // `win.focus()`: the first window back, through the store.
    b.windows
        .act(
            &rt,
            WindowAction::Focus {
                item: window("strand-matrix"),
            },
        )
        .unwrap();
    focus_is("win.focus()", &both, "strand-matrix");

    // Workspaces over ext-workspace-v1 (labwc's `desktops` in rc.xml): one
    // output, so the one active workspace is focused; `ws.focus()` to the
    // other and back.
    let workspaces = || b.workspaces.cells().snapshot(&rt).unwrap();
    until("two workspaces", || {
        pump();
        let ws = workspaces();
        if ws.all.len() >= 2 && ws.focused.is_some() {
            Ok(())
        } else {
            Err(format!("{:?}", ws.all))
        }
    });
    let home = workspaces().focused.unwrap();
    let other = workspaces()
        .all
        .into_iter()
        .find(|w| w.id != home.id)
        .unwrap();
    b.workspaces
        .act(
            &rt,
            WorkspaceAction::Focus {
                item: other.clone(),
            },
        )
        .unwrap();
    until("ws.focus()", || {
        pump();
        let ws = workspaces();
        match ws.focused {
            Some(f) if f.id == other.id && f.active => Ok(()),
            f => Err(format!("focused {f:?}")),
        }
    });
    eprintln!("matrix: ws.focus(): {} focused", other.name);
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item: home.clone() })
        .unwrap();
    until("ws.focus() back", || {
        pump();
        match workspaces().focused {
            Some(f) if f.id == home.id => Ok(()),
            f => Err(format!("focused {f:?}")),
        }
    });

    // `win.close()`: the compositor asks the client.
    b.windows
        .act(
            &rt,
            WindowAction::Close {
                item: window("strand-matrix-two"),
            },
        )
        .unwrap();
    until("the window closed", || {
        pump();
        let gone = b
            .windows
            .cells()
            .snapshot(&rt)
            .is_ok_and(|w| w.all.iter().all(|x| x.app_id != "strand-matrix-two"));
        if second.closed.load(Ordering::SeqCst) && gone {
            Ok(())
        } else {
            Err(format!(
                "asked: {}; gone from the store: {gone}",
                second.closed.load(Ordering::SeqCst)
            ))
        }
    });
    drop(second);

    // Nothing changes: nothing wakes the logic thread.
    std::thread::sleep(Duration::from_millis(500));
    s.pump(&rt);
    let before = wakes.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        before,
        "an idle labwc woke the host"
    );
    drop(first);
    s.shutdown();
}

// ---- the bar ---------------------------------------------------------------

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Img {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Img {
    fn ppm(ppm: &[u8]) -> Option<Img> {
        let mut nl = ppm.iter().enumerate().filter(|(_, b)| **b == b'\n');
        let (a, b, c) = (nl.next()?.0, nl.next()?.0, nl.next()?.0);
        let dims = std::str::from_utf8(&ppm[a + 1..b]).ok()?;
        let mut it = dims.split_whitespace().map(|v| v.parse::<usize>().ok());
        let (w, h) = (it.next()??, it.next()??);
        Some(Img {
            w,
            h,
            rgb: ppm.get(c + 1..c + 1 + w * h * 3)?.to_vec(),
        })
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        if x >= self.w || y >= self.h {
            return [0; 3];
        }
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

fn dist(a: [u8; 3], b: [u8; 3]) -> i32 {
    (0..3).map(|i| (a[i] as i32 - b[i] as i32).abs()).sum()
}

/// The accent: clearly blue (the theme's accent on the light surface).
fn blue(p: [u8; 3]) -> bool {
    p[2] as i32 - p[0] as i32 > 40 && p[2] > 110
}

/// One workspace as the bar draws it.
#[derive(Clone, Debug, PartialEq)]
struct Drawn {
    focused: bool,
    occupied: bool,
}

/// The bar as drawn: its dots (left to right), whether a title follows
/// them, and each dot's left edge; `Err` while a dot is between sizes
/// (entering) or the dots cannot be read.
fn drawn(img: &Img) -> Result<(Vec<Drawn>, bool, Vec<usize>), String> {
    let bg = img.px(DOTS_X - 4, BAR_Y);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    let mut x = DOTS_X - 2;
    loop {
        let on = dist(img.px(x, BAR_Y), bg) > 30;
        match (on, start) {
            (true, None) => start = Some(x),
            (false, Some(s)) => {
                runs.push((s, x));
                start = None;
            }
            (false, None) => {
                // More than the dots' 4 px gap since the last one: the
                // row ended.
                let last = runs.last().map_or(DOTS_X, |r| r.1);
                if x > last + 8 {
                    break;
                }
            }
            _ => {}
        }
        x += 1;
        if x > DOTS_X + 600 {
            return Err("no end to the dots".into());
        }
    }
    let mut dots = Vec::new();
    // Empty dots are $fg.alpha(0.25), occupied ones $fg.muted
    // ($fg.alpha(0.65)): further from the bar's background.
    let mut contrast = Vec::new();
    for &(a, b) in &runs {
        let w = b - a;
        let centre = img.px((a + b) / 2, BAR_Y);
        match w {
            6..=11 => {
                dots.push(Drawn {
                    focused: false,
                    occupied: false,
                });
                contrast.push(Some(dist(centre, bg)));
            }
            19..=29 if blue(centre) => {
                dots.push(Drawn {
                    focused: true,
                    occupied: false,
                });
                contrast.push(None);
            }
            _ => return Err(format!("a dot {w} px wide at x {a}: {runs:?}")),
        }
    }
    if contrast.iter().any(Option::is_some) {
        // A dot's contrast over the full `$fg`'s is its alpha: 0.25 empty,
        // 0.65 occupied, cut midway. `$fg` is the clock's ink: the pixel
        // of the centred clock furthest from the background.
        let mut ink = 0;
        for y in BAR_Y - 8..BAR_Y + 8 {
            for x in img.w / 2 - 120..img.w / 2 + 120 {
                let p = img.px(x, y);
                if !blue(p) {
                    ink = ink.max(dist(p, bg));
                }
            }
        }
        if ink < 150 {
            return Err(format!("no clock ink to weigh the dots by ({ink})"));
        }
        for (d, c) in dots.iter_mut().zip(&contrast) {
            if let Some(c) = c {
                d.occupied = *c as f32 / ink as f32 > 0.45;
            }
        }
    }
    // A title: ink right of the dots (after the 12 px gap).
    let from = runs.last().map_or(DOTS_X, |r| r.1) + 6;
    let mut ink = 0;
    for y in BAR_Y - 8..BAR_Y + 8 {
        for x in from..from + 160 {
            if dist(img.px(x, y), img.px(from - 3, BAR_Y)) > 60 {
                ink += 1;
            }
        }
    }
    Ok((dots, ink > 30, runs.iter().map(|r| r.0).collect()))
}

/// What the bar should draw for `real`. A focused dot's occupancy is not
/// drawn (the pill is the accent either way).
fn expected(real: &Real) -> (Vec<Drawn>, bool) {
    (
        real.workspaces
            .iter()
            .map(|w| Drawn {
                focused: w.focused,
                occupied: w.occupied && !w.focused,
            })
            .collect(),
        real.window.as_ref().is_some_and(|(_, t)| !t.is_empty()),
    )
}

struct Bar<'a> {
    live: &'a Live,
    dir: PathBuf,
    log: PathBuf,
    shots: Option<PathBuf>,
    n: u32,
    strand: Proc,
}

impl Bar<'_> {
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn shot(&self) -> Option<Img> {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", &self.live.output])
            .arg(&path)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
        Img::ppm(&std::fs::read(&path).ok()?)
    }

    fn keep(&mut self, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        self.n += 1;
        let _ = std::fs::create_dir_all(dir);
        let kind = format!("{:?}", self.live.kind).to_lowercase();
        let _ = Command::new("grim")
            .args(["-t", "png", "-o", &self.live.output])
            .arg(dir.join(format!("matrix-{kind}-{:02}-{name}.png", self.n)))
            .status();
    }

    /// The bar draws the compositor's state (twice in a row, so a dot
    /// mid-animation does not count): that state and the dots' left
    /// edges.
    fn shows(&mut self, what: &str) -> (Real, Vec<usize>) {
        let mut seen = 0;
        let mut last = None;
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Ok(Some(status)) = self.strand.0.try_wait() {
                panic!("strand exited ({status}): {}", self.log_text());
            }
            let real = self.live.real();
            let want = expected(&real);
            let got = match self.shot() {
                Some(img) => drawn(&img),
                None => Err("grim failed".into()),
            };
            match got {
                Ok((dots, title, xs)) if (&dots, title) == (&want.0, want.1) => {
                    seen += 1;
                    if seen == 2 {
                        eprintln!("matrix: the bar shows {what}: {real}");
                        self.keep(what);
                        return (real, xs);
                    }
                }
                other => {
                    seen = 0;
                    last = Some(format!(
                        "compositor: {real}\nwant: {want:?}\ndrawn: {other:?}"
                    ));
                }
            }
            if Instant::now() >= deadline {
                self.keep(&format!("{what}-timeout"));
                panic!(
                    "never: the bar shows {what}\n{}\n{}",
                    last.unwrap_or_default(),
                    self.log_text()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Whether the compositor offers `interface`.
/// Whether the bar's click must be tested (`STRAND_MATRIX_REQUIRE_CLICK`
/// set and not `0`, as in CI): without a virtual pointer the test fails
/// instead of skipping the click.
fn click_required() -> bool {
    std::env::var("STRAND_MATRIX_REQUIRE_CLICK").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn offers(live: &Live, interface: &str) -> bool {
    use wayland_client::Connection;
    use wayland_client::globals::registry_queue_init;
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&live.socket) else {
        return false;
    };
    let Ok(conn) = Connection::from_socket(stream) else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<support::pointer::Client>(&conn) else {
        return false;
    };
    globals
        .contents()
        .with_list(|l| l.iter().any(|g| g.interface == interface))
}

#[test]
fn the_design_bar_shows_the_live_compositor() {
    let Some(live) = live() else {
        skipped("the live compositor bar test");
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-matrix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    for (name, text) in FILES {
        std::fs::write(config.join(name), text).unwrap();
    }
    // The bar's other services (the tray, the portal) on a bus of their
    // own; battery and audio have no daemon here and stay at their
    // defaults.
    let bus = PrivateBus::start();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
    cmd.arg("run")
        .arg(&config)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PIPEWIRE_RUNTIME_DIR", dir.join("no-pipewire"))
        .env_remove("PIPEWIRE_REMOTE")
        .env_remove("STRAND_MOCK")
        .stdin(Stdio::null());
    match &bus {
        Some(bus) => drop(cmd.envs(bus.env())),
        None => drop(
            cmd.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
                .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent"),
        ),
    }
    let log = dir.join("strand.log");
    cmd.stderr(std::fs::File::create(&log).unwrap());

    // A window first: its workspace occupied and focused, its title in
    // the bar.
    let _window = TestWindow::open(&live.socket, "strand-matrix", "a matrix window");
    until("the window", || {
        let real = live.real();
        if real.window.is_some() {
            Ok(())
        } else {
            Err(format!("{real}"))
        }
    });
    let mut bar = Bar {
        live: &live,
        dir: dir.clone(),
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: 0,
        strand: Proc(cmd.spawn().unwrap()),
    };
    let (real, _) = bar.shows("a-window");
    let home_ws = real
        .workspaces
        .iter()
        .position(|w| w.focused)
        .expect("a focused workspace");

    // A switch from outside: the pill moves, the title goes.
    let other = live.other(&real);
    live.switch(&other);
    let (real, xs) = bar.shows("switched");
    assert!(real.window.is_none(), "{real}");

    // A click on the window's workspace's dot switches the compositor
    // (`ws.focus()`), when it has a virtual pointer to click with.
    if offers(&live, "zwlr_virtual_pointer_manager_v1") {
        let x = xs[home_ws] + 4;
        let mut pointer = support::pointer::Pointer::new(&live.socket);
        // (A new virtual pointer's first button may reach no surface:
        // one on the desktop first.)
        let (w, h) = (live.w, live.h);
        pointer.click(w / 2, h - 40, w, h);
        std::thread::sleep(Duration::from_millis(200));
        pointer.click(x as u32, BAR_Y as u32, w, h);
        pointer.motion(w / 2, h - 40, w, h);
        let (real, _) = bar.shows("clicked");
        assert!(real.workspaces[home_ws].focused, "{real}");
    } else {
        // CI (scripts/compositor-matrix-ci.sh) requires the click: a
        // compositor that stops offering the protocol fails there.
        let msg = format!(
            "matrix: {:?} offers no zwlr_virtual_pointer_manager_v1: the click is not tested here",
            live.kind
        );
        assert!(
            !click_required(),
            "{msg} (STRAND_MATRIX_REQUIRE_CLICK is set)"
        );
        eprintln!("{msg}");
    }
    drop(bar);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- reading the bar offline -------------------------------------------------

/// The light theme as sway draws it: the bar's background, `$fg` and the
/// accent (measured from matrix-sway-02-switched.png).
const SHOT_BG: [u8; 3] = [226, 225, 230];
const SHOT_FG: [u8; 3] = [26, 26, 31];
const SHOT_ACCENT: [u8; 3] = [122, 162, 247];

/// `$fg.alpha(a)` over the bar's background.
fn over(a: f32) -> [u8; 3] {
    let mut p = [0; 3];
    for i in 0..3 {
        p[i] = (SHOT_BG[i] as f32 + a * (SHOT_FG[i] as f32 - SHOT_BG[i] as f32)).round() as u8;
    }
    p
}

/// A bar 800 px wide: the centred clock in `$fg`, and `dots` (width,
/// colour) from the first dot's edge, 4 px apart, 8 px high.
fn synthetic(dots: &[(usize, [u8; 3])]) -> Img {
    let (w, h) = (800, 52);
    let mut img = Img {
        w,
        h,
        rgb: SHOT_BG.repeat(w * h),
    };
    let mut fill = |x0: usize, x1: usize, y0: usize, y1: usize, c: [u8; 3]| {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * w + x) * 3;
                img.rgb[i..i + 3].copy_from_slice(&c);
            }
        }
    };
    // The clock: a few glyph stems.
    for k in 0..8 {
        let x = w / 2 - 40 + k * 10;
        fill(x, x + 2, BAR_Y - 6, BAR_Y + 6, SHOT_FG);
    }
    let mut x = DOTS_X;
    for &(dw, c) in dots {
        fill(x, x + dw, BAR_Y - 4, BAR_Y + 4, c);
        x += dw + 4;
    }
    img
}

#[test]
fn a_lone_empty_dot_beside_the_pill_reads_empty() {
    // niri's trailing empty workspace beside the focused one.
    let img = synthetic(&[(24, SHOT_ACCENT), (8, over(0.25))]);
    let (dots, title, _) = drawn(&img).unwrap();
    assert!(!title);
    assert_eq!(
        dots,
        vec![
            Drawn {
                focused: true,
                occupied: false
            },
            Drawn {
                focused: false,
                occupied: false
            },
        ]
    );
}

#[test]
fn a_lone_occupied_dot_beside_the_pill_reads_occupied() {
    let img = synthetic(&[(24, SHOT_ACCENT), (8, over(0.65))]);
    let (dots, _, _) = drawn(&img).unwrap();
    assert_eq!(
        dots[1],
        Drawn {
            focused: false,
            occupied: true
        }
    );
}

#[test]
fn occupied_and_empty_dots_are_told_apart() {
    let img = synthetic(&[
        (8, over(0.65)),
        (24, SHOT_ACCENT),
        (8, over(0.25)),
        (8, over(0.65)),
    ]);
    let (dots, _, _) = drawn(&img).unwrap();
    let occupied: Vec<bool> = dots.iter().map(|d| d.occupied).collect();
    assert_eq!(occupied, [true, false, false, true]);
    let focused: Vec<bool> = dots.iter().map(|d| d.focused).collect();
    assert_eq!(focused, [false, true, false, false]);
}

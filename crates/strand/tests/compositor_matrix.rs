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
//! With a second output (scripts/compositor-matrix.sh makes one for sway
//! and Hyprland; nested niri has one), the stores must agree on every
//! output and on which one has the focus, through a switch to the other
//! output from outside, a window there, `ws.focus()` and `win.focus()`
//! across outputs; and each output's bar must draw that output's
//! workspaces and take its own clicks.

//! labwc (`STRAND_MATRIX=labwc`) is the matrix's compositor without an
//! IPC adapter: no CLI reports its state, so the two tests above skip it
//! and `the_stores_follow_a_compositor_without_ipc` runs instead, with the
//! test windows' own `xdg_toplevel` configures (activated or not) and
//! close requests as the truth, and the workspaces labwc reports over
//! `ext-workspace-v1`.
//!
//! On all four, `window_state_actions_follow_the_compositor` turns a
//! window's fullscreen and maximize on and off through the `windows` store
//! and checks the compositor's report and the window's own configures.
//!
//! Environment: `STRAND_MATRIX` names the compositor (`sway`, `hyprland`
//! or `niri`); `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and its IPC variable
//! (`SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`, `NIRI_SOCKET`) point at
//! it; `STRAND_MATRIX_NIRI_CONFIG` is the config file niri watches (the
//! reload edits it); `STRAND_MATRIX_OUTPUTS` is how many outputs the
//! script set up (checked). Without `STRAND_MATRIX` the tests print that
//! they were skipped.
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
use strand_services::wm::{
    self, WindowAction, WindowItem, WmAction, WmConfig, WmError, WmRequest, WorkspaceAction,
    WorkspaceItem,
};
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
    /// The output's logical size.
    w: u32,
    h: u32,
    socket: PathBuf,
}

/// One enabled output as the compositor reports it, in layout (logical)
/// coordinates.
#[derive(Clone, Debug, PartialEq)]
struct Output {
    name: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    focused: bool,
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
    let outputs = live.outputs();
    let o = outputs
        .iter()
        .find(|o| o.focused)
        .or(outputs.first())
        .expect("an output")
        .clone();
    live.output = o.name;
    live.w = o.w;
    live.h = o.h;
    let all: Vec<String> = outputs
        .iter()
        .map(|o| format!("{} {}x{}+{}+{}", o.name, o.w, o.h, o.x, o.y))
        .collect();
    eprintln!(
        "matrix: {kind:?} on {} ({}x{}); outputs: {}",
        live.output,
        live.w,
        live.h,
        all.join(", ")
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
    /// The enabled outputs, left to right (then top to bottom), in layout
    /// coordinates, and which one has the focus.
    fn outputs(&self) -> Vec<Output> {
        let int = |v: &Value| v.as_f64().unwrap_or(0.0).round() as i32;
        let mut all: Vec<Output> = match self.kind {
            Kind::Sway => json("swaymsg", &["-t", "get_outputs", "-r"])
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["active"] != false)
                .map(|o| {
                    let r = &o["rect"];
                    Output {
                        name: text(&o["name"]),
                        x: int(&r["x"]),
                        y: int(&r["y"]),
                        w: int(&r["width"]) as u32,
                        h: int(&r["height"]) as u32,
                        focused: o["focused"] == true,
                    }
                })
                .collect(),
            Kind::Hyprland => json("hyprctl", &["-j", "monitors"])
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["disabled"] != true)
                .map(|m| {
                    // `width`/`height` are the mode's pixels; `x`/`y` are
                    // already logical.
                    let s = m["scale"].as_f64().unwrap_or(1.0).max(0.1);
                    let logical = |v: &Value| (v.as_f64().unwrap_or(0.0) / s).round() as u32;
                    Output {
                        name: text(&m["name"]),
                        x: int(&m["x"]),
                        y: int(&m["y"]),
                        w: logical(&m["width"]),
                        h: logical(&m["height"]),
                        focused: m["focused"] == true,
                    }
                })
                .collect(),
            Kind::Niri => {
                let focused = json("niri", &["msg", "--json", "focused-output"]);
                json("niri", &["msg", "--json", "outputs"])
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.values())
                    .filter(|o| !o["logical"].is_null())
                    .map(|o| {
                        let l = &o["logical"];
                        Output {
                            name: text(&o["name"]),
                            x: int(&l["x"]),
                            y: int(&l["y"]),
                            w: int(&l["width"]) as u32,
                            h: int(&l["height"]) as u32,
                            focused: focused["name"] == o["name"],
                        }
                    })
                    .collect()
            }
        };
        all.sort_by_key(|o| (o.x, o.y));
        all
    }

    /// Where `(x, y)` on output `name` is for a virtual pointer's absolute
    /// motion, which spans the whole layout: the point and the layout's
    /// extent, as `motion_absolute` takes them.
    fn layout_point(&self, name: &str, x: u32, y: u32) -> (u32, u32, u32, u32) {
        let outs = self.outputs();
        let left = outs.iter().map(|o| o.x).min().unwrap_or(0);
        let top = outs.iter().map(|o| o.y).min().unwrap_or(0);
        let right = outs.iter().map(|o| o.x + o.w as i32).max().unwrap_or(0);
        let bottom = outs.iter().map(|o| o.y + o.h as i32).max().unwrap_or(0);
        let o = outs
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("no output {name}: {outs:?}"));
        (
            (o.x - left) as u32 + x,
            (o.y - top) as u32 + y,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    }

    /// The compositor's own report, for the output under test.
    fn real(&self) -> Real {
        self.real_on(&self.output)
    }

    /// The compositor's own report, for output `output`.
    fn real_on(&self, output: &str) -> Real {
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
                        if text(&o["name"]) != output {
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
                    .filter(|w| text(&w["monitor"]) == output)
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
                    .filter(|w| text(&w["output"]) == output)
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

    /// The workspace to switch to that is not the focused one, on the
    /// output under test: niri's empty one (it always keeps one, and its
    /// names are indices on the focused output); for sway and Hyprland a
    /// new one, made by switching to it, named by the first number no
    /// output uses (workspace 2 may already be another output's, and
    /// switching to it would move the focus there).
    fn other(&self, real: &Real) -> String {
        if self.kind == Kind::Niri {
            let empty = real
                .workspaces
                .iter()
                .find(|w| !w.occupied && !w.focused)
                .expect("niri's empty workspace");
            return empty.name.clone();
        }
        let used: Vec<String> = self
            .outputs()
            .iter()
            .flat_map(|o| self.real_on(&o.name).workspaces)
            .map(|w| w.name)
            .collect();
        let mut n = 2;
        while used.contains(&n.to_string()) {
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
    stored_on(b, rt, &live.output)
}

/// What the stores say about output `output`, in their order.
fn stored_on(b: &Builtin, rt: &Runtime, output: &str) -> Result<Real, String> {
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
            .filter(|w| w.screen == output)
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

// ---- window state actions ----------------------------------------------------

/// What the compositor itself says about the test window `app`:
/// `(maximized, fullscreen)`, or `None` when its IPC does not say.
/// sway: `fullscreen_mode` (no maximize); Hyprland: `j/clients`'
/// `fullscreen` mode (1 maximized, 2 fullscreen). niri's IPC reports
/// neither and labwc has no IPC: there the compositor's own word is the
/// `xdg_toplevel` configure it sends the window.
fn compositor_says(kind: &str, app: &str) -> Option<(bool, bool)> {
    match kind {
        "sway" => {
            fn find(n: &Value, app: &str) -> Option<i64> {
                if n["app_id"] == app {
                    return n["fullscreen_mode"].as_i64();
                }
                ["nodes", "floating_nodes"]
                    .iter()
                    .filter_map(|k| n[*k].as_array())
                    .flatten()
                    .find_map(|c| find(c, app))
            }
            let mode = find(&json("swaymsg", &["-t", "get_tree", "-r"]), app)?;
            Some((false, mode != 0))
        }
        "hyprland" => {
            let clients = json("hyprctl", &["-j", "clients"]);
            let c = clients
                .as_array()?
                .iter()
                .find(|c| c["class"] == app)?
                .clone();
            let mode = c["fullscreen"].as_i64()?;
            Some((mode == 1, mode & 2 != 0))
        }
        _ => None,
    }
}

/// The reply to `action` from a `wm::run` of its own (the stores log a
/// failed action, they do not return it), once that run shows `app`'s
/// window: the window's id there is the stores' (the same adapter).
fn reply_to(app: &str, action: fn(String) -> WmAction) -> Result<(), WmError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mirror = Arc::new(std::sync::Mutex::new(wm::Mirror::default()));
        let m = mirror.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let service = tokio::spawn(wm::run(
            WmConfig::from_env(None),
            move |batch: Vec<wm::WmChange>| {
                let mut m = m.lock().unwrap();
                for c in &batch {
                    m.apply(c).unwrap();
                }
            },
            rx,
        ));
        let deadline = Instant::now() + PATIENCE;
        let id = loop {
            let found = mirror
                .lock()
                .unwrap()
                .window_by_app(app)
                .map(|w| w.id.clone());
            if let Some(id) = found {
                break id;
            }
            assert!(Instant::now() < deadline, "{app} never showed in wm::run");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let (req, reply) = WmRequest::new(action(id));
        tx.send(req).unwrap();
        let out = tokio::time::timeout(PATIENCE, reply).await.unwrap();
        service.abort();
        out
    })
}

/// `win.fullscreen()` and `win.maximize()` through the `windows` store,
/// on and off, against the compositor's own truth ([`compositor_says`])
/// and the window's own view (its configures), on every compositor of the
/// matrix: fullscreen everywhere; maximize on Hyprland (its maximized
/// fullscreen mode), niri (maximize-to-edges) and labwc (the wlr
/// protocol); `Unsupported` on sway, which has no maximize, and nothing
/// changes there.
#[test]
fn window_state_actions_follow_the_compositor() {
    let Ok(kind) = std::env::var("STRAND_MATRIX") else {
        skipped("the window state actions test");
        return;
    };
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let socket = runtime.join(std::env::var_os("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY"));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.workspaces.acquire(&rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    const APP: &str = "strand-winstate";
    let win = TestWindow::open(&socket, APP, "window state");
    let item = || {
        s.pump(&rt);
        rt.flush();
        b.windows
            .cells()
            .snapshot(&rt)
            .ok()
            .and_then(|w| w.all.into_iter().find(|w| w.app_id == APP))
    };
    until("the window in windows.all", || {
        item().map(|_| ()).ok_or_else(|| "not yet".to_string())
    });
    // One state, three views: the store's, the compositor's, the window's.
    let check = |what: &str, maximized: bool, fullscreen: bool| {
        until(what, || {
            let w = item().ok_or("the window is gone")?;
            let ours = (w.maximized, w.fullscreen);
            let client = (
                // Hyprland tells every xdg toplevel it is maximized when
                // it maps (to keep client decorations off,
                // src/protocols/XDGShell.cpp at v0.56.2), so there the
                // window's own maximized says nothing; its IPC does.
                if kind == "hyprland" {
                    maximized
                } else {
                    win.maximized.load(Ordering::SeqCst)
                },
                win.fullscreen.load(Ordering::SeqCst),
            );
            let theirs = compositor_says(&kind, APP).unwrap_or(client);
            let want = (maximized, fullscreen);
            if ours == want && theirs == want && client == want {
                Ok(())
            } else {
                Err(format!(
                    "want {want:?}; windows.all {ours:?}; {kind} {theirs:?}; the window {client:?}"
                ))
            }
        });
        eprintln!("matrix: {what}: maximized {maximized}, fullscreen {fullscreen}");
    };
    check("before", false, false);
    let act = |action: fn(WindowItem) -> WindowAction| {
        let w = item().expect("the window");
        b.windows.act(&rt, action(w)).unwrap();
    };

    // `win.fullscreen()`: on, then off.
    act(|item| WindowAction::Fullscreen { item });
    check("win.fullscreen()", false, true);
    act(|item| WindowAction::Fullscreen { item });
    check("win.fullscreen() again", false, false);

    // `win.maximize()`.
    if kind == "sway" {
        assert!(
            matches!(
                reply_to(APP, WmAction::MaximizeWindow),
                Err(WmError::Unsupported(_))
            ),
            "sway has no maximize"
        );
        act(|item| WindowAction::Maximize { item });
        std::thread::sleep(Duration::from_millis(500));
        check("win.maximize() on sway changes nothing", false, false);
    } else {
        act(|item| WindowAction::Maximize { item });
        check("win.maximize()", true, false);
        act(|item| WindowAction::Maximize { item });
        check("win.maximize() again", false, false);
    }
    // Both replies from the compositor's own path: `Ok`.
    assert_eq!(reply_to(APP, WmAction::FullscreenWindow), Ok(()));
    check("win.fullscreen() by request", false, true);
    assert_eq!(reply_to(APP, WmAction::FullscreenWindow), Ok(()));
    check("and back", false, false);
    drop(win);
    s.shutdown();
}

/// (M4) `thumbnail w`'s capture, live: a tap on a real window through
/// `wm::capture` gets frames of it over the compositor's
/// ext-image-copy-capture, scaled down to cover the size asked for, in
/// the window's own colour (a capture of the wrong buffer, read before
/// `ready`, or all zeros fails it).
/// Skipped (with a message) on a
/// compositor that offers no ext-image-copy-capture.
#[test]
fn a_window_is_captured_for_its_thumbnail() {
    use strand_services::wm::capture::{CaptureFrame, capture_window};
    let Ok(kind) = std::env::var("STRAND_MATRIX") else {
        skipped("the thumbnail capture test");
        return;
    };
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let socket = runtime.join(std::env::var_os("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY"));
    let globals = {
        use wayland_client::Connection;
        use wayland_client::globals::registry_queue_init;
        let conn =
            Connection::from_socket(std::os::unix::net::UnixStream::connect(&socket).unwrap())
                .unwrap();
        let (globals, _queue) = registry_queue_init::<support::pointer::Client>(&conn).unwrap();
        globals
            .contents()
            .with_list(|l| l.iter().map(|g| g.interface.clone()).collect::<Vec<_>>())
    };
    let has = |i: &str| globals.iter().any(|g| g == i);
    if !(has("ext_image_copy_capture_manager_v1")
        && has("ext_foreign_toplevel_image_capture_source_manager_v1"))
    {
        eprintln!(
            "\n*** {kind} offers no ext-image-copy-capture of toplevels: thumbnails skipped ***\n"
        );
        return;
    }
    const APP: &str = "strand-thumb";
    // An opaque colour no compositor background or border shares.
    const RGBA: [u8; 4] = [0x2a, 0x9d, 0x5c, 0xff];
    let _win = TestWindow::open_filled(&socket, APP, "thumbnail", RGBA);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mirror = Arc::new(std::sync::Mutex::new(wm::Mirror::default()));
        let m = mirror.clone();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let service = tokio::spawn(wm::run(
            WmConfig::from_env(None),
            move |batch: Vec<wm::WmChange>| {
                let mut m = m.lock().unwrap();
                for c in &batch {
                    m.apply(c).unwrap();
                }
            },
            rx,
        ));
        let deadline = Instant::now() + PATIENCE;
        let id = loop {
            let found = mirror
                .lock()
                .unwrap()
                .window_by_app(APP)
                .map(|w| w.id.clone());
            if let Some(id) = found {
                break id;
            }
            assert!(Instant::now() < deadline, "{APP} never showed in wm::run");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let (tx, frames) = std::sync::mpsc::channel::<CaptureFrame>();
        let tap = capture_window(&id, (16, 16), move |f| {
            if let Some(f) = f {
                let _ = tx.send(f.clone());
            }
        });
        // A pixel as straight RGBA (frames are premultiplied BGRA; the
        // window is opaque).
        let at = |f: &CaptureFrame, x: u32, y: u32| -> [u8; 4] {
            let i = ((y * f.width + x) * 4) as usize;
            let p = &f.pixels[i..i + 4];
            [p[2], p[1], p[0], p[3]]
        };
        let centre = |f: &CaptureFrame| at(f, f.width / 2, f.height / 2);
        let near = |a: [u8; 4], b: [u8; 4]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3);
        // The window's colour anywhere in the frame: sway and labwc capture
        // the 64 x 64 buffer, Hyprland the tiled window's whole box, whose
        // centre lies outside that buffer (transparent; its top-left
        // corner is the buffer's).
        let coloured =
            |f: &CaptureFrame| (0..f.height).any(|y| (0..f.width).any(|x| near(at(f, x, y), RGBA)));
        // The first frame may come before the window's buffer is shown
        // (a compositor may capture the toplevel before its first
        // commit lands): the colour must arrive within the patience.
        let deadline = Instant::now() + PATIENCE;
        let mut last = None;
        let frame = loop {
            while let Ok(f) = frames.try_recv() {
                last = Some(f);
            }
            if let Some(f) = last.as_ref().filter(|f| f.width > 0 && coloured(f)) {
                break f.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no frame of {APP} in its colour from {kind}: last (w, h, centre) {:?}",
                last.as_ref().map(|f| (f.width, f.height, centre(f)))
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        eprintln!(
            "matrix: {kind} captured {APP} at {}x{}",
            frame.width, frame.height
        );
        assert!(frame.width >= 1 && frame.height >= 1);
        assert!(
            frame.width.min(frame.height) <= 16,
            "scaled down to cover 16 x 16: {}x{}",
            frame.width,
            frame.height
        );
        assert_eq!(
            frame.pixels.len(),
            (frame.width * frame.height * 4) as usize
        );
        drop(tap);
        service.abort();
    });
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
    _bus: Option<PrivateBus>,
}

impl<'a> Bar<'a> {
    /// `strand run` with design.md's bar (one instance per output) in a
    /// home of its own under `dir`.
    fn launch(live: &'a Live, dir: &std::path::Path) -> Self {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let home = dir.join("home");
        let config = home.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        for (name, text) in FILES {
            std::fs::write(config.join(name), text).unwrap();
        }
        // The bar's other services (the tray, the portal) on a bus of
        // their own; battery and audio have no daemon here and stay at
        // their defaults.
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
        Bar {
            live,
            dir: dir.to_path_buf(),
            log,
            shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
            n: 0,
            strand: Proc(cmd.spawn().unwrap()),
            _bus: bus,
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Output `output` as grim captures it.
    fn shot(&self, output: &str) -> Option<Img> {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
        Img::ppm(&std::fs::read(&path).ok()?)
    }

    fn keep(&mut self, output: &str, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        self.n += 1;
        let _ = std::fs::create_dir_all(dir);
        let kind = format!("{:?}", self.live.kind).to_lowercase();
        // The output in the name when it is not the one under test.
        let name = if output == self.live.output {
            name.to_string()
        } else {
            format!("{name}-{output}")
        };
        let _ = Command::new("grim")
            .args(["-t", "png", "-o", output])
            .arg(dir.join(format!("matrix-{kind}-{:02}-{name}.png", self.n)))
            .status();
    }

    /// The bar on the output under test draws the compositor's state.
    fn shows(&mut self, what: &str) -> (Real, Vec<usize>) {
        let output = self.live.output.clone();
        self.shows_on(&output, what)
    }

    /// The bar on `output` draws the compositor's state for that output
    /// (twice in a row, so a dot mid-animation does not count): that
    /// state and the dots' left edges.
    fn shows_on(&mut self, output: &str, what: &str) -> (Real, Vec<usize>) {
        let mut seen = 0;
        let mut last = None;
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Ok(Some(status)) = self.strand.0.try_wait() {
                panic!("strand exited ({status}): {}", self.log_text());
            }
            let real = self.live.real_on(output);
            let want = expected(&real);
            let got = match self.shot(output) {
                Some(img) => drawn(&img),
                None => Err("grim failed".into()),
            };
            match got {
                Ok((dots, title, xs)) if (&dots, title) == (&want.0, want.1) => {
                    seen += 1;
                    if seen == 2 {
                        eprintln!("matrix: the bar on {output} shows {what}: {real}");
                        self.keep(output, what);
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
                self.keep(output, &format!("{what}-timeout"));
                panic!(
                    "never: the bar on {output} shows {what}\n{}\n{}",
                    last.unwrap_or_default(),
                    self.log_text()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Whether the bar's click must be tested (`STRAND_MATRIX_REQUIRE_CLICK`
/// set and not `0`, as in CI): without a virtual pointer the test fails
/// instead of skipping the click.
fn click_required() -> bool {
    std::env::var("STRAND_MATRIX_REQUIRE_CLICK").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether the compositor offers `interface`.
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

/// A virtual pointer, when the compositor offers one; `None` (and a
/// message) when it does not and the click is not required, a failure
/// when it is.
fn pointer(live: &Live) -> Option<support::pointer::Pointer> {
    if offers(live, "zwlr_virtual_pointer_manager_v1") {
        let mut pointer = support::pointer::Pointer::new(&live.socket);
        // (A new virtual pointer's first button may reach no surface: one
        // on the desktop of the output under test first.)
        let (x, y, w, h) = live.layout_point(&live.output, live.w / 2, live.h - 40);
        pointer.click(x, y, w, h);
        std::thread::sleep(Duration::from_millis(200));
        return Some(pointer);
    }
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
    None
}

/// Clicks `(x, y)` on output `output`, then moves the pointer off the bar
/// onto that output's desktop (staying on the output: with the focus
/// following the mouse, crossing to another output would move the focus).
fn click_on(pointer: &mut support::pointer::Pointer, live: &Live, output: &str, x: u32, y: u32) {
    let (px, py, w, h) = live.layout_point(output, x, y);
    pointer.click(px, py, w, h);
    let o = live
        .outputs()
        .into_iter()
        .find(|o| o.name == output)
        .unwrap_or_else(|| panic!("no output {output}"));
    let (px, py, w, h) = live.layout_point(output, o.w / 2, o.h - 40);
    pointer.motion(px, py, w, h);
}

#[test]
fn the_design_bar_shows_the_live_compositor() {
    let Some(live) = live() else {
        skipped("the live compositor bar test");
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-matrix-{}", std::process::id()));

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
    let mut bar = Bar::launch(&live, &dir);
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
    if let Some(mut pointer) = pointer(&live) {
        let x = xs[home_ws] as u32 + 4;
        click_on(&mut pointer, &live, &live.output, x, BAR_Y as u32);
        let (real, _) = bar.shows("clicked");
        assert!(real.workspaces[home_ws].focused, "{real}");
    }
    drop(bar);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a second output ---------------------------------------------------------

/// The outputs, checked against `STRAND_MATRIX_OUTPUTS` (how many
/// scripts/compositor-matrix.sh set up) when it is set.
fn expected_outputs(live: &Live) -> Vec<Output> {
    let outs = live.outputs();
    if let Some(n) = std::env::var("STRAND_MATRIX_OUTPUTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        assert_eq!(outs.len(), n, "STRAND_MATRIX_OUTPUTS={n}: {outs:?}");
    }
    outs
}

/// The other output than the one under test, or `None` (and a message)
/// with one output.
fn second_output(live: &Live) -> Option<String> {
    let outs = expected_outputs(live);
    let second = outs.iter().find(|o| o.name != live.output);
    if second.is_none() {
        eprintln!(
            "matrix: {:?} has one output ({}): the second-output checks did not run",
            live.kind, live.output
        );
    }
    second.map(|o| o.name.clone())
}

/// The compositor's focused output.
fn focused_output(live: &Live) -> Option<String> {
    live.outputs()
        .into_iter()
        .find(|o| o.focused)
        .map(|o| o.name)
}

/// The stores agree with the compositor on every output: each output's
/// workspaces, the focused window and workspace, and which output has the
/// focus (the focused workspace's `screen`). Each output's report.
fn agree_all(
    rt: &Runtime,
    s: &Services,
    b: &Builtin,
    live: &Live,
    what: &str,
) -> Vec<(String, Real)> {
    let mut last = Vec::new();
    until(what, || {
        s.pump(rt);
        rt.flush();
        let outs = live.outputs();
        let mut all = Vec::new();
        for o in &outs {
            let real = live.real_on(&o.name);
            let ours = stored_on(b, rt, &o.name)?;
            if ours != real {
                return Err(format!(
                    "{0}: compositor: {real}\n{0}: stores:     {ours}",
                    o.name
                ));
            }
            all.push((o.name.clone(), real));
        }
        let screen = outs.iter().find(|o| o.focused).map(|o| o.name.clone());
        let ours = b
            .workspaces
            .cells()
            .snapshot(rt)
            .map_err(|e| format!("{e:?}"))?
            .focused
            .map(|w| w.screen);
        if ours != screen {
            return Err(format!(
                "the focused output: compositor {screen:?}, stores {ours:?}"
            ));
        }
        last = all;
        Ok(())
    });
    for (o, real) in &last {
        eprintln!("matrix: {what}: {o}: {real}");
    }
    last
}

/// The first workspace the compositor lists on `output` (a new output
/// has one).
fn shown_on(all: &[(String, Real)], output: &str) -> String {
    let (_, real) = all
        .iter()
        .find(|(o, _)| o == output)
        .unwrap_or_else(|| panic!("no report for {output}"));
    real.workspaces
        .first()
        .unwrap_or_else(|| panic!("{output} has no workspace: {real}"))
        .name
        .clone()
}

#[test]
fn the_stores_report_every_output() {
    let Some(live) = live() else {
        skipped("the live compositor second-output stores test");
        return;
    };
    let Some(second) = second_output(&live) else {
        return;
    };
    let first = live.output.clone();
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.workspaces.acquire(&rt);
    b.windows.acquire(&rt);
    b.wm.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    let boot = agree_all(&rt, &s, &b, &live, "boot, every output");
    let home = boot
        .iter()
        .find(|(o, _)| *o == first)
        .and_then(|(_, r)| r.workspaces.iter().find(|w| w.focused))
        .expect("a focused workspace on the output under test")
        .name
        .clone();
    let there = shown_on(&boot, &second);

    // The focus moves to the second output from outside (a switch to the
    // workspace it shows).
    live.switch(&there);
    agree_all(&rt, &s, &b, &live, "the second output focused from outside");
    assert_eq!(focused_output(&live).as_deref(), Some(second.as_str()));

    // A window opens there: its workspace is the second output's.
    let win = TestWindow::open(&live.socket, "strand-matrix-second", "on the second output");
    let all = agree_all(&rt, &s, &b, &live, "a window on the second output");
    let (_, real) = all.iter().find(|(o, _)| *o == second).unwrap();
    assert!(
        real.workspaces
            .iter()
            .any(|w| w.name == there && w.occupied),
        "{real}"
    );
    let window = b
        .windows
        .cells()
        .snapshot(&rt)
        .unwrap()
        .all
        .into_iter()
        .find(|w| w.app_id == "strand-matrix-second")
        .expect("the window in windows.all");
    let ws = workspace_named(&b, &rt, &there);
    assert_eq!(window.workspace, Some(ws.id), "the window's workspace");
    assert_eq!(ws.screen, second, "the window's workspace's screen");

    // `ws.focus()` on the first output's workspace moves the focus back.
    let item = workspace_named(&b, &rt, &home);
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item })
        .unwrap();
    agree_all(&rt, &s, &b, &live, "ws.focus() on the first output");
    assert_eq!(focused_output(&live).as_deref(), Some(first.as_str()));

    // `win.focus()` on the window there moves it to the second output.
    b.windows
        .act(&rt, WindowAction::Focus { item: window })
        .unwrap();
    let all = agree_all(&rt, &s, &b, &live, "win.focus() on the second output");
    assert_eq!(focused_output(&live).as_deref(), Some(second.as_str()));
    let (_, real) = all.iter().find(|(o, _)| *o == second).unwrap();
    assert_eq!(
        real.window,
        Some((
            "strand-matrix-second".to_string(),
            "on the second output".to_string()
        ))
    );

    // It closes; back on the first output.
    drop(win);
    agree_all(&rt, &s, &b, &live, "the window closed");
    let item = workspace_named(&b, &rt, &home);
    b.workspaces
        .act(&rt, WorkspaceAction::Focus { item })
        .unwrap();
    agree_all(&rt, &s, &b, &live, "back on the first output");
    s.shutdown();
}

#[test]
fn the_design_bar_is_on_every_output() {
    let Some(live) = live() else {
        skipped("the live compositor second-output bar test");
        return;
    };
    let Some(second) = second_output(&live) else {
        return;
    };
    let first = live.output.clone();
    let dir = std::env::temp_dir().join(format!("strand-matrix-two-{}", std::process::id()));
    let focused_on = |output: &str| {
        until(&format!("the focus on {output}"), || {
            match focused_output(&live) {
                Some(o) if o == output => Ok(()),
                other => Err(format!("focused: {other:?}")),
            }
        })
    };
    let focused_ws = |output: &str| -> String {
        let real = live.real_on(output);
        real.workspaces
            .iter()
            .find(|w| w.focused)
            .unwrap_or_else(|| panic!("no focused workspace on {output}: {real}"))
            .name
            .clone()
    };

    // An occupied workspace on each output, and the second output showing
    // another, empty one. Moving the pointer onto an output's bar focuses
    // the workspace that output shows (sway and Hyprland: the focus
    // follows the mouse), so only a click on a dot it does not show
    // proves that the bar took the click.
    let _a = TestWindow::open(&live.socket, "strand-matrix", "first output");
    until("the first window", || match live.real().window {
        Some(_) => Ok(()),
        None => Err(format!("{}", live.real())),
    });
    let home = focused_ws(&first);
    let there = live.real_on(&second).workspaces[0].name.clone();
    live.switch(&there);
    focused_on(&second);
    let _c = TestWindow::open(&live.socket, "strand-matrix-second", "second output");
    until("the second window", || match live.real_on(&second).window {
        Some((app, _)) if app == "strand-matrix-second" => Ok(()),
        other => Err(format!("{other:?}")),
    });
    let shown_second = live.other(&live.real_on(&second));
    live.switch(&shown_second);
    live.switch(&home);
    focused_on(&first);

    // Each output's bar draws that output's workspaces; both the title.
    let mut bar = Bar::launch(&live, &dir);
    let (real, _) = bar.shows_on(&first, "two-outputs");
    assert!(
        real.workspaces.iter().any(|w| w.name == home && w.focused),
        "{real}"
    );
    let (real, xs) = bar.shows_on(&second, "two-outputs");
    let i_there = real
        .workspaces
        .iter()
        .position(|w| w.name == there)
        .unwrap_or_else(|| panic!("{there} on {second}: {real}"));
    assert!(
        real.workspaces[i_there].occupied && !real.workspaces[i_there].focused,
        "{real}"
    );
    assert!(
        real.workspaces.iter().any(|w| w.name == shown_second),
        "{real}"
    );

    let Some(mut pointer) = pointer(&live) else {
        // No click: a switch from outside moves the pill between bars.
        live.switch(&there);
        let (real, _) = bar.shows_on(&second, "two-switched");
        assert!(real.workspaces[i_there].focused, "{real}");
        drop(bar);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };

    // A click on the second output's bar, on the workspace it does not
    // show: that output and that workspace take the focus.
    click_on(
        &mut pointer,
        &live,
        &second,
        xs[i_there] as u32 + 4,
        BAR_Y as u32,
    );
    let (real, _) = bar.shows_on(&second, "two-clicked-second");
    assert!(real.workspaces[i_there].focused, "{real}");
    assert_eq!(focused_output(&live).as_deref(), Some(second.as_str()));
    let (real, _) = bar.shows_on(&first, "two-clicked-second");
    assert!(real.workspaces.iter().all(|w| !w.focused), "{real}");

    // The same on the first output: it shows a new, empty workspace, the
    // focus is on the second output, and a click on the first output's
    // occupied workspace focuses it.
    live.switch(&home);
    focused_on(&first);
    let shown_first = live.other(&live.real_on(&first));
    live.switch(&shown_first);
    live.switch(&there);
    focused_on(&second);
    let (real, xs) = bar.shows_on(&first, "two-switched");
    let i_home = real
        .workspaces
        .iter()
        .position(|w| w.name == home)
        .unwrap_or_else(|| panic!("{home} on {first}: {real}"));
    assert!(!real.workspaces[i_home].focused, "{real}");
    click_on(
        &mut pointer,
        &live,
        &first,
        xs[i_home] as u32 + 4,
        BAR_Y as u32,
    );
    let (real, _) = bar.shows_on(&first, "two-clicked-first");
    assert!(real.workspaces[i_home].focused, "{real}");
    assert_eq!(focused_output(&live).as_deref(), Some(first.as_str()));
    let (real, _) = bar.shows_on(&second, "two-clicked-first");
    assert!(real.workspaces.iter().all(|w| !w.focused), "{real}");
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

// ---- surfaces: capabilities, the blur region, scrims, Hyprland's rules ------

/// A surface host that paints every surface the test blue (or as
/// `fills` says) and asks for the blur region it is given, recording the
/// capabilities reported.
#[derive(Default)]
struct BlueHost {
    caps: Vec<strand_scene::CompositorCaps>,
    blur: Vec<strand_scene::BlurRegion>,
    fills: std::collections::HashMap<strand_scene::SurfaceId, Fill>,
    /// (M4) The compositor pose each surface reports
    /// (`Painter::surface_pose`).
    poses: std::collections::HashMap<strand_scene::SurfaceId, strand_scene::SurfacePose>,
}

/// How [`BlueHost`] paints a surface other than blue.
#[derive(Copy, Clone, PartialEq)]
enum Fill {
    /// Black and white columns three pixels wide: sharp unless blurred.
    /// Not two: Hyprland's dual Kawase blur (size 8, one pass) samples
    /// 8 px apart going down and 2 and 4 half-resolution px apart going
    /// up, all whole periods of 2 px columns, so it leaves them at half
    /// their contrast (decisions.md, m4-surface-w2). A 6 px period is
    /// not a divisor of those offsets and blurs to under 4 %.
    Stripes,
    /// White at 60 % (above Hyprland's `ignore_alpha 0.5`).
    Glass,
    /// Opaque white.
    White,
}

impl strand_scene::Painter for BlueHost {
    fn paint(
        &mut self,
        id: strand_scene::SurfaceId,
        target: &mut strand_scene::PaintTarget<'_>,
    ) -> strand_scene::Damage {
        let (w, stride) = (target.size.w as usize, target.stride as usize);
        let fill = self.fills.get(&id).copied();
        for row in target.pixels.chunks_exact_mut(stride) {
            for (x, px) in row[..w * 4].chunks_exact_mut(4).enumerate() {
                // ARGB8888, little-endian, premultiplied.
                px.copy_from_slice(&match fill {
                    // The blue of the surface tests.
                    None => [0xe0, 0x60, 0x20, 0xff],
                    Some(Fill::Stripes) if (x / 3) % 2 == 0 => [0, 0, 0, 0xff],
                    Some(Fill::Stripes) => [0xff; 4],
                    Some(Fill::Glass) => [0x99; 4],
                    Some(Fill::White) => [0xff; 4],
                });
            }
        }
        strand_scene::Damage::full(target.size)
    }

    fn wants_frame(&self, _: strand_scene::SurfaceId) -> bool {
        false
    }

    fn blur_region(&self, _: strand_scene::SurfaceId) -> Vec<strand_scene::BlurRegion> {
        self.blur.clone()
    }

    fn surface_pose(&self, id: strand_scene::SurfaceId) -> Option<strand_scene::SurfacePose> {
        self.poses.get(&id).copied()
    }
}

impl strand_surface::SurfaceHost for BlueHost {
    fn compositor_caps(&mut self, caps: &strand_scene::CompositorCaps) {
        self.caps.push(*caps);
    }
}

/// The interfaces the compositor at `socket` offers.
fn registry(socket: &std::path::Path) -> Vec<String> {
    use wayland_client::Connection;
    use wayland_client::globals::registry_queue_init;
    let stream = std::os::unix::net::UnixStream::connect(socket).expect("the compositor");
    let conn = Connection::from_socket(stream).expect("a connection");
    let (globals, _queue) =
        registry_queue_init::<support::pointer::Client>(&conn).expect("the registry");
    globals
        .contents()
        .with_list(|l| l.iter().map(|g| g.interface.clone()).collect())
}

/// The whole layout as grim captures it (labwc has one output), or one
/// output.
fn grab(output: Option<&str>, dir: &std::path::Path) -> Option<Img> {
    let path = dir.join("surfaces.ppm");
    let mut cmd = Command::new("grim");
    cmd.args(["-t", "ppm"]);
    if let Some(o) = output {
        cmd.args(["-o", o]);
    }
    let ok = cmd
        .arg(&path)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return None;
    }
    Img::ppm(&std::fs::read(&path).ok()?)
}

/// M4's surface protocols against each compositor (labwc included):
/// the capabilities the surface manager reports are the registry's (the
/// background effect only where it is offered); where the compositor
/// blurs, a panel's rounded blur region is sent; a panel's scrim dims a
/// wallpaper surface beneath it and not the panel, and so does a scrim on
/// a panel's popup (neither the panel nor the popup), and one on the
/// popup of a bar whose shadow reaches past its exclusive zone (not the
/// shadow); and on Hyprland the layer rules
/// `strand compositor-rules` prints evaluate without errors and blur the
/// surface they name.
#[test]
fn surfaces_meet_the_live_compositor() {
    use strand_scene::{
        BlurRegion, Color, NodeId, NodeKind, Prop, PropValue, Rect, SurfaceChange, SurfaceSpec,
    };
    use strand_surface::{Config, SurfaceManager};
    use wayland_client::Connection;

    let Ok(kind) = std::env::var("STRAND_MATRIX") else {
        eprintln!(
            "\n*** SKIPPED: STRAND_MATRIX is not set; surfaces_meet_the_live_compositor did not \
             run (scripts/compositor-matrix.sh) ***\n"
        );
        return;
    };
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    let socket = runtime.join(std::env::var_os("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY"));
    // The output under test (labwc: its only one, the whole layout).
    let live = live();
    let output = live.as_ref().map(|l| l.output.clone());
    let (ow, oh) = match &live {
        Some(l) => (l.w as usize, l.h as usize),
        None => {
            let img = grab(None, &std::env::temp_dir()).expect("grim");
            (img.w, img.h)
        }
    };
    let dir = tempfile::tempdir().expect("a temporary directory");

    let stream = std::os::unix::net::UnixStream::connect(&socket).expect("the compositor");
    let conn = Connection::from_socket(stream).expect("a connection");
    let mut mgr = SurfaceManager::with_connection(conn, BlueHost::default(), Config::default())
        .expect("the surface manager starts");
    let ok = mgr
        .dispatch_until(PATIENCE, |s| !s.host().caps.is_empty())
        .expect("dispatch");
    assert!(ok, "{kind}: no capabilities reported");
    // The blur capability comes with the manager's first event.
    let _ = mgr.dispatch_until(Duration::from_millis(500), |_| false);
    let caps = mgr.state().compositor_caps();
    let offered = registry(&socket);
    let has = |i: &str| offered.iter().any(|o| o == i);
    eprintln!("matrix: {kind} reports {caps:?}");
    assert_eq!(caps.alpha_modifier, has("wp_alpha_modifier_v1"), "{kind}");
    assert_eq!(caps.viewporter, has("wp_viewporter"), "{kind}");
    assert_eq!(
        caps.single_pixel_buffer,
        has("wp_single_pixel_buffer_manager_v1"),
        "{kind}"
    );
    assert_eq!(
        caps.session_lock,
        has("ext_session_lock_manager_v1"),
        "{kind}"
    );
    assert_eq!(caps.data_device, has("wl_data_device_manager"), "{kind}");
    assert!(
        !caps.background_effect || has("ext_background_effect_manager_v1"),
        "{kind}: blur without the protocol"
    );
    assert_eq!(mgr.state().host().caps.last(), Some(&caps));

    // A wallpaper of our own on the background layer, so the scrim has
    // something to dim (the matrix's desktops are black on some).
    const WALL: NodeId = NodeId::new(8, 0);
    let wall: std::collections::HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text("Wall".into())),
        (Prop::Layer, PropValue::Keyword("background".into())),
        (Prop::Width, PropValue::Number(ow as f32)),
        (Prop::Height, PropValue::Number(oh as f32)),
    ]
    .into_iter()
    .collect();
    mgr.state_mut().apply_surface_change(
        WALL,
        SurfaceChange::Created(SurfaceSpec::resolve(NodeKind::Panel, |p| wall.get(&p))),
    );
    let ok = mgr
        .dispatch_until(PATIENCE, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == WALL && i.stats.commits > 0)
        })
        .expect("dispatch");
    assert!(ok, "{kind}: the wallpaper: {:?}", mgr.state().surfaces());
    let deadline = Instant::now() + PATIENCE;
    let before = loop {
        let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
        let img = grab(output.as_deref(), dir.path()).expect("grim");
        if dist(img.px(ow / 4, oh * 3 / 4), [0x20, 0x60, 0xe0]) <= 9 {
            break img;
        }
        assert!(
            Instant::now() < deadline,
            "{kind}: the wallpaper never showed"
        );
    };
    // `panel Matrix { anchor: top_right; 300 × 200; scrim: black 30 % }`,
    // asking for blur behind its box with 16 px corners.
    const PANEL: NodeId = NodeId::new(9, 0);
    let props: std::collections::HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text("Matrix".into())),
        (Prop::Anchor, PropValue::Keyword("top_right".into())),
        (Prop::Width, PropValue::Number(300.0)),
        (Prop::Height, PropValue::Number(200.0)),
        (
            Prop::Scrim,
            PropValue::Color(Color::new(0.0, 0.0, 0.0, 0.3)),
        ),
    ]
    .into_iter()
    .collect();
    let spec = SurfaceSpec::resolve(NodeKind::Panel, |p| props.get(&p));
    mgr.state_mut().host_mut().blur = vec![BlurRegion {
        rect: Rect::new(0, 0, 300, 200),
        radii: [16.0; 4],
        radius: 24.0,
    }];
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(PATIENCE, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == PANEL && i.stats.commits > 0 && i.scrim.is_some())
        })
        .expect("dispatch");
    assert!(
        ok,
        "{kind}: the panel and its scrim: {:?}",
        mgr.state().surfaces()
    );
    let id = mgr.state().surfaces_of(PANEL)[0];
    if caps.background_effect {
        let info = mgr.state().surface(id).expect("the panel");
        let region = info.blur_region.expect("a blur region was sent");
        assert!(
            region.len() > 1,
            "{kind}: rounded corners are bands: {region:?}"
        );
        assert!(region.contains(&(0, 16, 300, 168)), "{kind}: {region:?}");
        eprintln!("matrix: {kind} blurs: {} rectangles", region.len());
    } else {
        eprintln!("matrix: {kind} does not blur: the tint fallback stays");
    }

    // The scrim dims a desktop pixel far from the panel; the panel itself
    // keeps its blue.
    let (sx, sy) = (ow / 4, oh * 3 / 4);
    let (px, py) = (ow - 150, 120);
    let base = before.px(sx, sy);
    let want = base.map(|c| (f32::from(c) * 0.7).round() as u8);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
        let img = grab(output.as_deref(), dir.path()).expect("grim");
        let (got, panel) = (img.px(sx, sy), img.px(px, py));
        if dist(got, want) <= 9 && dist(panel, [0x20, 0x60, 0xe0]) <= 9 {
            eprintln!("matrix: {kind}: the scrim dims {base:?} to {got:?}; the panel is {panel:?}");
            break;
        }
        if Instant::now() >= deadline {
            if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
                let mut cmd = Command::new("grim");
                if let Some(o) = &output {
                    cmd.args(["-o", o]);
                }
                let _ = cmd
                    .arg(PathBuf::from(shots).join(format!("matrix-{kind}-surfaces-failed.png")))
                    .status();
            }
            panic!(
                "{kind}: the desktop at ({sx}, {sy}) is {got:?} (was {base:?}, want {want:?}); \
                 the panel at ({px}, {py}) is {panel:?}"
            );
        }
    }
    if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
        let mut cmd = Command::new("grim");
        if let Some(o) = &output {
            cmd.args(["-o", o]);
        }
        let _ = cmd
            .arg(PathBuf::from(shots).join(format!("matrix-{kind}-surfaces-scrim.png")))
            .status();
    }
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Removed);
    mgr.state_mut().host_mut().blur.clear();

    // `panel Host { anchor: top_right; 300 × 200; popup { scrim: black
    // 30 % } }`: the popup's scrim goes on the layer below the panel (which
    // rises to `overlay` for it), so on a compositor that stacks the newer
    // of two surfaces on one layer on top the scrim still dims neither.
    const HOST: NodeId = NodeId::new(10, 0);
    const MENU: NodeId = NodeId::new(11, 0);
    let host: std::collections::HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text("Host".into())),
        (Prop::Anchor, PropValue::Keyword("top_right".into())),
        (Prop::Width, PropValue::Number(300.0)),
        (Prop::Height, PropValue::Number(200.0)),
    ]
    .into_iter()
    .collect();
    mgr.state_mut().apply_surface_change(
        HOST,
        SurfaceChange::Created(SurfaceSpec::resolve(NodeKind::Panel, |p| host.get(&p))),
    );
    let mut menu = SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&PropValue>);
    menu.name = Some("Menu".into());
    menu.parent = Some(HOST);
    menu.anchor_rect = Some(strand_scene::LogicalRect::new(100.0, 150.0, 60.0, 20.0));
    menu.width = Some(200.0);
    menu.height = Some(120.0);
    menu.scrim = Some(Color::new(0.0, 0.0, 0.0, 0.3));
    mgr.state_mut()
        .apply_surface_change(MENU, SurfaceChange::Created(menu));
    let ok = mgr
        .dispatch_until(PATIENCE, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == MENU && i.stats.commits > 0 && i.scrim.is_some())
        })
        .expect("dispatch");
    assert!(
        ok,
        "{kind}: the panel's popup and its scrim: {:?}",
        mgr.state().surfaces()
    );
    let host_id = mgr.state().surfaces_of(HOST)[0];
    assert_eq!(
        mgr.state().surface(host_id).and_then(|i| i.layer),
        Some(strand_scene::Layer::Overlay),
        "{kind}: the panel rises for its popup's scrim"
    );
    // The popup opens below its anchor, past the panel's bottom edge.
    let (hx, hy) = (ow - 150, 60);
    let (mx, my) = (ow - 170, 260);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
        let img = grab(output.as_deref(), dir.path()).expect("grim");
        let (got, panel, popup) = (img.px(sx, sy), img.px(hx, hy), img.px(mx, my));
        let blue = [0x20, 0x60, 0xe0];
        if dist(got, want) <= 9 && dist(panel, blue) <= 9 && dist(popup, blue) <= 9 {
            eprintln!(
                "matrix: {kind}: a popup's scrim dims {base:?} to {got:?}; \
                 the panel is {panel:?}, the popup {popup:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
                let mut cmd = Command::new("grim");
                if let Some(o) = &output {
                    cmd.args(["-o", o]);
                }
                let _ = cmd
                    .arg(PathBuf::from(shots).join(format!("matrix-{kind}-popup-scrim-failed.png")))
                    .status();
            }
            panic!(
                "{kind}: a popup's scrim: the desktop at ({sx}, {sy}) is {got:?} (want {want:?}); \
                 the panel at ({hx}, {hy}) is {panel:?}; the popup at ({mx}, {my}) is {popup:?}"
            );
        }
    }
    for node in [MENU, HOST] {
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Removed);
    }

    // `bar Shade { edge: top; height: 30 }` with a 10 px shadow below
    // it, past its exclusive zone, and `popup { scrim: black 30 % }`:
    // the shadow is in the usable area the scrim covers, so the scrim
    // goes on the layer below the bar (which rises to `overlay`) and the
    // shadow strip keeps its blue on every compositor.
    const SHADE: NodeId = NodeId::new(12, 0);
    const TIP: NodeId = NodeId::new(13, 0);
    let shade: std::collections::HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text("Shade".into())),
        (Prop::Edge, PropValue::Keyword("top".into())),
        (Prop::Height, PropValue::Number(30.0)),
    ]
    .into_iter()
    .collect();
    let mut bar = SurfaceSpec::resolve(NodeKind::Bar, |p| shade.get(&p));
    bar.overhang.bottom = 10.0;
    mgr.state_mut()
        .apply_surface_change(SHADE, SurfaceChange::Created(bar));
    let mut tip = SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&PropValue>);
    tip.name = Some("Tip".into());
    tip.parent = Some(SHADE);
    tip.anchor_rect = Some(strand_scene::LogicalRect::new(100.0, 5.0, 60.0, 20.0));
    tip.width = Some(200.0);
    tip.height = Some(120.0);
    tip.scrim = Some(Color::new(0.0, 0.0, 0.0, 0.3));
    mgr.state_mut()
        .apply_surface_change(TIP, SurfaceChange::Created(tip));
    let ok = mgr
        .dispatch_until(PATIENCE, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == TIP && i.stats.commits > 0 && i.scrim.is_some())
        })
        .expect("dispatch");
    assert!(
        ok,
        "{kind}: the shadowed bar's popup and its scrim: {:?}",
        mgr.state().surfaces()
    );
    let shade_id = mgr.state().surfaces_of(SHADE)[0];
    assert_eq!(
        mgr.state().surface(shade_id).and_then(|i| i.layer),
        Some(strand_scene::Layer::Overlay),
        "{kind}: a bar shadowing the usable area rises for its popup's scrim"
    );
    // Far from the popup: the bar's box, then its shadow strip.
    let (bx, by, shy) = (ow * 3 / 4, 15, 35);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
        let img = grab(output.as_deref(), dir.path()).expect("grim");
        let (got, bar, shadow) = (img.px(sx, sy), img.px(bx, by), img.px(bx, shy));
        let blue = [0x20, 0x60, 0xe0];
        if dist(got, want) <= 9 && dist(bar, blue) <= 9 && dist(shadow, blue) <= 9 {
            eprintln!(
                "matrix: {kind}: a shadowed bar's popup scrim dims {base:?} to {got:?}; \
                 the bar is {bar:?}, its shadow {shadow:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
                let mut cmd = Command::new("grim");
                if let Some(o) = &output {
                    cmd.args(["-o", o]);
                }
                let _ = cmd
                    .arg(PathBuf::from(shots).join(format!("matrix-{kind}-bar-scrim-failed.png")))
                    .status();
            }
            panic!(
                "{kind}: a shadowed bar's popup scrim: the desktop at ({sx}, {sy}) is {got:?} \
                 (want {want:?}); the bar at ({bx}, {by}) is {bar:?}; its shadow at ({bx}, {shy}) \
                 is {shadow:?}"
            );
        }
    }
    for node in [TIP, SHADE] {
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Removed);
    }
    let _ = mgr.dispatch_until(Duration::from_millis(300), |_| false);

    // (M4) Compositor-animated poses: `panel Pose { anchor: bottom_left;
    // 200 × 100 }`, opaque white, held at the pose render reports for
    // half opacity and half size about its centre: the alpha modifier
    // fades it over the wallpaper, and the viewport's destination with
    // the margins puts the half-size box around the full box's centre
    // (the compositor shrinks a layer surface towards its top-left
    // corner; render's offset moves that corner). Hyprland draws a layer
    // surface stretched to its arranged box whatever its viewport, so the
    // host delegates no scale there: it is held at half opacity, moved
    // 50 px right and 25 px up by its margins alone.
    if caps.alpha_modifier {
        const POSE: NodeId = NodeId::new(14, 0);
        let props: std::collections::HashMap<Prop, PropValue> = [
            (Prop::Name, PropValue::Text("Pose".into())),
            (Prop::Anchor, PropValue::Keyword("bottom_left".into())),
            (Prop::Width, PropValue::Number(200.0)),
            (Prop::Height, PropValue::Number(100.0)),
        ]
        .into_iter()
        .collect();
        mgr.state_mut().apply_surface_change(
            POSE,
            SurfaceChange::Created(SurfaceSpec::resolve(NodeKind::Panel, |p| props.get(&p))),
        );
        let id = mgr.state().surfaces_of(POSE)[0];
        let scaled = caps.viewporter && kind != "hyprland";
        let moved = kind == "hyprland";
        let pose = if moved {
            strand_scene::SurfacePose {
                opacity: 0.5,
                scale: 1.0,
                offset: strand_scene::LogicalPoint::new(50.0, -25.0),
            }
        } else if scaled {
            strand_scene::SurfacePose {
                opacity: 0.5,
                scale: 0.5,
                offset: strand_scene::LogicalPoint::new(50.0, 25.0),
            }
        } else {
            strand_scene::SurfacePose {
                opacity: 0.5,
                ..strand_scene::SurfacePose::IDENTITY
            }
        };
        let host = mgr.state_mut().host_mut();
        host.fills.insert(id, Fill::White);
        host.poses.insert(id, pose);
        let ok = mgr
            .dispatch_until(PATIENCE, |s| {
                s.surface(id)
                    .is_some_and(|i| i.stats.commits > 0 && i.pose == pose)
            })
            .expect("dispatch");
        assert!(ok, "{kind}: the posed panel: {:?}", mgr.state().surface(id));
        let blue = [0x20, 0x60, 0xe0];
        // White at half over the wallpaper's blue.
        let half = [0x90, 0xb0, 0xf0];
        let inside = if moved {
            // The box at x 50..250, y oh - 125..oh - 25.
            [(100, oh - 75), (240, oh - 115), (60, oh - 35)]
        } else {
            [(100, oh - 50), (60, oh - 70), (140, oh - 30)]
        };
        let outside: &[(usize, usize)] = if moved {
            &[
                (40, oh - 75),
                (260, oh - 75),
                (100, oh - 135),
                (100, oh - 15),
            ]
        } else if scaled {
            &[
                (40, oh - 50),
                (160, oh - 50),
                (100, oh - 85),
                (100, oh - 15),
            ]
        } else {
            &[(210, oh - 50), (100, oh - 110)]
        };
        let deadline = Instant::now() + PATIENCE;
        loop {
            let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
            let img = grab(output.as_deref(), dir.path()).expect("grim");
            let ins: Vec<[u8; 3]> = inside.iter().map(|&(x, y)| img.px(x, y)).collect();
            let outs: Vec<[u8; 3]> = outside.iter().map(|&(x, y)| img.px(x, y)).collect();
            if ins.iter().all(|p| dist(*p, half) <= 12) && outs.iter().all(|p| dist(*p, blue) <= 9)
            {
                eprintln!(
                    "matrix: {kind}: the alpha modifier fades the posed panel to {:?}{}",
                    ins[0],
                    if moved {
                        ", and the margins move it (its scale is painted: Hyprland stretches a layer surface to its box)"
                    } else if scaled {
                        ", and the viewport and margins scale it about its centre"
                    } else {
                        " (no viewporter: no scale)"
                    }
                );
                break;
            }
            if Instant::now() >= deadline {
                if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
                    let mut cmd = Command::new("grim");
                    if let Some(o) = &output {
                        cmd.args(["-o", o]);
                    }
                    let _ = cmd
                        .arg(PathBuf::from(shots).join(format!("matrix-{kind}-pose-failed.png")))
                        .status();
                }
                panic!(
                    "{kind}: the posed panel: inside {inside:?} is {ins:?} (want {half:?}),                      outside {outside:?} is {outs:?} (want the wallpaper's {blue:?}); {:?}",
                    mgr.state().surface(id)
                );
            }
        }
        mgr.state_mut()
            .apply_surface_change(POSE, SurfaceChange::Removed);
    } else {
        eprintln!("matrix: {kind} has no alpha modifier: poses repaint");
    }
    mgr.state_mut()
        .apply_surface_change(WALL, SurfaceChange::Removed);
    let _ = mgr.dispatch_until(Duration::from_millis(300), |_| false);

    if kind == "hyprland" {
        // `strand compositor-rules` for a config with a blurred panel and
        // a blurred popup, evaluated by Hyprland's Lua config.
        let conf = dir.path().join("conf");
        std::fs::create_dir_all(&conf).expect("a config directory");
        std::fs::write(
            conf.join("shell.strand"),
            "bar Top { edge: top; height: 30; blur: 24\n  \
             popup { open: true; width: 100; height: 50; box { blur: 8 } }\n}\n\
             panel Dash { anchor: top_right; width: 300; height: 200; blur: 24 }\n",
        )
        .expect("the config");
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("compositor-rules")
            .arg(&conf)
            .output()
            .expect("strand compositor-rules");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lua = String::from_utf8(out.stdout).expect("UTF-8 rules");
        assert_eq!(lua.matches("hl.layer_rule(").count(), 2, "{lua}");
        // The printed rules open with Lua `--` comments, which hyprctl
        // would take for an unknown flag (it prints its usage and exits
        // 0): only the code goes to `eval`.
        let code: String = lua
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.trim_start().starts_with('-'), "{code}");
        let eval = Command::new("hyprctl")
            .arg("eval")
            .arg(&code)
            .output()
            .expect("hyprctl eval");
        let said = String::from_utf8_lossy(&eval.stdout).to_string();
        eprintln!("matrix: hyprctl eval of the rules: {said:?}");
        assert!(eval.status.success(), "hyprctl eval failed: {said}");
        assert!(
            !said.contains("usage: hyprctl"),
            "hyprctl did not take the code as `eval`'s argument: {said}"
        );
        assert!(
            !said.to_lowercase().contains("error"),
            "Hyprland rejected the rules: {said}\n{lua}"
        );
        let errors = run("hyprctl", &["configerrors"]);
        assert!(
            !errors.to_lowercase().contains("layer_rule"),
            "config errors: {errors}"
        );

        // hyprctl answers 0 for a rule it did not take, so the proof is
        // a blurred pixel: `strand-Dash` (the rule's namespace), white
        // at 60 % over sharp stripes and asking for no blur of its own
        // (no ext-background-effect region), shows the stripes blurred
        // only if Hyprland applied the rule; beside it they stay sharp.
        const STRIPES: NodeId = NodeId::new(12, 0);
        const DASH: NodeId = NodeId::new(13, 0);
        let wall: std::collections::HashMap<Prop, PropValue> = [
            (Prop::Name, PropValue::Text("Stripes".into())),
            (Prop::Layer, PropValue::Keyword("background".into())),
            (Prop::Width, PropValue::Number(ow as f32)),
            (Prop::Height, PropValue::Number(oh as f32)),
        ]
        .into_iter()
        .collect();
        let dash: std::collections::HashMap<Prop, PropValue> = [
            (Prop::Name, PropValue::Text("Dash".into())),
            (Prop::Anchor, PropValue::Keyword("top_right".into())),
            (Prop::Width, PropValue::Number(300.0)),
            (Prop::Height, PropValue::Number(200.0)),
        ]
        .into_iter()
        .collect();
        for (node, props, fill) in [(STRIPES, &wall, Fill::Stripes), (DASH, &dash, Fill::Glass)] {
            mgr.state_mut().apply_surface_change(
                node,
                SurfaceChange::Created(SurfaceSpec::resolve(NodeKind::Panel, |p| props.get(&p))),
            );
            let id = mgr.state().surfaces_of(node)[0];
            mgr.state_mut().host_mut().fills.insert(id, fill);
        }
        let ok = mgr
            .dispatch_until(PATIENCE, |s| {
                [STRIPES, DASH].iter().all(|n| {
                    s.surfaces()
                        .iter()
                        .any(|i| i.node == *n && i.stats.commits > 0)
                })
            })
            .expect("dispatch");
        assert!(
            ok,
            "{kind}: the stripes and Dash: {:?}",
            mgr.state().surfaces()
        );
        // The largest step between a pixel and the one two to its right
        // (black against white in sharp stripes), along row `y`.
        let contrast = |img: &Img, x0: usize, y: usize| {
            (x0..x0 + 160)
                .map(|x| dist(img.px(x, y), img.px(x + 2, y)))
                .max()
                .unwrap_or(0)
        };
        let deadline = Instant::now() + PATIENCE;
        loop {
            let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
            let img = grab(output.as_deref(), dir.path()).expect("grim");
            let (inside, beside) = (
                contrast(&img, ow - 240, 100),
                contrast(&img, ow / 4, oh / 2),
            );
            // Unblurred, the glass (white at 60 %) leaves 40 % of the
            // stripes' 765: 306. Blurred by Hyprland's default kawase
            // (size 8, one pass), 3 px stripes keep under 4 % of their
            // contrast, about 12 here with its contrast and noise; the
            // 2 px stripes this test used first kept half (178/229
            // against 153/255 unblurred in GitHub run 38000763592), an
            // alias of the kernel's sample spacing, not a partial blur.
            if beside > 600 && inside < 60 {
                eprintln!(
                    "matrix: hyprland blurs strand-Dash by its layer rule: contrast {inside} \
                     under it, {beside} beside it"
                );
                break;
            }
            if Instant::now() >= deadline {
                if let Some(shots) = std::env::var_os("STRAND_SHOTS") {
                    let mut cmd = Command::new("grim");
                    if let Some(o) = &output {
                        cmd.args(["-o", o]);
                    }
                    let _ = cmd
                        .arg(PathBuf::from(shots).join("matrix-hyprland-layer-rule-failed.png"))
                        .status();
                }
                panic!(
                    "hyprland: the layer rule did not blur strand-Dash: contrast {inside} under \
                     it (want < 60; 306 unblurred), {beside} beside it (want > 600)\n{lua}"
                );
            }
        }
        for node in [DASH, STRIPES] {
            mgr.state_mut()
                .apply_surface_change(node, SurfaceChange::Removed);
        }
        let _ = mgr.dispatch_until(Duration::from_millis(300), |_| false);
    }
}

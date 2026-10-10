//! (M4) Tray menus end to end (design.md: "Popups are anchored
//! xdg\_popups that nest, which is how tray menus work"): `strand run` on
//! a headless sway with `tests/fixtures/tray_menu.strand`, the shell's
//! own tray host on a private `dbus-daemon`, and an app's tray item (a
//! zbus StatusNotifierItem) whose DBusMenu has a submenu.
//!
//! A left click activates the item at the icon's bottom-left corner on
//! the output; a right click opens its menu as a popup under the bar; the
//! keyboard walks it (Down to the submenu's row, Right opens the submenu
//! beside the row, level with it, Left closes it again), Return on a
//! submenu entry tells the app it was clicked and closes the menu, and
//! Escape closes it too, the app told each time.
//!
//! Shots of each state go to `$STRAND_SHOTS` when it is set. Skipped,
//! loudly, without sway, grim or dbus-daemon (CI sets
//! `STRAND_REQUIRE_SWAY` and `STRAND_REQUIRE_DBUS`).

mod support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_services::testing::PrivateBus;
use support::keyboard::Keyboard;
use support::pointer::Pointer;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SHELL: &str = include_str!("fixtures/tray_menu.strand");
const W: usize = 1280;
const H: usize = 720;
/// The bar's height in the fixture.
const BAR: usize = 36;

/// The fixture's colours.
const MENU: [u8; 3] = [0x30, 0x50, 0xa0];
const SUBMENU: [u8; 3] = [0x30, 0xa0, 0x50];
const SELECTED: [u8; 3] = [0xe0, 0xa0, 0x20];

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

/// A rectangle of pixels: `x0..x1` × `y0..y1`.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Bounds {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Img {
    fn ppm(ppm: &[u8]) -> Img {
        let mut nl = ppm.iter().enumerate().filter(|(_, b)| **b == b'\n');
        let (a, b, c) = (
            nl.next().unwrap().0,
            nl.next().unwrap().0,
            nl.next().unwrap().0,
        );
        let dims = std::str::from_utf8(&ppm[a + 1..b]).unwrap();
        let mut it = dims.split_whitespace().map(|v| v.parse::<usize>().unwrap());
        let (w, h) = (it.next().unwrap(), it.next().unwrap());
        Img {
            w,
            h,
            rgb: ppm[c + 1..c + 1 + w * h * 3].to_vec(),
        }
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// The bounds of the pixels `f` accepts in `ys` (rows), and how many
    /// there are.
    fn find(&self, ys: std::ops::Range<usize>, f: impl Fn([u8; 3]) -> bool) -> (usize, Bounds) {
        let mut n = 0;
        let mut b = Bounds {
            x0: usize::MAX,
            y0: usize::MAX,
            x1: 0,
            y1: 0,
        };
        for y in ys.start..ys.end.min(self.h) {
            for x in 0..self.w {
                if f(self.px(x, y)) {
                    n += 1;
                    b.x0 = b.x0.min(x);
                    b.y0 = b.y0.min(y);
                    b.x1 = b.x1.max(x + 1);
                    b.y1 = b.y1.max(y + 1);
                }
            }
        }
        (n, b)
    }

    /// The bounds of exactly `colour` below the bar, if at least `min`
    /// pixels have it.
    fn region(&self, colour: [u8; 3], min: usize) -> Option<Bounds> {
        self.region_in(colour, 0..self.w, min)
    }

    /// [`Img::region`] in columns `xs` only.
    fn region_in(&self, colour: [u8; 3], xs: std::ops::Range<usize>, min: usize) -> Option<Bounds> {
        let mut n = 0;
        let mut b = Bounds {
            x0: usize::MAX,
            y0: usize::MAX,
            x1: 0,
            y1: 0,
        };
        for y in BAR..self.h {
            for x in xs.start..xs.end.min(self.w) {
                if self.px(x, y) == colour {
                    n += 1;
                    b.x0 = b.x0.min(x);
                    b.y0 = b.y0.min(y);
                    b.x1 = b.x1.max(x + 1);
                    b.y1 = b.y1.max(y + 1);
                }
            }
        }
        (n >= min).then_some(b)
    }
}

/// The app's tray icon: pure magenta.
fn magenta(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] > 200
}

/// What the app was asked, in order.
type Calls = Arc<Mutex<Vec<String>>>;

/// A tray item with a 16×16 magenta pixmap and a DBusMenu at `/Menu`.
struct Item(Calls);

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, x: i32, y: i32) {
        self.0.lock().unwrap().push(format!("Activate at {x},{y}"));
    }
    fn secondary_activate(&self, x: i32, y: i32) {
        self.0
            .lock()
            .unwrap()
            .push(format!("SecondaryActivate at {x},{y}"));
    }
    fn context_menu(&self, x: i32, y: i32) {
        self.0
            .lock()
            .unwrap()
            .push(format!("ContextMenu at {x},{y}"));
    }
    #[zbus(property)]
    fn id(&self) -> String {
        "menus".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        "Menus".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let px = [255u8, 255, 0, 255];
        vec![(16, 16, px.repeat(16 * 16))]
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/Menu").unwrap()
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
}

/// A DBusMenu layout: revision, then the root `(ia{sv}av)`.
type Layout = (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>));

fn node(id: i32, label: &str, children: Vec<Value<'static>>) -> Value<'static> {
    let mut props: HashMap<String, Value<'static>> =
        HashMap::from([("label".to_string(), Value::from(label.to_string()))]);
    if !children.is_empty() {
        props.insert("children-display".into(), Value::from("submenu"));
    }
    Value::from((id, props, children))
}

/// The item's menu: `Open`, then `More` with `First` and `Second` in its
/// submenu.
struct Menu(Calls);

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(&self, _parent: i32, _depth: i32, _props: Vec<String>) -> Layout {
        let children = vec![
            node(1, "Open", vec![]),
            node(
                2,
                "More",
                vec![node(3, "First", vec![]), node(4, "Second", vec![])],
            ),
        ];
        let children = children
            .into_iter()
            .map(|c| OwnedValue::try_from(c).unwrap())
            .collect();
        (1, (0, HashMap::new(), children))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.0
            .lock()
            .unwrap()
            .push(format!("Event {id} {event_id}"));
    }

    fn about_to_show(&self, id: i32) -> bool {
        self.0.lock().unwrap().push(format!("AboutToShow {id}"));
        false
    }
}

struct Shell {
    dir: PathBuf,
    display: String,
    log: PathBuf,
    shots: Option<PathBuf>,
    n: std::cell::Cell<u32>,
    strand: Option<Proc>,
    _sway: Proc,
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.strand.take();
        if std::env::var_os("STRAND_KEEP_TMP").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl Shell {
    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
        ]
    }

    fn socket(&self) -> PathBuf {
        self.dir.join(&self.display)
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn shot(&self) -> Img {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", "HEADLESS-1"])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    /// A PNG of the output under `$STRAND_SHOTS`, numbered and named.
    fn keep(&self, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        let n = self.n.get() + 1;
        self.n.set(n);
        let _ = std::fs::create_dir_all(dir);
        let _ = Command::new("grim")
            .args(["-t", "png", "-o", "HEADLESS-1"])
            .arg(dir.join(format!("tray-menu-{n:02}-{name}.png")))
            .envs(self.env())
            .status();
    }

    /// Waits (bounded) until `look` at a fresh screenshot returns
    /// something, and returns it.
    fn wait<T>(&mut self, what: &str, look: impl Fn(&Img) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(v) = look(&self.shot()) {
                return v;
            }
            let exited = self
                .strand
                .as_mut()
                .is_some_and(|p| p.0.try_wait().unwrap().is_some());
            assert!(!exited, "strand exited: {}", self.log_text());
            assert!(
                Instant::now() < deadline,
                "never: {what}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Waits (bounded) until the app was asked `call`.
fn wait_call(calls: &Calls, call: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !calls.lock().unwrap().iter().any(|c| c == call) {
        assert!(
            Instant::now() < deadline,
            "the app was never asked {call}: {:?}",
            calls.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn count(calls: &Calls, call: &str) -> usize {
    calls.lock().unwrap().iter().filter(|c| *c == call).count()
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the tray menu test did not run ***\n"
            );
            return false;
        }
    }
    true
}

fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        format!("xwayland disable\noutput HEADLESS-1 resolution {W}x{H} position 0 0 scale 1\n"),
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("sway.log")).unwrap();
    let mut cmd = Command::new("sway");
    // SAFETY: the hook runs between fork and exec and makes one
    // async-signal-safe syscall.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd
        .arg("-c")
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("SWAYSOCK")
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let sway = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let display = names
            .iter()
            .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
        let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
        if let (Some(d), Some(i)) = (display, ipc) {
            let ok = Command::new("swaymsg")
                .args(["-t", "get_version"])
                .env("SWAYSOCK", dir.join(i))
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                return (sway, d.clone());
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn tray_menus_nest_walk_and_choose() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let dir = std::env::temp_dir().join(format!("strand-tray-menu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display) = sway(&dir);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("tray_menu.strand"), SHELL).unwrap();
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env(
                "STRAND_LOG",
                std::env::var("STRAND_TEST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .envs(bus.env())
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let mut sh = Shell {
        dir: dir.clone(),
        display,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };

    // The session has no watcher, so the shell is it; the app registers
    // its item, whose magenta icon joins the bar.
    let calls: Calls = Arc::default();
    let item_name = "org.kde.StatusNotifierItem-778-1";
    let item = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(item_name)
            .unwrap()
            .serve_at("/StatusNotifierItem", Item(calls.clone()))
            .unwrap()
            .serve_at("/Menu", Menu(calls.clone()))
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    assert!(bus.wait_for_name("org.kde.StatusNotifierWatcher", Duration::from_secs(10)));
    tokio
        .block_on(item.call_method(
            Some("org.kde.StatusNotifierWatcher"),
            "/StatusNotifierWatcher",
            Some("org.kde.StatusNotifierWatcher"),
            "RegisterStatusNotifierItem",
            &(item_name,),
        ))
        .unwrap();
    let icon = sh.wait("the tray icon", |img| {
        let (n, b) = img.find(0..BAR, magenta);
        (n >= 200).then_some(b)
    });
    sh.keep("icon");
    let (cx, cy) = (
        ((icon.x0 + icon.x1) / 2) as u32,
        ((icon.y0 + icon.y1) / 2) as u32,
    );
    let mut pointer = Pointer::new(&sh.socket());
    let mut keys = Keyboard::new(&sh.socket(), &dir);
    let (w, h) = (W as u32, H as u32);

    // A left click activates it, told the icon's bottom-left corner on
    // the output (the bar is at the output's top-left corner).
    pointer.click(cx, cy, w, h);
    let at = format!("Activate at {},{}", icon.x0, icon.y1);
    wait_call(&calls, &at);

    // A right click opens its menu: a popup under the bar, below the
    // icon, and the app is told it opens.
    pointer.right_click(cx, cy, w, h);
    let menu = sh.wait("the menu", |img| img.region(MENU, 2000));
    wait_call(&calls, "Event 0 opened");
    sh.keep("menu");
    assert!(menu.y0 >= BAR, "under the bar: {menu:?}");
    assert!(
        menu.x0 <= icon.x1 && icon.x0 <= menu.x1,
        "under its icon {icon:?}: {menu:?}"
    );

    // Down twice selects `More`, the second row.
    keys.press("Down");
    keys.press("Down");
    let row = sh.wait("the second row selected", |img| {
        img.region(SELECTED, 500).filter(|r| r.y0 >= menu.y0 + 24)
    });
    assert!(
        row.x0 >= menu.x0 && row.x1 <= menu.x1,
        "a row of the menu {menu:?}: {row:?}"
    );

    // Right opens its submenu beside the row: to its right, level with
    // it; the app is told the submenu opens.
    keys.press("Right");
    let sub = sh.wait("the submenu", |img| img.region(SUBMENU, 2000));
    wait_call(&calls, "Event 2 opened");
    sh.keep("submenu");
    assert!(sub.x0 >= menu.x1, "right of the menu {menu:?}: {sub:?}");
    assert!(
        sub.y0.abs_diff(row.y0) <= 2,
        "level with its row {row:?}: {sub:?}"
    );

    // Left closes the submenu; the menu stays.
    keys.press("Left");
    sh.wait("the submenu closed", |img| {
        (img.region(SUBMENU, 1).is_none() && img.region(MENU, 2000).is_some()).then_some(())
    });

    // Right again, then Down twice and Return choose `Second`: the app
    // is told it was clicked, and the menu closes (the app told).
    keys.press("Right");
    sh.wait("the submenu again", |img| img.region(SUBMENU, 2000));
    keys.press("Down");
    keys.press("Down");
    // (The menu's own row stays selected beside it.)
    sh.wait("the submenu's second row selected", |img| {
        img.region_in(SELECTED, sub.x0..sub.x1, 500)
            .filter(|r| r.y0 >= sub.y0 + 24)
    });
    keys.press("Return");
    wait_call(&calls, "Event 4 clicked");
    sh.wait("the menu closed", |img| {
        (img.region(MENU, 1).is_none() && img.region(SUBMENU, 1).is_none()).then_some(())
    });
    wait_call(&calls, "Event 0 closed");
    sh.keep("chosen");
    assert_eq!(count(&calls, "Event 3 clicked"), 0, "{:?}", calls);
    assert_eq!(count(&calls, "Event 1 clicked"), 0, "{:?}", calls);

    // Opened again right after a choice, it stays and holds the keys.
    // Sway sends the leave for the bar's released grab with the new
    // grab's enter, before any key that follows; it used to close the
    // reopened menu. So a key after the menu shows must move its
    // selection, with the menu still open and the app told of no close.
    pointer.right_click(cx, cy, w, h);
    let again = sh.wait("the menu again", |img| img.region(MENU, 2000));
    let deadline = Instant::now() + Duration::from_secs(10);
    while count(&calls, "Event 0 opened") < 2 {
        assert!(Instant::now() < deadline, "{:?}", calls);
        std::thread::sleep(Duration::from_millis(20));
    }
    // The shell's calls come in order on its one connection: every
    // close of the first menu is in by now.
    let closed = count(&calls, "Event 0 closed");
    let selected = |img: &Img| img.region_in(SELECTED, again.x0..again.x1, 500);
    let before = selected(&sh.shot());
    keys.press("Down");
    sh.wait("the reopened menu's selection moved", |img| {
        img.region(MENU, 2000)?;
        let now = selected(img)?;
        (Some(now) != before).then_some(())
    });
    assert_eq!(
        count(&calls, "Event 0 closed"),
        closed,
        "the reopened menu was closed: {calls:?}"
    );
    // Escape closes it, and the app is told once more.
    keys.press("Escape");
    sh.wait("the menu closed by Escape", |img| {
        img.region(MENU, 1).is_none().then_some(())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while count(&calls, "Event 0 closed") == closed {
        assert!(Instant::now() < deadline, "{:?}", calls);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(item);
}

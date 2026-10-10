//! design.md's bar (a) and OSD (d) on the real compositor and audio
//! services (M3), end to end: `strand run` without `STRAND_MOCK` on a
//! headless sway (`SWAYSOCK`: the sway IPC adapter) with a private
//! PipeWire (null sinks, WirePlumber). The bar shows sway's workspaces
//! (`for ws in workspaces.on(screen) { Dot ws }`: the occupied one and
//! the focused pill) and the focused window's title, and a click on a
//! dot switches sway's workspace (`ws.focus()`); a volume change made
//! with `wpctl` pops the OSD up; `strand set audio.sink.volume` (absolute
//! and `+5%`) lands in PipeWire, where `wpctl` reads it.
//!
//! Shots of each state are written to `$STRAND_SHOTS` when it is set.
//! Skipped, loudly, without sway, grim, PipeWire or WirePlumber (CI sets
//! `STRAND_REQUIRE_SWAY` and `STRAND_REQUIRE_PIPEWIRE`).

mod support;

#[path = "../../strand-services/tests/pipewire/mod.rs"]
mod pipewire;
#[allow(dead_code)]
#[path = "../../strand-services/tests/common/window.rs"]
mod window;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_services::testing::PrivateBus;
use support::pointer::Pointer;
use window::TestWindow;

const FILES: [(&str, &str); 3] = [
    (
        "theme.strand",
        include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
    ),
    (
        "bar.strand",
        include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
    ),
    (
        "osd.strand",
        include_str!("../../strand-compiler/tests/fixtures/osd.strand"),
    ),
];

const W: usize = 1280;
const H: usize = 720;
/// The bar's vertical middle: 8 px margin, 36 px high.
const BAR_Y: usize = 26;
/// The first workspace dot's left edge: the bar's 8 px margin and the
/// split's 12 px padding.
const DOTS_X: usize = 20;

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

    fn count(
        &self,
        xs: std::ops::Range<usize>,
        ys: std::ops::Range<usize>,
        f: impl Fn([u8; 3]) -> bool,
    ) -> usize {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| x < self.w && y < self.h && f(self.px(x, y)))
            .count()
    }

    /// The runs of accent (clearly blue) pixels along the bar's middle,
    /// in the workspace dots' part, as `(start, end)` x ranges.
    fn pills(&self) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut start = None;
        for x in DOTS_X - 4..DOTS_X + 120 {
            let on = blue(self.px(x, BAR_Y));
            match (on, start) {
                (true, None) => start = Some(x),
                (false, Some(s)) => {
                    runs.push((s, x));
                    start = None;
                }
                _ => {}
            }
        }
        runs
    }
}

/// The accent: clearly blue (the theme's accent on the light surface).
fn blue(p: [u8; 3]) -> bool {
    p[2] as i32 - p[0] as i32 > 40 && p[2] > 110
}

struct Shell {
    dir: PathBuf,
    display: String,
    ipc: PathBuf,
    log: PathBuf,
    shots: Option<PathBuf>,
    n: std::cell::Cell<u32>,
    strand: Option<Proc>,
    _sway: Proc,
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Shell {
    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
            ("SWAYSOCK", self.ipc.clone()),
        ]
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

    fn keep(&self, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        let n = self.n.get() + 1;
        self.n.set(n);
        let _ = std::fs::create_dir_all(dir);
        let _ = Command::new("grim")
            .args(["-t", "png", "-o", "HEADLESS-1"])
            .arg(dir.join(format!("wm-audio-{n:02}-{name}.png")))
            .envs(self.env())
            .status();
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Shell) -> bool) {
        self.wait_or(what, done, String::new);
    }

    /// [`Shell::wait`], its failure also saying `detail()` (what the test
    /// last saw), so a CI failure names its cause.
    fn wait_or(
        &mut self,
        what: &str,
        done: impl Fn(&Shell) -> bool,
        mut detail: impl FnMut() -> String,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            let exited = self
                .strand
                .as_mut()
                .is_some_and(|p| p.0.try_wait().unwrap().is_some());
            assert!(!exited, "strand exited: {}", self.log_text());
            assert!(
                Instant::now() < deadline,
                "never: {what}\n{}\n{}",
                detail(),
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn swaymsg(&self, args: &[&str]) -> String {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .unwrap();
        assert!(out.status.success(), "swaymsg {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The name of the workspace sway has focused.
    fn focused(&self) -> String {
        let out = self.swaymsg(&["-t", "get_workspaces", "-r"]);
        let v: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        v.iter()
            .find(|w| w["focused"] == true)
            .and_then(|w| w["name"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn cli(&self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "strand {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the compositor and audio shells test did not run ***\n"
            );
            return false;
        }
    }
    true
}

/// A headless sway in `dir`: the process, its display and its IPC socket.
fn sway(dir: &Path) -> (Proc, String, PathBuf) {
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
                return (sway, d.clone(), dir.join(i));
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn design_shells_on_the_real_compositor_and_audio() {
    if !tools() {
        return;
    }
    let Some(pw) = PipeWire::start("design_shells_on_the_real_compositor_and_audio") else {
        return;
    };
    pw.wait_for_defaults();
    pw.wpctl(&["set-volume", "@DEFAULT_AUDIO_SINK@", "0.5"]);
    // The other services (the tray) on a bus of their own.
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-wm-audio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, ipc) = sway(&dir);

    // A window on workspace 1, then workspace 2: two dots, 1 occupied
    // and 2 focused.
    let socket = dir.join(&display);
    let _window = TestWindow::open(&socket, "strand-e2e", "an e2e window");
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    for (name, text) in FILES {
        std::fs::write(config.join(name), text).unwrap();
    }
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("SWAYSOCK", &ipc)
            .env("PIPEWIRE_RUNTIME_DIR", pw.dir.path())
            .env_remove("PIPEWIRE_REMOTE")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("STRAND_LOG", "damage")
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
        ipc,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };
    // Workspace 1 (the window's) is focused: one pill, first.
    sh.wait("workspace 1's pill", |s| {
        let p = s.shot().pills();
        p.len() == 1 && p[0].0 <= DOTS_X + 1
    });
    // The focused window's title is in the bar next to it.
    sh.wait("the window's title", |s| {
        let img = s.shot();
        img.count(56..160, 18..34, |p| p[0] < 160) > 40
    });
    std::thread::sleep(Duration::from_millis(500));
    sh.keep("workspace-1-with-a-window");
    sh.swaymsg(&["workspace", "2"]);
    sh.wait("workspace 2 focused: the pill moves right", |s| {
        let p = s.shot().pills();
        p.len() == 1 && p[0].0 > DOTS_X + 6
    });
    std::thread::sleep(Duration::from_millis(500));
    sh.keep("workspace-2");
    let img = sh.shot();
    let pill = img.pills()[0];
    eprintln!("pill on workspace 2: {pill:?}");
    // Workspace 1's dot, occupied, left of the pill: not the bar's
    // background.
    let bg = img.px(DOTS_X - 6, BAR_Y);
    assert_ne!(img.px(DOTS_X + 4, BAR_Y), bg, "workspace 1's dot");

    // A click on workspace 1's dot: `ws.focus()` switches sway.
    let mut pointer = Pointer::new(&sh.dir.join(&sh.display));
    // (A new virtual pointer's first buttons reach no surface: one on
    // the empty desktop first.)
    pointer.click(1200, 700, W as u32, H as u32);
    std::thread::sleep(Duration::from_millis(200));
    pointer.click(DOTS_X as u32 + 4, BAR_Y as u32, W as u32, H as u32);
    pointer.motion(1200, 700, W as u32, H as u32);
    sh.wait("sway on workspace 1", |s| s.focused() == "1");
    sh.wait("the pill back on workspace 1", |s| {
        let p = s.shot().pills();
        p.len() == 1 && p[0].0 <= DOTS_X + 1
    });
    std::thread::sleep(Duration::from_millis(500));
    sh.keep("clicked-workspace-1");

    // The OSD's part of the screen, against the desktop.
    let osd = |img: &Img| {
        img.count(W / 2 - 140..W / 2 + 140, H - 140..H - 60, |p| {
            p != img.px(8, H / 2)
        })
    };
    assert!(osd(&sh.shot()) < 100, "no OSD before a change");

    // A volume change from outside (wpctl) pops the OSD up.
    pw.wpctl(&["set-volume", "@DEFAULT_AUDIO_SINK@", "0.3"]);
    sh.wait("the OSD", |s| osd(&s.shot()) > 1000);
    std::thread::sleep(Duration::from_millis(400));
    sh.keep("osd-wpctl-30");
    // ... and it goes 1.2 s after the last change.
    sh.wait("the OSD gone", |s| osd(&s.shot()) < 100);

    // `strand set audio.sink.volume`: absolute, then a relative step,
    // land in PipeWire.
    sh.cli(&["set", "audio.sink.volume", "0.55"]);
    sh.wait("PipeWire at 0.55", |_| {
        pw.volume_of("@DEFAULT_AUDIO_SINK@").0 == 0.55
    });
    sh.wait("the OSD for the set", |s| osd(&s.shot()) > 1000);
    std::thread::sleep(Duration::from_millis(400));
    sh.keep("osd-strand-set-55");
    sh.cli(&["set", "audio.sink.volume", "+5%"]);
    sh.wait("PipeWire at 0.6", |_| {
        (pw.volume_of("@DEFAULT_AUDIO_SINK@").0 - 0.6).abs() < 0.001
    });
    sh.cli(&["set", "audio.sink.muted", "true"]);
    sh.wait("PipeWire muted", |_| pw.volume_of("@DEFAULT_AUDIO_SINK@").1);
}

/// `on wm.config_reloaded { ... }` as a shell's only use of `wm` starts
/// the compositor service and fires on `swaymsg reload` (design.md's
/// change sources: "Compositor reload"): the box goes red, then green
/// after one reload, then blue after a second (the count is also shown).
#[test]
fn a_reload_handler_alone_hears_the_compositor_reload() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-wm-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, ipc) = sway(&dir);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("reloads.strand"),
        "state reloads = 0\n\
         on wm.config_reloaded { reloads += 1 }\n\
         bar Top {\n  edge: top; height: 32\n  row {\n    \
         box { size: 24; bg: #ff0000\n      \
         when reloads == 1 { bg: #00ff00 }\n      \
         when reloads >= 2 { bg: #0000ff }\n    }\n    \
         text join(\" \", \"reloads\", reloads)\n  }\n}\n",
    )
    .unwrap();
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("SWAYSOCK", &ipc)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
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
        ipc,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };
    // The box's pixels of one colour, in the bar.
    let of = |img: &Img, c: [u8; 3]| {
        img.count(0..200, 0..32, |p| {
            p.iter()
                .zip(c)
                .all(|(a, b)| (*a as i32 - b as i32).abs() < 24)
        })
    };
    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    sh.wait("the box, no reload yet", |s| of(&s.shot(), RED) > 300);
    // The reload event is not state: one sent before the service's
    // adapter subscribed is not heard, so the first is resent (3 s
    // apart) until it is.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        sh.swaymsg(&["reload"]);
        let heard = Instant::now() + Duration::from_secs(3);
        while Instant::now() < heard && of(&sh.shot(), GREEN) <= 300 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if of(&sh.shot(), GREEN) > 300 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no reload heard\n{}",
            sh.log_text()
        );
    }
    sh.keep("reloaded-once");
    sh.swaymsg(&["reload"]);
    sh.wait("two reloads", |s| of(&s.shot(), BLUE) > 300);
    sh.keep("reloaded-twice");
    assert_eq!(of(&sh.shot(), RED), 0);
}

/// `win.maximize()` and `win.fullscreen()` from a click, end to end on a
/// headless sway (the sway adapter): one button per window, red for
/// maximize, green for fullscreen. sway has no maximize, so the red click
/// logs `Unsupported` and changes nothing; the green one fullscreens the
/// window in sway's tree, which the window itself sees in its configure.
#[test]
fn window_buttons_maximize_and_fullscreen_on_sway() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-wm-winstate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, ipc) = sway(&dir);
    let win = TestWindow::open(&dir.join(&display), "strand-winstate", "a window");
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("winstate.strand"),
        "bar Top {\n  edge: top; height: 32\n  row {\n    \
         for w in windows.all {\n      \
         box { size: 24; bg: #ff0000; when w.maximized { bg: #000000 }\n        \
         on click { w.maximize() } }\n      \
         box { size: 24; bg: #00ff00; when w.fullscreen { bg: #000000 }\n        \
         on click { w.fullscreen() } }\n    }\n  }\n}\n",
    )
    .unwrap();
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("SWAYSOCK", &ipc)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
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
        ipc,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };
    // The middle of the button of colour `c` in the bar, once drawn.
    let button = |img: &Img, c: [u8; 3]| -> Option<(u32, u32)> {
        let near = |p: [u8; 3]| {
            p.iter()
                .zip(c)
                .all(|(a, b)| (*a as i32 - b as i32).abs() < 24)
        };
        let hits: Vec<(usize, usize)> = (0..32)
            .flat_map(|y| (0..400).map(move |x| (x, y)))
            .filter(|&(x, y)| near(img.px(x, y)))
            .collect();
        if hits.len() < 300 {
            return None;
        }
        let n = hits.len();
        let (sx, sy) = hits.iter().fold((0, 0), |(a, b), (x, y)| (a + x, b + y));
        Some(((sx / n) as u32, (sy / n) as u32))
    };
    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    sh.wait("both buttons", |s| {
        let img = s.shot();
        button(&img, RED).is_some() && button(&img, GREEN).is_some()
    });
    sh.keep("winstate-buttons");
    let img = sh.shot();
    let (max_at, full_at) = (button(&img, RED).unwrap(), button(&img, GREEN).unwrap());
    let fullscreen_mode = |s: &Shell| -> Option<i64> {
        let tree: serde_json::Value =
            serde_json::from_str(&s.swaymsg(&["-t", "get_tree", "-r"])).unwrap();
        fn find(n: &serde_json::Value) -> Option<i64> {
            if n["app_id"] == "strand-winstate" {
                return n["fullscreen_mode"].as_i64();
            }
            ["nodes", "floating_nodes"]
                .iter()
                .filter_map(|k| n[*k].as_array())
                .flatten()
                .find_map(find)
        }
        find(&tree)
    };
    assert_eq!(fullscreen_mode(&sh), Some(0));

    let mut pointer = Pointer::new(&sh.dir.join(&sh.display));
    // (A new virtual pointer's first buttons reach no surface: one on
    // the window first, which only focuses it.)
    pointer.click(640, 400, W as u32, H as u32);
    std::thread::sleep(Duration::from_millis(200));
    // `w.maximize()`: sway has no maximize.
    pointer.click(max_at.0, max_at.1, W as u32, H as u32);
    sh.wait("maximize refused", |s| {
        s.log_text().contains("sway has no maximize")
    });
    assert_eq!(fullscreen_mode(&sh), Some(0));
    assert!(!win.maximized.load(std::sync::atomic::Ordering::SeqCst));
    // `w.fullscreen()`: sway fullscreens the window.
    pointer.click(full_at.0, full_at.1, W as u32, H as u32);
    pointer.motion(640, 400, W as u32, H as u32);
    sh.wait("sway fullscreens the window", |s| {
        fullscreen_mode(s) == Some(1)
    });
    sh.wait("the window sees it", |_| {
        win.fullscreen.load(std::sync::atomic::Ordering::SeqCst)
    });
    sh.keep("winstate-fullscreen");
    drop(win);
}

/// (M4) `spectrum audio.sink { … }` in `strand run` on the real audio
/// service, end to end: at rest its bars are dots; a 1 kHz test tone
/// played to the default sink lifts its own bar, not the far ones (the FFT on the audio
/// thread, the bands fed to render through `run/feeds.rs` only while the
/// spectrum is visible), and when the tone stops the bars rest again.
#[test]
fn a_test_tone_lifts_a_spectrum_bar() {
    if !tools() {
        return;
    }
    let Some(pw) = PipeWire::start("a_test_tone_lifts_a_spectrum_bar") else {
        return;
    };
    pw.wait_for_defaults();
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-spectrum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, ipc) = sway(&dir);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("spectrum.strand"),
        "bar Top {\n  edge: top; height: 48\n  row {\n    \
         spectrum audio.sink { bars: 16; width: 320; height: 40; smooth: 0; color: #ff0000 }\n  \
         }\n}\n",
    )
    .unwrap();
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("SWAYSOCK", &ipc)
            .env("PIPEWIRE_RUNTIME_DIR", pw.dir.path())
            .env_remove("PIPEWIRE_REMOTE")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            // The audio thread's info lines (a meter failing and its
            // retry, PipeWire lost, the default sink read) say why a
            // spectrum never lifted (GitHub run 38065740030; m4-audit).
            .env("STRAND_LOG", "info")
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
        ipc,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };
    // Each bar's height in red pixels, left to right: runs of columns
    // with red in them (a bar at rest is a dot, so all 16 show).
    let bars = |img: &Img| {
        let mut out: Vec<usize> = Vec::new();
        let mut run = false;
        for x in 0..400.min(img.w) {
            let n = (0..48)
                .filter(|&y| {
                    let p = img.px(x, y);
                    p[0] > 160 && p[1] < 90 && p[2] < 90
                })
                .count();
            match (n > 0, run) {
                (true, false) => out.push(n),
                (true, true) => {
                    if let Some(h) = out.last_mut() {
                        *h = (*h).max(n);
                    }
                }
                _ => {}
            }
            run = n > 0;
        }
        out
    };
    let tallest = |img: &Img| bars(img).into_iter().max().unwrap_or(0);
    sh.wait("the spectrum at rest: dots", |s| {
        let b = bars(&s.shot());
        b.len() == 16 && b.iter().all(|h| (1..=4).contains(h))
    });
    sh.keep("spectrum-at-rest");
    // The default sink's name, from the `default` metadata.
    let meta = pw.metadata();
    let sink = meta
        .split("default.audio.sink")
        .nth(1)
        .and_then(|r| r.split("\"name\":\"").nth(1))
        .and_then(|r| r.split('"').next())
        .expect("a default sink")
        .to_string();
    let wav = dir.join("tone.wav");
    pipewire::sine_wav(&wav, 30.0, 1000.0, 0.5);
    let mut player = pw.play(&wav, &sink);
    // 1 kHz falls in band 34 of 64, so in bar 8 of 16 (the bands are
    // spaced evenly in pitch): that bar (or a neighbour, as the tone may
    // spill over a band edge) lifts, and bars about two octaves and more
    // away (bars 4 and under, up to 260 Hz; 12 and over, from 3.6 kHz)
    // stay under half height.
    let tone_bar = strand_services::audio::spectrum::band_of(1000.0) * 16
        / strand_services::audio::spectrum::BANDS;
    assert_eq!(tone_bar, 8);
    // The last bars seen, for the failure message.
    let seen = std::cell::RefCell::new(Vec::new());
    let check = |s: &Shell| {
        let b = bars(&s.shot());
        *seen.borrow_mut() = b.clone();
        if b.len() != 16 {
            return false;
        }
        let peak = (0..16).max_by_key(|&i| b[i]).unwrap_or(0);
        let far_low = (0..16)
            .filter(|&i| i <= tone_bar - 4 || i >= tone_bar + 4)
            .all(|i| b[i] < 20);
        peak.abs_diff(tone_bar) <= 1 && b[peak] > 20 && far_low
    };
    let detail = || {
        let player_state = match player.try_wait() {
            Ok(None) => "running".to_string(),
            Ok(Some(st)) => format!("exited: {st}"),
            Err(e) => format!("unknown: {e}"),
        };
        format!(
            "last bars: {:?}\npw-play {player_state}, linked to {:?} (sink {sink})",
            seen.borrow(),
            pw.linked_to("pw-play"),
        )
    };
    sh.wait_or("the tone's bar lifted, the far bars low", check, detail);
    sh.keep("spectrum-tone");
    let _ = player.kill();
    let _ = player.wait();
    sh.wait("the bars at rest again", |s| tallest(&s.shot()) <= 4);
}

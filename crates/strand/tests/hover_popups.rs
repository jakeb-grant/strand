//! (M6, decisions.md m6-w2-surface) Hover islands on mixed-scale
//! outputs. `strand run` on a headless sway with HEADLESS-1 (1280×720 at
//! scale 1, at 0,0) and HEADLESS-2 (1920×1080 at 1.5, 1280×720 logical,
//! right of it), running the owner's pattern from the test config: a
//! floating, shadowed bar whose items each open an island below them
//! while the pointer is on the item or the island.
//!
//! A virtual pointer moves between a bar item, its island and the other
//! output, and the test reads where each island shows, on which output,
//! and that it stays open while the pointer crosses the gap from its item
//! to it.
//!
//! Shots go to `$STRAND_SHOTS` when it is set. Skipped, loudly, without
//! sway or grim (CI sets `STRAND_REQUIRE_SWAY`).

mod support;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::pointer::Pointer;

/// The layout: two outputs of 1280×720 logical pixels side by side.
const LW: u32 = 2560;
const LH: u32 = 720;

/// Two outputs side by side: each one's mode in physical pixels and its
/// scale; HEADLESS-2 starts where HEADLESS-1 ends.
#[derive(Copy, Clone)]
struct Layout {
    one: (u32, u32, f64),
    two: (u32, u32, f64),
}

impl Layout {
    /// HEADLESS-1 1280×720 at 1, HEADLESS-2 1920×1080 at 1.5: both 1280
    /// logical pixels wide.
    const EVEN: Layout = Layout {
        one: (1280, 720, 1.0),
        two: (1920, 1080, 1.5),
    };
    /// (owner-testing.md bug 6) As the owner's monitors differ: HEADLESS-1
    /// 1920×1080 at 1 (eDP-1, DP-6), HEADLESS-2 3840×2160 at 1.5, 2560
    /// logical pixels wide (DP-8, DP-10).
    const MIXED: Layout = Layout {
        one: (1920, 1080, 1.0),
        two: (3840, 2160, 1.5),
    };

    /// HEADLESS-1's width in logical pixels: where HEADLESS-2 starts.
    fn split(self) -> u32 {
        (self.one.0 as f64 / self.one.2).round() as u32
    }

    /// The layout's logical extent, for the virtual pointer.
    fn extent(self) -> (u32, u32) {
        let w2 = (self.two.0 as f64 / self.two.2).round() as u32;
        let h1 = (self.one.1 as f64 / self.one.2).round() as u32;
        let h2 = (self.two.1 as f64 / self.two.2).round() as u32;
        (self.split() + w2, h1.max(h2))
    }
}

const ITEM_A: [u8; 3] = [0xff, 0x00, 0x00];
const ITEM_B: [u8; 3] = [0x00, 0x00, 0xff];
const ISLAND_A: [u8; 3] = [0x00, 0xff, 0x00];
const ISLAND_B: [u8; 3] = [0x00, 0xff, 0xff];

/// The owner's pattern (target/try/qs/strand/shell.strand): one island
/// at a time, chosen by which item is hovered, closing 200 ms after the
/// pointer leaves both the items and the islands. Item `a` is the
/// bar's first, `b` its last; the bar floats 4 px from the edges with a
/// shadow, as the owner's does.
const SHELL: &str = r#"
bar Top {
  edge: top; height: 36; margin: 4, 4, 0; radius: 16
  bg: #303030; shadow: 0 6px 24px #000000
  Bar
}

component Bar {
  state pop = ""
  let target = a.hover ? "a" : b.hover ? "b" : ""
  let over = target != "" || ia.hover || ib.hover
  on change target { if target != "" { pop = target } }
  on change over after 200ms { if !over { pop = "" } }
  split { pad: 4
    start {
      box { id: a; size: 28; bg: #ff0000
        popup { open: pop == "a"; margin: 8, 0, 0
          box { id: ia; width: 200; height: 120; bg: #00ff00 }
        }
      }
    }
    end {
      box { id: b; size: 28; bg: #0000ff
        popup { open: pop == "b"; margin: 8, 0, 0
          box { id: ib; width: 200; height: 120; bg: #00ffff }
        }
      }
    }
  }
}
"#;

/// The hover island without wiring (spec.md 6.3): a `trigger: hover`
/// popup in each item.
const TRIGGER: &str = r#"
bar Top {
  edge: top; height: 36; margin: 4, 4, 0; radius: 16
  bg: #303030; shadow: 0 6px 24px #000000
  split { pad: 4
    start { Item { c: #ff0000; island: #00ff00 } }
    end { Item { c: #0000ff; island: #00ffff } }
  }
}

component Item(c: color, island: color) {
  box { size: 28; bg: c
    popup { trigger: hover; margin: 8, 0, 0
      box { width: 200; height: 120; bg: island }
    }
  }
}
"#;

/// Two hover islands side by side at the start of the bar, `a` and then
/// `b` 8 px after it, each `ISLAND`: a `trigger: hover` popup, or (as
/// before M6) a popup inside the item's `if hover`.
const SIDE_BY_SIDE: &str = r#"
bar Top {
  edge: top; height: 36; margin: 4, 4, 0; radius: 16
  bg: #303030; shadow: 0 6px 24px #000000
  split { pad: 4
    start { gap: 8
      Item { c: #ff0000; island: #00ff00 }
      Item { c: #0000ff; island: #00ffff }
    }
  }
}

component Item(c: color, island: color) {
  box { size: 28; bg: c
    ISLAND
  }
}
"#;

const ITEM_C: [u8; 3] = [0xff, 0x00, 0xff];

/// The owner's travelling island (target/try/qs/strand/shell.strand,
/// `Status`): three items at the right end of the bar and one
/// `trigger: hover` popup over the group, wider than it by 160 on each
/// side, holding one card that springs to the item `it` names, clamped
/// inside the popup. The card is `ISLAND_A`; it is 304 wide, or 232
/// over `c`, and casts a shadow, so the popup reserves the shadow's reach
/// at every edge (its window geometry starts inside its buffer).
const TRAVEL: &str = r#"
bar Top {
  edge: top; height: 36; margin: 4, 4, 0; radius: 16
  bg: #303030; shadow: 0 6px 24px #000000
  split { pad: 4
    end { Status }
  }
}

component Status {
  state at = "a"
  let over = a.hover ? "a" : b.hover ? "b" : c.hover ? "c" : ""
  on change over { if over != "" { at = over } }
  let wide = at == "c" ? 232 : 304
  row { id: group; gap: 4; align: center
    box { id: a; size: 28; bg: #ff0000
      when at == "a" { id: it }
    }
    box { id: b; size: 28; bg: #0000ff
      when at == "b" { id: it }
    }
    box { id: c; size: 28; bg: #ff00ff
      when at == "c" { id: it }
    }
    popup { id: island; trigger: hover; grace: 250ms; margin: 8, 0, 0
      width: group.box.width + 320; height: 200
      box { place: absolute; height: 120; bg: #00ff00; shadow: 0 6px 24px #000000
            width: wide ~ spring(3000, 0.75)
            x: clamp(it.box.x + it.box.width / 2 - wide / 2 - island.box.x, 0, island.box.width - wide) ~ spring(3000, 0.75) }
    }
  }
}
"#;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A screenshot of one output, in its physical pixels.
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

    /// The bounds of exactly `colour`, if at least `min` pixels have it.
    fn region(&self, colour: [u8; 3], min: usize) -> Option<Bounds> {
        let mut n = 0;
        let mut b = Bounds {
            x0: usize::MAX,
            y0: usize::MAX,
            x1: 0,
            y1: 0,
        };
        for y in 0..self.h {
            for x in 0..self.w {
                let i = (y * self.w + x) * 3;
                if self.rgb[i..i + 3] == colour {
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

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn shot(&self, output: &str) -> Img {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    /// PNGs of both outputs under `$STRAND_SHOTS`, numbered and named.
    fn keep(&self, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        let n = self.n.get() + 1;
        self.n.set(n);
        let _ = std::fs::create_dir_all(dir);
        for output in ["HEADLESS-1", "HEADLESS-2"] {
            let _ = Command::new("grim")
                .args(["-t", "png", "-o", output])
                .arg(dir.join(format!("hover-{n:02}-{name}-{output}.png")))
                .envs(self.env())
                .status();
        }
    }

    /// Waits (bounded) until `look` at fresh shots of both outputs
    /// returns something, and returns it.
    fn wait<T>(&mut self, what: &str, look: impl Fn(&Img, &Img) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(v) = look(&self.shot("HEADLESS-1"), &self.shot("HEADLESS-2")) {
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

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the hover popup test did not run ***\n"
            );
            return false;
        }
    }
    true
}

/// Headless sway with the two outputs: HEADLESS-2 is made over IPC.
fn sway(dir: &Path, layout: Layout) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    let (w1, h1, s1) = layout.one;
    let (w2, h2, s2) = layout.two;
    let x2 = layout.split();
    std::fs::write(
        &cfg,
        format!(
            "xwayland disable\n\
             output HEADLESS-1 resolution {w1}x{h1} position 0 0 scale {s1}\n\
             output HEADLESS-2 resolution {w2}x{h2} position {x2} 0 scale {s2}\n"
        ),
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
            let sock = dir.join(i);
            let ok = Command::new("swaymsg")
                .args(["-t", "get_version"])
                .env("SWAYSOCK", &sock)
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                let (mode, x2, s2) = (format!("{w2}x{h2}"), x2.to_string(), s2.to_string());
                for args in [
                    &["create_output"][..],
                    &[
                        "output",
                        "HEADLESS-2",
                        "resolution",
                        &mode,
                        "position",
                        &x2,
                        "0",
                        "scale",
                        &s2,
                    ],
                ] {
                    let out = Command::new("swaymsg")
                        .args(args)
                        .env("SWAYSOCK", &sock)
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "swaymsg {args:?}");
                }
                return (sway, d.clone());
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start(name: &str, shell: &str) -> Option<Shell> {
    start_on(name, shell, Layout::EVEN)
}

fn start_on(name: &str, shell: &str, layout: Layout) -> Option<Shell> {
    if !tools() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("strand-hover-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display) = sway(&dir, layout);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("shell.strand"), shell).unwrap();
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
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    Some(Shell {
        dir,
        display,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    })
}

/// The centre of `b`, in layout coordinates, for an output at `x`
/// logical pixels drawn at `scale`.
fn centre(b: Bounds, x: u32, scale: f64) -> (u32, u32) {
    let c = |v0: usize, v1: usize| ((v0 + v1) as f64 / 2.0 / scale).round() as u32;
    (x + c(b.x0, b.x1), c(b.y0, b.y1))
}

/// Logical bounds of `b` on an output drawn at `scale`.
fn logical(b: Bounds, scale: f64) -> (f64, f64, f64, f64) {
    let l = |v: usize| v as f64 / scale;
    (l(b.x0), l(b.y0), l(b.x1), l(b.y1))
}

/// The test config's pattern: the owner's wiring (ids on every island,
/// a grace written by hand) opens each island under its item, on its own
/// output, at either scale, and keeps it open across the gap.
#[test]
fn hover_islands_open_under_their_items_on_mixed_scale_outputs() {
    let Some(mut sh) = start("islands", SHELL) else {
        return;
    };
    walk(&mut sh);
}

/// The same islands with no wiring (spec.md 6.3, decisions.md
/// m6-w2-surface): `popup { trigger: hover; … }` in each item, as level
/// 3 of the guide writes it. Each island opens as the pointer reaches
/// its item, stays across the gap (`HOVER_GRACE`) and under the
/// pointer, and closes once the pointer is away from both.
#[test]
fn hover_trigger_islands_need_no_wiring() {
    let Some(mut sh) = start("trigger-hover", TRIGGER) else {
        return;
    };
    walk(&mut sh);
}

/// The pointer going straight from item `a` to item `b` beside it.
/// With `trigger: hover`, `b`'s island replaces `a`'s at once, and only
/// `b`'s stays. An island inside `if hover` with no trigger behaves as
/// before M6: it closes as the pointer leaves its item, into the gap
/// below it or onto `b`.
#[test]
fn going_straight_from_item_to_item_swaps_the_islands() {
    for keep in [false, true] {
        let island = if keep {
            "popup { trigger: hover; margin: 8, 0, 0\n      box { width: 200; height: 120; bg: island }\n    }"
        } else {
            "if hover {\n      popup { margin: 8, 0, 0\n        box { width: 200; height: 120; bg: island }\n      }\n    }"
        };
        let shell = SIDE_BY_SIDE.replace("ISLAND", island);
        let name = if keep { "side-trigger" } else { "side-plain" };
        let Some(mut sh) = start(name, &shell) else {
            return;
        };
        let (a, b) = sh.wait("the items", |one, _| {
            Some((one.region(ITEM_A, 400)?, one.region(ITEM_B, 400)?))
        });
        let mut pointer = Pointer::new(&sh.dir.join(&sh.display));
        let (ax, ay) = centre(a, 0, 1.0);
        let (bx, by) = centre(b, 0, 1.0);
        pointer.motion(ax, ay, LW, LH);
        sh.wait("a's island", |one, _| one.region(ISLAND_A, 2000));
        if !keep {
            // Into the gap below `a`: its island is not held.
            pointer.motion(ax, ay + 18, LW, LH);
            sh.wait("a's plain island closes in the gap", |one, _| {
                one.region(ISLAND_A, 1).is_none().then_some(())
            });
            pointer.motion(ax, ay, LW, LH);
            sh.wait("a's island again", |one, _| one.region(ISLAND_A, 2000));
        }
        // Straight onto `b`: the first shot that shows b's island shows
        // no trace of a's (no grace overlap, review r2 minor 6).
        pointer.motion(bx, by, LW, LH);
        let a_left = sh.wait("b's island", |one, _| {
            one.region(ISLAND_B, 2000)?;
            Some(one.region(ISLAND_A, 1))
        });
        assert_eq!(a_left, None, "{name}: a's island beside b's");
        sh.keep(name);
        // It stays, alone.
        std::thread::sleep(Duration::from_millis(400));
        let one = sh.shot("HEADLESS-1");
        assert!(
            one.region(ISLAND_B, 2000).is_some(),
            "{name}: b's island stays"
        );
        assert!(one.region(ISLAND_A, 1).is_none(), "{name}: a's is gone");
    }
}

/// (owner-testing.md bug 6) The travelling island at the right end of a
/// bar stays on its output, under its items, on outputs of different
/// widths (the owner's are 1920 and 2560 logical pixels): HEADLESS-1
/// 1920 wide at scale 1, whose right edge meets HEADLESS-2, and
/// HEADLESS-2 2560 wide at 1.5, the layout's right edge. The popup is
/// wider than the room there; the card, clamped inside it by
/// `island.box`, lands whole on the output under each item: the popup
/// is anchored on the bar it opens from and placed inside its output,
/// and `it.box` and `island.box` are read on that bar.
#[test]
fn the_travelling_island_stays_on_its_output_at_the_right_edge() {
    travel("travel", TRAVEL);
}

/// The same island whose popup grows by 80 while it is over `c`: the
/// popup changes while open (`xdg_popup.reposition`, or made again on
/// sway 1.9, whose `xdg_wm_base` is version 2), and still lands inside
/// its output.
#[test]
fn a_repositioned_island_stays_on_its_output() {
    let shell = TRAVEL.replace(
        "width: group.box.width + 320",
        "width: group.box.width + (at == \"c\" ? 400 : 320)",
    );
    assert_ne!(shell, TRAVEL);
    travel("travel-grow", &shell);
}

fn travel(name: &str, shell: &str) {
    let layout = Layout::MIXED;
    let Some(mut sh) = start_on(name, shell, layout) else {
        return;
    };
    let (lw, lh) = layout.extent();
    let split = layout.split();
    let (a1, b1, c1, a2, b2, c2) = sh.wait("the items on both bars", |one, two| {
        Some((
            one.region(ITEM_A, 400)?,
            one.region(ITEM_B, 400)?,
            one.region(ITEM_C, 400)?,
            two.region(ITEM_A, 900)?,
            two.region(ITEM_B, 900)?,
            two.region(ITEM_C, 900)?,
        ))
    });
    let mut pointer = Pointer::new(&sh.dir.join(&sh.display));
    let cases = [
        ("c on HEADLESS-1", c1, 0, 1.0, 232.0, false),
        ("b on HEADLESS-1", b1, 0, 1.0, 304.0, false),
        ("a on HEADLESS-1", a1, 0, 1.0, 304.0, false),
        ("c on HEADLESS-2", c2, split, 1.5, 232.0, true),
        ("b on HEADLESS-2", b2, split, 1.5, 304.0, true),
        ("a on HEADLESS-2", a2, split, 1.5, 304.0, true),
    ];
    let mut failures = Vec::new();
    for (what, item, x, scale, wide, second) in cases {
        let (cx, cy) = centre(item, x, scale);
        pointer.motion(cx, cy, lw, lh);
        let (here_name, there_name) = if second {
            ("HEADLESS-2", "HEADLESS-1")
        } else {
            ("HEADLESS-1", "HEADLESS-2")
        };
        // Shown, on either output.
        sh.wait(what, |one, two| {
            one.region(ISLAND_A, 1)
                .or_else(|| two.region(ISLAND_A, 1))
                .map(|_| ())
        });
        // Settled: the springs are done well within this.
        std::thread::sleep(Duration::from_millis(700));
        sh.keep(what);
        let here = sh.shot(here_name);
        let card = here.region(ISLAND_A, 1);
        let there = sh.shot(there_name).region(ISLAND_A, 1);
        let (tx0, _, tx1, _) = logical(item, scale);
        let out_w = here.w as f64 / scale;
        let Some(card) = card else {
            failures.push(format!(
                "{what}: no card on its output (on the other: {there:?})"
            ));
            continue;
        };
        let (x0, _, x1, _) = logical(card, scale);
        eprintln!(
            "{what}: card {x0}..{x1} ({} wide), item {tx0}..{tx1}, output {out_w} wide; \
             on the other output: {there:?}",
            x1 - x0,
        );
        if there.is_some() {
            failures.push(format!(
                "{what}: the card shows on the other output: {there:?}"
            ));
        }
        if ((x1 - x0) - wide).abs() > 2.0 {
            failures.push(format!(
                "{what}: the card {x0}..{x1} is {} wide, not {wide}: cut off at the output's edge",
                x1 - x0
            ));
        }
        // Under its item: centred on it, or clamped to the popup's end
        // (an item near the output's edge), still over it.
        if !(x0 <= tx1 && tx0 <= x1) {
            failures.push(format!(
                "{what}: the card {x0}..{x1} is not under its item {tx0}..{tx1}"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}\n{}", sh.log_text());
}

/// (owner-testing.md bug 7) The tray's hand-off: two items, each with
/// its own `trigger: hover` popup with a `delay` and an `enter` pose,
/// whose content changes as it closes (the app re-sends its menu when
/// told it closed). The pointer goes straight from `a` to `b`.
const HANDOFF: &str = r#"
bar Top {
  edge: top; height: 36; margin: 4, 4, 0; radius: 16
  bg: #303030; shadow: 0 6px 24px #000000
  split { pad: 4
    start { gap: 8
      Item { c: #ff0000; island: #00ff00 }
      Item { c: #0000ff; island: #00ffff }
    }
  }
}

component Item(c: color, island: color) {
  state shut = 0
  on change menu.open { if !menu.open { shut += 1 } }
  box { size: 28; bg: c
    popup { id: menu; trigger: hover; delay: 100ms; margin: 8, 0, 0
      enter { opacity: 0; scale: 0.9 }
      box { width: 200; height: 120; bg: shut > 0 ? #ffffff : island }
    }
  }
}
"#;

/// (owner-testing.md bug 7) Going straight from one tray item to the
/// next, the old island draws nothing new and never comes back: it
/// plays its exit (its `enter` mirrored, spec.md 6.3) over its last
/// content (white would show the content its close changed), and its
/// surface is made once and goes once, while the new island enters
/// (its own surface, made once). Screens are sampled through the
/// hand-off and never show white. (The two cross-fade: a's exit and
/// b's `enter` overlap for about 250 ms, both faint for a few frames
/// in the middle.)
#[test]
fn a_hover_hand_off_draws_nothing_new_on_the_old_island() {
    let Some(mut sh) = start("handoff", HANDOFF) else {
        return;
    };
    let (a, b) = sh.wait("the items", |one, _| {
        Some((one.region(ITEM_A, 400)?, one.region(ITEM_B, 400)?))
    });
    let mut pointer = Pointer::new(&sh.dir.join(&sh.display));
    let (ax, ay) = centre(a, 0, 1.0);
    let (bx, by) = centre(b, 0, 1.0);
    pointer.motion(ax, ay, LW, LH);
    sh.wait("a's island", |one, _| one.region(ISLAND_A, 2000));
    std::thread::sleep(Duration::from_millis(400));
    pointer.motion(bx, by, LW, LH);
    // Below the bar: green (a's island), cyan (b's), white (a's island
    // after its close changed it), faded over the grey desktop or not.
    let classify = |img: &Img| {
        let (mut g, mut c, mut w) = (0, 0, 0);
        for y in 50..img.h {
            for x in 0..img.w {
                let i = (y * img.w + x) * 3;
                let (r, gg, bb) = (
                    img.rgb[i] as i32,
                    img.rgb[i + 1] as i32,
                    img.rgb[i + 2] as i32,
                );
                if gg > 30 && r * 3 < gg && bb * 3 < gg {
                    g += 1;
                } else if gg > 30 && bb > 30 && r * 3 < gg.min(bb) && (gg - bb).abs() < 20 {
                    c += 1;
                } else if r > 120 && (r - gg).abs() < 10 && (r - bb).abs() < 10 {
                    w += 1;
                }
            }
        }
        (g, c, w)
    };
    let t0 = Instant::now();
    let mut shots = Vec::new();
    while t0.elapsed() < Duration::from_millis(700) {
        let (g, c, w) = classify(&sh.shot("HEADLESS-1"));
        shots.push((t0.elapsed().as_millis(), g, c, w));
    }
    sh.wait("b's island", |one, _| one.region(ISLAND_B, 2000));
    std::thread::sleep(Duration::from_millis(400));
    eprintln!("{shots:?}");
    assert!(
        shots.iter().all(|(_, _, _, w)| *w == 0),
        "the old island drew its changed content: {shots:?}"
    );
    // Three surfaces were made in all (the bar, a's island, b's), and
    // only a's island went.
    let log = sh.log_text();
    let made: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("surface ") && l.contains(" on HEADLESS-1"))
        .collect();
    let gone = log.lines().filter(|l| l.contains(" detached")).count();
    assert_eq!(made.len(), 3, "the bar, a's island, b's island: {made:?}");
    assert_eq!(gone, 1, "a's island went once\n{log}");
}

/// Walks the pointer over each item, into its island and away.
fn walk(sh: &mut Shell) {
    // Both bars drew their items.
    let (a1, b1, a2, b2) = sh.wait("the items on both bars", |one, two| {
        Some((
            one.region(ITEM_A, 400)?,
            one.region(ITEM_B, 400)?,
            two.region(ITEM_A, 900)?,
            two.region(ITEM_B, 900)?,
        ))
    });
    sh.keep("bars");
    let mut pointer = Pointer::new(&sh.dir.join(&sh.display));

    // Each item's island opens under it, on its own output, entirely:
    // the scale-1 output's last item (right edge, next to the scale-1.5
    // output), the scale-1.5 output's first item (left edge, next to the
    // scale-1 output), and the other two.
    let cases = [
        ("b on HEADLESS-1", b1, 0, 1.0, ISLAND_B, false),
        ("a on HEADLESS-2", a2, 1280, 1.5, ISLAND_A, true),
        ("a on HEADLESS-1", a1, 0, 1.0, ISLAND_A, false),
        ("b on HEADLESS-2", b2, 1280, 1.5, ISLAND_B, true),
    ];
    for (what, item, x, scale, island, second) in cases {
        let (cx, cy) = centre(item, x, scale);
        pointer.motion(cx, cy, LW, LH);
        let (here, there) = sh.wait(what, |one, two| {
            let (here, there) = if second { (two, one) } else { (one, two) };
            Some((here.region(island, 2000)?, there.region(island, 1)))
        });
        sh.keep(what);
        assert_eq!(
            there, None,
            "{what}: the island shows on the other output too"
        );
        let (ix0, iy0, ix1, _) = logical(here, scale);
        let (tx0, _, tx1, ty1) = logical(item, scale);
        assert!(
            ix0 <= tx1 && tx0 <= ix1,
            "{what}: the island {here:?} is not under its item {item:?}"
        );
        assert!(
            (iy0 - (ty1 + 4.0 + 8.0)).abs() <= 2.0 || iy0 >= ty1,
            "{what}: the island starts at {iy0}, its item ends at {ty1}"
        );
        // The island is whole: 200 × 120 logical pixels on screen.
        let (w, h) = ((here.x1 - here.x0) as f64, (here.y1 - here.y0) as f64);
        assert!(
            (w - 200.0 * scale).abs() <= 2.0 && (h - 120.0 * scale).abs() <= 2.0,
            "{what}: the island is {w}×{h} physical pixels at {scale}"
        );

        // From the item down into its island, a few pixels at a time at
        // a hand's pace (12 steps of 15 ms), across the gap: it is still
        // open once the pointer is on it. (A screenshot per step would
        // take longer than the 200 ms a hand needs to cross the gap.)
        let (icx, icy) = centre(here, x, scale);
        let steps: i64 = 12;
        let lerp = |a: u32, b: u32, k: i64| (a as i64 + (b as i64 - a as i64) * k / steps) as u32;
        for k in 1..=steps {
            pointer.motion(lerp(cx, icx, k), lerp(cy, icy, k), LW, LH);
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(
            sh.shot(if second { "HEADLESS-2" } else { "HEADLESS-1" })
                .region(island, 2000)
                .is_some(),
            "{what}: the island closed on the way to it\n{}",
            sh.log_text()
        );
        // It stays while the pointer rests on it.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            sh.shot(if second { "HEADLESS-2" } else { "HEADLESS-1" })
                .region(island, 2000)
                .is_some(),
            "{what}: the island closed under the pointer\n{}",
            sh.log_text()
        );
        // Away, onto the other output's empty desktop: it closes.
        let away = if second { 640 } else { 1280 + 640 };
        pointer.motion(away, 400, LW, LH);
        sh.wait("the island closed", |one, two| {
            (one.region(island, 1).is_none() && two.region(island, 1).is_none()).then_some(())
        });
    }
}

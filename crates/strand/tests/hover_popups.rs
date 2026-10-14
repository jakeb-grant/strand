//! owner-testing.md bug 1 and gap 4: hover islands on mixed-scale
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

/// Gap 4's hover island without wiring: the item's own `hover`.
const IF_HOVER: &str = r#"
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
    if hover {
      popup { margin: 8, 0, 0
        box { width: 200; height: 120; bg: island }
      }
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
fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        "xwayland disable\n\
         output HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n\
         output HEADLESS-2 resolution 1920x1080 position 1280 0 scale 1.5\n",
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
                for args in [
                    &["create_output"][..],
                    &[
                        "output",
                        "HEADLESS-2",
                        "resolution",
                        "1920x1080",
                        "position",
                        "1280",
                        "0",
                        "scale",
                        "1.5",
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
    if !tools() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("strand-hover-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display) = sway(&dir);
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

/// The same islands with no wiring (gap 4): `if hover { popup { … } }`
/// in each item, as the guide's hover slider is written. The pointer on
/// an island keeps its item hovered, and the item keeps its hover for
/// `HOVER_GRACE` while the pointer crosses the gap, so each island opens,
/// stays across the gap and under the pointer, and closes once the
/// pointer is away from both.
#[test]
fn if_hover_islands_need_no_wiring() {
    let Some(mut sh) = start("if-hover", IF_HOVER) else {
        return;
    };
    walk(&mut sh);
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

        // From the item down into its island, a few pixels at a time
        // across the gap: it stays open the whole way.
        let (icx, icy) = centre(here, x, scale);
        let steps: i64 = 12;
        let lerp = |a: u32, b: u32, k: i64| (a as i64 + (b as i64 - a as i64) * k / steps) as u32;
        for k in 1..=steps {
            pointer.motion(lerp(cx, icx, k), lerp(cy, icy, k), LW, LH);
            std::thread::sleep(Duration::from_millis(15));
            let img = sh.shot(if second { "HEADLESS-2" } else { "HEADLESS-1" });
            assert!(
                img.region(island, 2000).is_some(),
                "{what}: the island closed at step {k} on the way to it\n{}",
                sh.log_text()
            );
        }
        // It stays while the pointer rests on it.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            sh.shot(if second { "HEADLESS-2" } else { "HEADLESS-1" })
                .region(island, 2000)
                .is_some(),
            "{what}: the island closed under the pointer"
        );
        // Away, onto the other output's empty desktop: it closes.
        let away = if second { 640 } else { 1280 + 640 };
        pointer.motion(away, 400, LW, LH);
        sh.wait("the island closed", |one, two| {
            (one.region(island, 1).is_none() && two.region(island, 1).is_none()).then_some(())
        });
    }
}

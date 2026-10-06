//! `strand run --demo` end to end on a headless sway: two 2560x1440 outputs
//! (the second at scale 1.25) within the 34 MB PSS budget, a bar on each
//! with its clock centred, no wakeups while idle, then a third output of
//! another width at scale 1 (hotplugged) whose bar is aligned to its own
//! width. Skipped, loudly, when sway or grim is not installed.
//! The 34 MB gate is asserted when the test runs in release (`cargo test
//! --release -p strand --test demo`); a debug run is held to a looser
//! debug ceiling. `scripts/m0-exit.sh` measures the full M0 gates on a
//! release build (a whole minute, the tick's damage).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Sway {
    _child: Proc,
    dir: PathBuf,
    display: String,
    ipc: PathBuf,
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Sway {
    fn start() -> Option<Self> {
        Self::start_as("demo")
    }

    /// A sway of its own for each test (`tag` keeps their directories
    /// apart when tests run in parallel).
    fn start_as(tag: &str) -> Option<Self> {
        for tool in ["sway", "swaymsg", "grim"] {
            if Command::new(tool).arg("--version").output().is_err() {
                // CI sets STRAND_REQUIRE_SWAY so a missing tool fails loudly
                // instead of passing as a skip.
                assert!(
                    std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                    "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
                );
                eprintln!(
                    "\n*** SKIPPED: {tool} is not installed; the M0 PSS, idle and alignment checks did not run ***\n"
                );
                return None;
            }
        }
        // Short: the IPC socket path must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 2560x1440 position 0 0 scale 1\n",
        )
        .unwrap();
        let log = std::fs::File::create(dir.join("sway.log")).unwrap();
        let child = Command::new("sway")
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &dir)
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
        let mut sway = Sway {
            _child: Proc(child),
            dir: dir.clone(),
            display: String::new(),
            ipc: PathBuf::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = names
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc) {
                sway.display = d.clone();
                sway.ipc = dir.join(i);
                if sway.msg(&["-t", "get_version"]).is_some() {
                    return Some(sway);
                }
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn grim(&self, args: &[&str], path: &Path) -> bool {
        Command::new("grim")
            .args(args)
            .arg(path)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Voluntary + involuntary context switches over every thread of `pid`.
fn switches(pid: u32) -> u64 {
    let mut total = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let status = std::fs::read_to_string(task.unwrap().path().join("status")).unwrap();
        for line in status.lines() {
            if line.contains("ctxt_switches:") {
                total += line
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
            }
        }
    }
    total
}

fn seconds_into_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 60
}

fn damage_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("strand: damage"))
        .map(String::from)
        .collect()
}

/// The M0 memory gate (`docs/design.md`: the build fails above 34 MB for
/// the two-monitor bar), held on a release build (`cargo test --release`).
const PSS_GATE_KB: u64 = 34 * 1024;

/// A debug build carries about 9 MB more than release (30–31 MB against
/// 21–22 MB in M0): its own, looser ceiling, so debug-only growth does not
/// pass for a budget breach. The gate itself is checked in release.
const DEBUG_PSS_CEILING_KB: u64 = 40 * 1024;

/// The PSS limit for the binary under test and its name.
fn pss_limit() -> (u64, &'static str) {
    if cfg!(debug_assertions) {
        (DEBUG_PSS_CEILING_KB, "debug ceiling")
    } else {
        (PSS_GATE_KB, "M0 gate")
    }
}

fn pss_kb(pid: u32) -> u64 {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    rollup
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap()
}

/// A screenshot of one output's top bar as RGB rows.
struct Shot {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Shot {
    fn take(sway: &Sway, output: &str) -> Shot {
        let path = sway.dir.join(format!("{output}.ppm"));
        assert!(
            sway.grim(&["-t", "ppm", "-o", output], &path),
            "grim {output}"
        );
        let ppm = std::fs::read(&path).unwrap();
        // Binary PPM: "P6\n<w> <h>\n255\n" then RGB.
        let mut nl = ppm.iter().enumerate().filter(|(_, b)| **b == b'\n');
        let (a, b, c) = (
            nl.next().unwrap().0,
            nl.next().unwrap().0,
            nl.next().unwrap().0,
        );
        let dims = std::str::from_utf8(&ppm[a + 1..b]).unwrap();
        let mut it = dims.split_whitespace().map(|v| v.parse::<usize>().unwrap());
        let (w, h) = (it.next().unwrap(), it.next().unwrap());
        Shot {
            w,
            h,
            rgb: ppm[c + 1..].to_vec(),
        }
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Dark (default text colour) pixels in columns `xs` of a red bar
    /// (`bar_h` rows).
    fn ink(&self, xs: std::ops::Range<usize>, bar_h: usize) -> usize {
        xs.into_iter()
            .map(|x| (0..bar_h).filter(|&y| self.px(x, y)[0] < 0x40).count())
            .sum()
    }

    /// Some pixel of column `x` in rows `ys` has the overlay panel's
    /// colour (`#1e1e2ef0`).
    fn overlay_at(&self, x: usize, ys: std::ops::Range<usize>) -> bool {
        ys.into_iter().any(|y| {
            let [r, g, b] = self.px(x, y);
            (0x16..=0x22).contains(&r) && (0x16..=0x22).contains(&g) && b >= r + 10
        })
    }

    /// Some pixel in columns `xs` of the bar (`bar_h` rows) is light text.
    fn lit(&self, xs: std::ops::Range<usize>, bar_h: usize) -> bool {
        xs.into_iter()
            .any(|x| (0..bar_h).any(|y| self.px(x, y)[0] > 0x90))
    }

    /// The bar shows its clock centred, its end text at the right edge and
    /// nothing in between (a layout aligned for another width would put
    /// the clock or the end text elsewhere).
    fn assert_aligned(&self, output: &str, scale: f64) {
        let bar_h = (32.0 * scale).round() as usize;
        assert!(self.h > bar_h, "{output}: {}x{}", self.w, self.h);
        let (w, c) = (self.w, self.w / 2);
        let near = (60.0 * scale) as usize;
        let edge = (110.0 * scale) as usize;
        assert_eq!(
            self.px(c, 1),
            [0x1e, 0x1e, 0x2e],
            "{output}: bar background"
        );
        assert!(
            self.lit(c - near..c + near, bar_h),
            "{output}: no clock at the centre"
        );
        assert!(
            self.lit(w - edge..w - 1, bar_h),
            "{output}: no end text at the edge"
        );
        assert!(
            !self.lit(edge..c - near, bar_h) && !self.lit(c + near..w - edge, bar_h),
            "{output}: text drawn away from start, centre and end"
        );
    }
}

#[test]
fn demo_bar_on_two_outputs_then_idle() {
    let Some(sway) = Sway::start() else {
        return;
    };
    sway.msg(&["create_output"]).unwrap();
    sway.msg(&[
        "output",
        "HEADLESS-2",
        "resolution",
        "2560x1440",
        "position",
        "2560",
        "0",
        "scale",
        "1.25",
    ])
    .unwrap();
    let log = sway.dir.join("strand.log");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(["run", "--demo"])
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_LOG", "damage")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut strand = Proc(child);
    let painted = |strand: &mut Proc, n: usize| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while damage_lines(&log).len() < n {
            assert!(
                strand.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            assert!(Instant::now() < deadline, "bars did not paint");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // One full frame per output: 2560×32 at 1.0, 2560×40 at 1.25.
    painted(&mut strand, 2);
    let boot = damage_lines(&log);
    assert!(
        boot.iter().any(|l| l.contains("buffer=2560x32 ")),
        "{boot:?}"
    );
    assert!(
        boot.iter().any(|l| l.contains("buffer=2560x40 ")),
        "{boot:?}"
    );
    // Let late text settle before looking.
    std::thread::sleep(Duration::from_millis(700));
    Shot::take(&sway, "HEADLESS-1").assert_aligned("HEADLESS-1", 1.0);
    Shot::take(&sway, "HEADLESS-2").assert_aligned("HEADLESS-2", 1.25);

    // The memory gate, on the two-monitor bar.
    let pss = pss_kb(pid);
    let (limit, what) = pss_limit();
    eprintln!("strand PSS with two 2560x1440 bars: {pss} kB ({what} {limit} kB)");
    assert!(pss <= limit, "PSS {pss} kB over the {limit} kB {what}");

    // Idle: no thread of the process wakes. Skip a window that would
    // contain a minute boundary (the clock tick is the one wakeup).
    if seconds_into_minute() >= 56 {
        std::thread::sleep(Duration::from_secs(6));
    }
    let frames = damage_lines(&log).len();
    let before = switches(pid);
    std::thread::sleep(Duration::from_secs(2));
    let after = switches(pid);
    assert_eq!(after - before, 0, "woke while idle");
    assert_eq!(damage_lines(&log).len(), frames, "painted while idle");

    // A third monitor, at scale 1 like the first but narrower: the shared
    // bar node needs a layout per width, and the first bar must not move.
    sway.msg(&["create_output"]).unwrap();
    sway.msg(&[
        "output",
        "HEADLESS-3",
        "resolution",
        "1920x1080",
        "position",
        "4608",
        "0",
        "scale",
        "1",
    ])
    .unwrap();
    // Its own first frame, not just any frame (a minute tick may land
    // meanwhile).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !damage_lines(&log)
        .iter()
        .any(|l| l.contains("buffer=1920x32 "))
    {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "no bar on HEADLESS-3: {:?}",
            damage_lines(&log)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));
    Shot::take(&sway, "HEADLESS-3").assert_aligned("HEADLESS-3", 1.0);
    Shot::take(&sway, "HEADLESS-1").assert_aligned("HEADLESS-1", 1.0);
    Shot::take(&sway, "HEADLESS-2").assert_aligned("HEADLESS-2", 1.25);
    drop(strand);
}

/// `strand run <dir>` on the hello bar of the design: the compiled config,
/// not the demo, with one bar per output (the `screens` service fed from
/// the surface layer's monitor hooks), and a bar for a monitor plugged in
/// later.
#[test]
fn strand_run_boots_the_hello_bar_on_every_output() {
    let Some(sway) = Sway::start_as("run") else {
        return;
    };
    sway.msg(&["create_output"]).unwrap();
    sway.msg(&[
        "output",
        "HEADLESS-2",
        "resolution",
        "2560x1440",
        "position",
        "2560",
        "0",
        "scale",
        "1.25",
    ])
    .unwrap();
    let config = sway.dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("hello_bar.strand"),
        include_str!("../../strand-compiler/tests/fixtures/hello_bar.strand"),
    )
    .unwrap();
    let log = sway.dir.join("strand.log");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("XDG_CACHE_HOME", sway.dir.join("cache"))
        .env("XDG_STATE_HOME", sway.dir.join("state"))
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_LOG", "damage")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut strand = Proc(child);
    let wait_for = |strand: &mut Proc, what: &str, buffer: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !damage_lines(&log).iter().any(|l| l.contains(buffer)) {
            assert!(
                strand.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "no {what}: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // `height: 32`: 2560×32 at 1.0, 2560×40 at 1.25, each its own bar.
    wait_for(&mut strand, "bar on HEADLESS-1", "buffer=2560x32 ");
    wait_for(&mut strand, "bar on HEADLESS-2", "buffer=2560x40 ");
    let surfaces: std::collections::BTreeSet<String> = damage_lines(&log)
        .iter()
        .filter_map(|l| l.split_whitespace().find(|w| w.starts_with("surface=")))
        .map(String::from)
        .collect();
    assert_eq!(surfaces.len(), 2, "{surfaces:?}");
    // Each bar draws its text (where in the bar is render's layout, M2):
    // pixels in the bar's rows unlike the desktop below it.
    std::thread::sleep(Duration::from_millis(700));
    for (output, scale) in [("HEADLESS-1", 1.0), ("HEADLESS-2", 1.25)] {
        let shot = Shot::take(&sway, output);
        let bar_h = (32.0 * scale) as usize;
        let desktop = shot.px(shot.w - 1, bar_h + 8);
        let drawn = (0..shot.w).any(|x| (0..bar_h).any(|y| shot.px(x, y) != desktop));
        assert!(drawn, "{output}: the bar drew nothing");
    }
    // A monitor plugged in later gets its own bar.
    sway.msg(&["create_output"]).unwrap();
    sway.msg(&[
        "output",
        "HEADLESS-3",
        "resolution",
        "1920x1080",
        "position",
        "4608",
        "0",
        "scale",
        "1",
    ])
    .unwrap();
    wait_for(&mut strand, "bar on HEADLESS-3", "buffer=1920x32 ");
    // Unplugged: its bar is parked (logic keeps it for a return), the
    // shell goes on.
    sway.msg(&["output", "HEADLESS-3", "unplug"]).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        strand.0.try_wait().unwrap().is_none(),
        "strand exited: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let errors: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("ERROR"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    drop(strand);
}

/// The bar of design.md (theme + bar, unchanged) on the mock desktop,
/// laid out by flex layout: workspace dots and the window title at the
/// start, the clock truly centred on the output, the battery at the end;
/// its shadow grows the layer surface past its box while the exclusive
/// zone still reserves margin + height (8 + 36). Set `STRAND_SHOTS` to a
/// directory to keep the screenshot.
#[test]
fn the_design_bar_is_laid_out_start_centre_end() {
    let Some(sway) = Sway::start_as("design") else {
        return;
    };
    let home = sway.dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    for (name, text) in [
        (
            "theme.strand",
            include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
        ),
        (
            "bar.strand",
            include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
        ),
    ] {
        std::fs::write(config.join(name), text).unwrap();
    }
    let log = sway.dir.join("strand.log");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&config)
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("XDG_CACHE_HOME", sway.dir.join("cache"))
        .env("XDG_STATE_HOME", sway.dir.join("state"))
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_MOCK", "desktop")
        .env("STRAND_MOCK_SCREEN", "HEADLESS-1")
        .env("STRAND_LOG", "damage")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut strand = Proc(child);
    // `shadow: $elevation.md` (0 2px 8px) reaches 13 px: 11 above the
    // box, 15 below, 13 each side; the margins move out by as much.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !damage_lines(&log)
        .iter()
        .any(|l| l.contains("buffer=2570x62 "))
    {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "no 2570x62 bar: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));
    // The exclusive zone: windows start below margin + height.
    let ws = sway.msg(&["-t", "get_workspaces"]).unwrap();
    let ws: serde_json::Value = serde_json::from_str(&ws).unwrap();
    assert_eq!(ws[0]["rect"]["y"], 44, "{ws}");
    let shot = Shot::take(&sway, "HEADLESS-1");
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(
            &["-g", "0,0 2560x70"],
            &PathBuf::from(dir).join("design_bar.png"),
        );
    }
    // Text and dots are dark on the light bar (the mock has no portal:
    // light scheme).
    // Inside the bar's box (8..2552 × 8..44), clear of its rounded ends.
    let dark =
        |x: usize| (14..38).any(|y| shot.px(x, y).iter().map(|c| *c as u32).sum::<u32>() < 300);
    let cols: Vec<usize> = (16..shot.w - 16).filter(|&x| dark(x)).collect();
    assert!(!cols.is_empty(), "nothing drawn on the bar");
    // The clock: the dark run nearest the output's centre.
    let mid = shot.w / 2;
    let (mut l, mut r) = (mid, mid);
    while l > 0 && (l - 60..l).any(&dark) {
        l -= 1;
    }
    while r < shot.w - 1 && (r..r + 60).any(&dark) {
        r += 1;
    }
    let centre = (l + r) as f64 / 2.0;
    assert!(
        (centre - mid as f64).abs() <= 3.0 && r - l > 40,
        "clock ink {l}..{r} is not centred on {mid}"
    );
    // Start: the dots and title begin at margin + pad (8 + 12); end: the
    // battery text ends before the tray icon (16 px, after a 12 px gap;
    // icons are not drawn yet), margin and pad: 2560 - 8 - 12 - 28.
    let first = cols[0];
    let last = *cols.last().unwrap();
    assert!((18..40).contains(&first), "start ink at {first}");
    assert!(
        (shot.w - 60..=shot.w - 48).contains(&last),
        "end ink at {last}"
    );
    // Nothing between the title and the clock, or the clock and the end.
    assert!(!(700..l - 1).any(&dark), "ink between start and centre");
    assert!(!(r + 1..1950).any(&dark), "ink between centre and end");
    let errors: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("ERROR") || l.contains("WARN"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    drop(strand);
}

/// The four design shells booted together (theme, bar, launcher, toasts
/// and OSD, unchanged but for the launcher starting open) on the mock
/// desktop: every surface paints at one size (no frame at an estimated
/// size first, however the surfaces' text and configures interleave),
/// the launcher's box is centred in the usable area below the bar
/// although its shadow reaches further down than up, a click inside it
/// keeps it open and a click on the bar (outside the usable area its
/// click-away catcher covers) closes it.
#[test]
fn the_design_launcher_is_centred_and_closes_on_click_away() {
    let Some(sway) = Sway::start_as("launcher") else {
        return;
    };
    let home = sway.dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    let launcher = include_str!("../../strand-compiler/tests/fixtures/launcher.strand")
        .replace("export state open = false", "export state open = true");
    for (name, text) in [
        (
            "theme.strand",
            include_str!("../../strand-compiler/tests/fixtures/theme.strand").to_string(),
        ),
        (
            "bar.strand",
            include_str!("../../strand-compiler/tests/fixtures/bar.strand").to_string(),
        ),
        ("launcher.strand", launcher),
        (
            "toasts.strand",
            include_str!("../../strand-compiler/tests/fixtures/toasts.strand").to_string(),
        ),
        (
            "osd.strand",
            include_str!("../../strand-compiler/tests/fixtures/osd.strand").to_string(),
        ),
    ] {
        std::fs::write(config.join(name), text).unwrap();
    }
    let log = sway.dir.join("strand.log");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&config)
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("XDG_CACHE_HOME", sway.dir.join("cache"))
        .env("XDG_STATE_HOME", sway.dir.join("state"))
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_MOCK", "desktop")
        .env("STRAND_MOCK_SCREEN", "HEADLESS-1")
        .env("STRAND_LOG", "damage")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut strand = Proc(child);
    // Two surfaces painted: the bar and the launcher.
    let deadline = Instant::now() + Duration::from_secs(30);
    let surfaces = |log: &Path| -> std::collections::BTreeMap<String, Vec<String>> {
        let mut m: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for l in damage_lines(log) {
            let mut it = l.split_whitespace();
            let s = it
                .find(|w| w.starts_with("surface="))
                .unwrap_or("")
                .to_string();
            let b = l
                .split_whitespace()
                .find(|w| w.starts_with("buffer="))
                .unwrap_or("")
                .to_string();
            m.entry(s).or_default().push(b);
        }
        m
    };
    while surfaces(&log).len() < 3 {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "no launcher: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));
    // Every frame of each surface at one buffer size: no frame at an
    // estimated size before its text arrived.
    for (s, sizes) in surfaces(&log) {
        assert!(
            sizes.iter().all(|b| *b == sizes[0]),
            "{s} changed size: {sizes:?}"
        );
    }
    let shot = Shot::take(&sway, "HEADLESS-1");
    let bright = |x: usize, y: usize| shot.px(x, y).iter().map(|c| *c as u32).sum::<u32>() > 450;
    // The launcher's box on a column clear of its text: its light rows
    // below the bar.
    let x = shot.w / 2 + 250;
    let rows: Vec<usize> = (60..shot.h).filter(|&y| bright(x, y)).collect();
    assert!(!rows.is_empty(), "no launcher drawn");
    let (top, bottom) = (rows[0], *rows.last().unwrap());
    // The usable area: below the bar's margin + height (8 + 36).
    let ws = sway.msg(&["-t", "get_workspaces"]).unwrap();
    let ws: serde_json::Value = serde_json::from_str(&ws).unwrap();
    let (uy, uh) = (
        ws[0]["rect"]["y"].as_f64().unwrap(),
        ws[0]["rect"]["height"].as_f64().unwrap(),
    );
    let centre = (top + bottom + 1) as f64 / 2.0;
    assert!(
        (centre - (uy + uh / 2.0)).abs() <= 2.0,
        "box {top}..{bottom} centred at {centre}, usable area {uy} + {uh}"
    );
    // A click inside keeps it open; one outside closes it.
    let mut pointer = pointer::Pointer::new(&sway.dir.join(&sway.display));
    let (w, h) = (shot.w as u32, shot.h as u32);
    pointer.click(x as u32, ((top + bottom) / 2) as u32, w, h);
    std::thread::sleep(Duration::from_millis(500));
    let again = Shot::take(&sway, "HEADLESS-1");
    assert!(
        again
            .px(x, (top + bottom) / 2)
            .iter()
            .map(|c| *c as u32)
            .sum::<u32>()
            > 450,
        "a click inside closed it"
    );
    // On the bar, clear of its text (between the clock and the end).
    pointer.click(w * 3 / 4, 26, w, h);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let after = Shot::take(&sway, "HEADLESS-1");
        if after
            .px(x, (top + bottom) / 2)
            .iter()
            .map(|c| *c as u32)
            .sum::<u32>()
            < 450
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a click on the bar did not close it"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(strand);
}

/// A virtual pointer on the sway seat (`zwlr_virtual_pointer_v1`): the
/// headless seat has no pointer of its own.
mod pointer {
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    pub struct Client;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(Client: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(Client: ignore ZwlrVirtualPointerV1);

    pub struct Pointer {
        _conn: Connection,
        queue: EventQueue<Client>,
        pointer: ZwlrVirtualPointerV1,
        time: u32,
    }

    impl Pointer {
        pub fn new(socket: &Path) -> Self {
            let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let qh = queue.handle();
            let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
            let pointer = manager.create_virtual_pointer(None, &qh, ());
            queue.roundtrip(&mut Client).unwrap();
            Self {
                _conn: conn,
                queue,
                pointer,
                time: 0,
            }
        }

        /// A left click at layout position (`x`, `y`) of a layout
        /// `w`×`h` logical pixels.
        pub fn click(&mut self, x: u32, y: u32, w: u32, h: u32) {
            self.time += 10;
            self.pointer.motion_absolute(self.time, x, y, w, h);
            self.pointer.frame();
            for state in [
                wl_pointer::ButtonState::Pressed,
                wl_pointer::ButtonState::Released,
            ] {
                self.time += 10;
                self.pointer.button(self.time, 0x110, state);
                self.pointer.frame();
            }
            self.queue.roundtrip(&mut Client).unwrap();
        }
    }
}

/// A monitor unplugged and back within 30 s is the same monitor
/// (`MonitorId`): its bar is parked while it is gone and comes back with
/// its state; SIGTERM then ends `strand run` cleanly, with a `persist`
/// value written less than the 250 ms debounce before it on disk.
#[test]
fn strand_run_keeps_a_replugged_monitors_bar() {
    let Some(sway) = Sway::start_as("replug") else {
        return;
    };
    sway.msg(&["create_output"]).unwrap();
    sway.msg(&[
        "output",
        "HEADLESS-2",
        "resolution",
        "1920x1080",
        "position",
        "2560",
        "0",
        "scale",
        "1",
    ])
    .unwrap();
    let config = sway.dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    // A click grows the bar: its buffer height shows the bar's `n`.
    std::fs::write(
        config.join("bar.strand"),
        "state total = 0 persist\nbar Top {\n  state n = 0\n  height: n > 0 ? 40 : 32\n  on click {\n    n += 1\n    total += 1\n  }\n  text join(\" \", screen.name, n, total)\n}\n",
    )
    .unwrap();
    let log = sway.dir.join("strand.log");
    let state = sway.dir.join("state");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("XDG_CACHE_HOME", sway.dir.join("cache"))
        .env("XDG_STATE_HOME", &state)
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_LOG", "damage,info")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut strand = Proc(child);
    let text = || std::fs::read_to_string(&log).unwrap_or_default();
    let wait = |strand: &mut Proc, what: &str, done: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(&text()) {
            assert!(
                strand.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                text()
            );
            assert!(Instant::now() < deadline, "no {what}: {}", text());
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // `surface N on HEADLESS-2` lines, in order.
    let attached = |log: &str| -> Vec<String> {
        log.lines()
            .filter(|l| l.ends_with(" on HEADLESS-2"))
            .filter_map(|l| l.split("surface ").nth(1))
            .filter_map(|l| l.split_whitespace().next())
            .map(String::from)
            .collect()
    };
    let damage_after = |log: &str, from: usize, buffer: &str| {
        log.lines()
            .filter(|l| l.starts_with("strand: damage"))
            .skip(from)
            .any(|l| l.contains(buffer))
    };
    let damage_count = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("strand: damage"))
            .count()
    };
    wait(&mut strand, "bars", &|l| {
        damage_after(l, 0, "buffer=2560x32 ") && damage_after(l, 0, "buffer=1920x32 ")
    });
    let first = attached(&text());
    assert_eq!(first.len(), 1, "{}", text());
    // Click HEADLESS-2's bar: the layout is 4480×1440, HEADLESS-2 starts
    // at x = 2560.
    let mut pointer = pointer::Pointer::new(&sway.dir.join(&sway.display));
    // The seat gains a pointer: give strand a moment to bind it.
    std::thread::sleep(Duration::from_millis(300));
    pointer.click(2560 + 100, 10, 4480, 1440);
    wait(&mut strand, "the click", &|l| {
        damage_after(l, 0, "buffer=1920x40 ")
    });
    assert!(
        !damage_after(&text(), 0, "buffer=2560x40 "),
        "the other bar has its own state"
    );
    // Disabled: the output's global goes, and its bar's surface with it.
    sway.msg(&["output", "HEADLESS-2", "disable"]).unwrap();
    let detached = format!("surface {} detached", first[0]);
    wait(&mut strand, "the detach", &|l| l.contains(&detached));
    // Enabled again within 30 s: the same monitor, its bar back with
    // `n` kept (40 px, never the fresh 32 px).
    let mark = damage_count(&text());
    sway.msg(&["output", "HEADLESS-2", "enable"]).unwrap();
    wait(&mut strand, "the bar back", &|l| {
        attached(l).len() == 2 && damage_after(l, mark, "buffer=1920x40 ")
    });
    assert!(
        !damage_after(&text(), mark, "buffer=1920x32 "),
        "the bar came back fresh: {}",
        text()
    );
    // Another click (n = 2, total = 2), then SIGTERM straight after it
    // is drawn: the run ends cleanly and `total` reached the disk.
    let mark = damage_count(&text());
    pointer.click(2560 + 100, 10, 4480, 1440);
    wait(&mut strand, "the second click", &|l| {
        l.lines()
            .filter(|l| l.starts_with("strand: damage"))
            .skip(mark)
            .any(|l| l.contains("buffer=1920x40 "))
    });
    let pid = strand.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = strand.0.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "strand ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}: {}", text());
    let stored = std::fs::read_to_string(state.join("strand/persist/bar.total")).unwrap();
    assert!(stored.contains('2'), "{stored:?}");
    let errors: Vec<String> = text()
        .lines()
        .filter(|l| l.contains("ERROR"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

/// `strand watch --json`'s events, read on a thread of their own.
struct Watch {
    _child: Proc,
    events: std::sync::mpsc::Receiver<serde_json::Value>,
}

impl Watch {
    fn start(sway: &Sway) -> Watch {
        let mut child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["watch", "--json"])
            .env("XDG_RUNTIME_DIR", &sway.dir)
            .env("WAYLAND_DISPLAY", &sway.display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        Watch {
            _child: Proc(child),
            events,
        }
    }

    /// Wait for `{"event": "watching"}`: subscribed, no event missed.
    fn ready(&self) {
        loop {
            let ev = self
                .events
                .recv_timeout(Duration::from_secs(20))
                .expect("strand watch never subscribed");
            if ev["event"] == "watching" {
                return;
            }
        }
    }

    /// The next reload event (20 s at most).
    fn next(&self) -> serde_json::Value {
        loop {
            let ev = self
                .events
                .recv_timeout(Duration::from_secs(20))
                .expect("no reload event");
            if ev["event"] == "reload" {
                return ev;
            }
        }
    }
}

/// Save `text` to `path` the way editors with atomic saves do: a
/// temporary file renamed over it.
fn save(path: &Path, text: &str) {
    let tmp = path.with_extension("strand.tmp~");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// The acceptance run of live reload (M1): the hello bar from a config
/// directory, then a token edit, a prop edit, a node added and removed,
/// and a broken save then its fix, each reloaded live with the bar's
/// state kept (its click count keeps it 40 px tall; a fresh bar is 32
/// px), checked through `strand watch --json` and with grim's pixels.
/// The broken save keeps the last good bar on screen and opens the error
/// overlay; the fix closes it. `strand reload` answers once its reload is
/// done.
#[test]
fn strand_run_reloads_live_with_state_kept() {
    let Some(sway) = Sway::start_as("live") else {
        return;
    };
    let config = sway.dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("bar.strand");
    let hello = |bg_token: &str, bg_prop: &str, extra: &str| {
        format!(
            "tokens base {{ bar.bg: {bg_token} }}\n\
             bar Top {{\n\
             \x20 state n = 0\n\
             \x20 edge: top; height: n > 0 ? 40 : 32\n\
             \x20 bg: {bg_prop}\n\
             \x20 on click {{ n += 1 }}\n\
             \x20 split {{\n\
             \x20   start  {{ text windows.focused?.title ?? \"\" }}\n\
             \x20   center {{ text clock.format(\"%H:%M\") }}\n\
             \x20   end    {{ text pct(battery.percent) }}\n\
             {extra}\
             \x20 }}\n\
             }}\n"
        )
    };
    std::fs::write(&file, hello("#204080", "$bar.bg", "")).unwrap();
    let log = sway.dir.join("strand.log");
    let child = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("XDG_CACHE_HOME", sway.dir.join("cache"))
        .env("XDG_STATE_HOME", sway.dir.join("state"))
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("STRAND_LOG", "damage,info")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut strand = Proc(child);
    let text = || std::fs::read_to_string(&log).unwrap_or_default();
    let damage_count = || damage_lines(&log).len();
    let damage_after = |from: usize, buffer: &str| {
        damage_lines(&log)
            .iter()
            .skip(from)
            .any(|l| l.contains(buffer))
    };
    let wait = |strand: &mut Proc, what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(
                strand.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                text()
            );
            assert!(Instant::now() < deadline, "no {what}: {}", text());
            std::thread::sleep(Duration::from_millis(30));
        }
    };
    wait(&mut strand, "the bar", &|| {
        damage_after(0, "buffer=2560x32 ")
    });
    // The bar's colour, away from its text.
    let bg = |sway: &Sway| Shot::take(sway, "HEADLESS-1").px(1800, 1);
    let settle = || std::thread::sleep(Duration::from_millis(400));
    settle();
    assert_eq!(bg(&sway), [0x20, 0x40, 0x80]);
    // A click: n = 1, the bar grows to 40 px.
    let mut pointer = pointer::Pointer::new(&sway.dir.join(&sway.display));
    std::thread::sleep(Duration::from_millis(300));
    pointer.click(1800, 10, 2560, 1440);
    wait(&mut strand, "the click", &|| {
        damage_after(0, "buffer=2560x40 ")
    });
    let watch = Watch::start(&sway);
    // Wait until the watcher is subscribed (its `{"ok": true}` read).
    watch.ready();
    let mark = damage_count();
    let fresh = |from: usize| damage_after(from, "buffer=2560x32 ");

    // 1. A token edit: the table swaps, the bar keeps its state.
    save(&file, &hello("#208040", "$bar.bg", ""));
    let ev = watch.next();
    assert_eq!(ev["classes"], serde_json::json!(["token"]), "{ev}");
    settle();
    assert_eq!(bg(&sway), [0x20, 0x80, 0x40]);

    // 2. A prop edit: patched in place.
    save(&file, &hello("#208040", "#802020", ""));
    let ev = watch.next();
    assert_eq!(ev["classes"], serde_json::json!(["prop"]), "{ev}");
    settle();
    assert_eq!(bg(&sway), [0x80, 0x20, 0x20]);

    // 3. A node added, then removed: more ink on the bar, then exactly
    // as before. The added `end` section lays out in the split's end
    // column, at the bar's right end: start on a fresh minute so the
    // clock does not tick between the shots, and keep away from the
    // pointer.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 60;
    if secs > 50 {
        std::thread::sleep(Duration::from_secs(61 - secs));
    }
    let end_ink = |sway: &Sway| Shot::take(sway, "HEADLESS-1").ink(1900..2560, 40);
    let before = end_ink(&sway);
    let added = "    end { text \"added\" }\n";
    save(&file, &hello("#208040", "#802020", added));
    let ev = watch.next();
    assert_eq!(ev["classes"], serde_json::json!(["node-added"]), "{ev}");
    settle();
    let with = end_ink(&sway);
    assert!(
        with > before,
        "no ink for the added node: {with} <= {before}"
    );
    save(&file, &hello("#208040", "#802020", ""));
    let ev = watch.next();
    assert_eq!(ev["classes"], serde_json::json!(["node-removed"]), "{ev}");
    settle();
    assert_eq!(end_ink(&sway), before, "the removed node's ink stays");

    // 4. Broken: held back, the last good bar stays; after 250 ms the
    // overlay (a 960 px wide panel) opens.
    let surfaces_before = text().matches("surface ").count();
    save(&file, &hello("#208040", "#802020", "    txet \"oops\"\n"));
    let ev = watch.next();
    assert_eq!(ev["held"].as_array().map(Vec::len), Some(1), "{ev}");
    let diags = ev["diagnostics"].as_array().unwrap();
    assert!(
        diags
            .iter()
            .any(|d| d["severity"] == "error" && d["help"] == "did you mean `text`?"),
        "{ev}"
    );
    wait(&mut strand, "the overlay", &|| {
        damage_lines(&log).iter().any(|l| l.contains("buffer=960x"))
    });
    settle();
    assert_eq!(bg(&sway), [0x80, 0x20, 0x20], "the last good bar runs");
    // The panel is centred below the bar: its left padding, x = 805.
    let overlay_shown = |sway: &Sway| Shot::take(sway, "HEADLESS-1").overlay_at(805, 40..400);
    assert!(overlay_shown(&sway), "no overlay pixels");

    // 5. Fixed: committed, the overlay goes.
    save(&file, &hello("#208040", "#802020", "    text \"fixed\"\n"));
    let ev = watch.next();
    assert_eq!(ev["held"], serde_json::json!([]), "{ev}");
    assert_eq!(ev["classes"], serde_json::json!(["node-added"]), "{ev}");
    wait(&mut strand, "the overlay to close", &|| {
        text().matches(" detached").count() >= 1
    });
    assert!(text().matches("surface ").count() > surfaces_before);
    settle();
    assert!(!overlay_shown(&sway), "the overlay is still drawn");

    // Through it all the bar kept n: never 32 px again.
    assert!(!fresh(mark), "a reload reset the bar: {}", text());
    // `strand reload` answers once its reload is done.
    let out = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("reload")
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .env("WAYLAND_DISPLAY", &sway.display)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("reload ("));
    let errors: Vec<String> = text()
        .lines()
        .filter(|l| l.contains("ERROR"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

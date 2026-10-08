//! `strand run --demo` end to end on a headless sway: two 2560x1440 outputs
//! (the second at scale 1.25) within the PSS budget (34 MB target, 38 MB
//! ceiling), a bar on each with its clock centred, no wakeups while idle,
//! then a third output of another width at scale 1 (hotplugged) whose bar
//! is aligned to its own width. Skipped, loudly, when sway or grim is not
//! installed. The 38 MB ceiling (34 MB target, warned) is asserted when the
//! test runs in release (`cargo test --release -p strand --test demo`); a
//! debug run is held to a looser debug ceiling. `scripts/m0-exit.sh`
//! measures the full M0 gates on a
//! release build (a whole minute, the tick's damage).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod support;
use support::{keyboard, pointer};

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

/// Context switches per thread of `pid`, by `tid comm`.
fn per_thread(pid: u32) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let path = task.unwrap().path();
        let name = format!(
            "{} {}",
            path.file_name().unwrap().to_string_lossy(),
            std::fs::read_to_string(path.join("comm"))
                .unwrap_or_default()
                .trim()
        );
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        let n = status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum();
        out.insert(name, n);
    }
    out
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

/// The memory gate (`docs/design.md`: the two-monitor bar aims at 34 MB and
/// the build fails above 38 MB), held on a release build (`cargo test
/// --release`).
const PSS_TARGET_KB: u64 = 34 * 1024;
const PSS_GATE_KB: u64 = 38 * 1024;

/// A debug build carries about 9 MB more than release (30–31 MB against
/// 21–22 MB in M0): its own, looser ceiling, so debug-only growth does not
/// pass for a budget breach. The gate itself is checked in release.
const DEBUG_PSS_CEILING_KB: u64 = 40 * 1024;

/// The PSS limit for the binary under test and its name.
fn pss_limit() -> (u64, &'static str) {
    if cfg!(debug_assertions) {
        (DEBUG_PSS_CEILING_KB, "debug ceiling")
    } else {
        (PSS_GATE_KB, "38 MB ceiling")
    }
}

/// A release measurement above the 34 MB target: a warning (a CI
/// annotation), not a failure.
fn warn_over_target(what: &str, pss: u64) {
    if !cfg!(debug_assertions) && pss > PSS_TARGET_KB {
        eprintln!("{what}: PSS {pss} kB is over the {PSS_TARGET_KB} kB target");
        if std::env::var_os("GITHUB_ACTIONS").is_some() {
            println!(
                "\n::warning title=memory over target::{what}: PSS {pss} kB is over the {PSS_TARGET_KB} kB target (the build fails above 38 MB)"
            );
        }
    }
}

/// What a PSS failure needs to be read on a machine we cannot log into:
/// the rollup, the process's THP state, the system's THP modes and the
/// largest mappings.
fn memory_report(pid: u32) -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_else(|e| format!("{p}: {e}\n"));
    let mut out = read(&format!("/proc/{pid}/smaps_rollup"));
    out.extend(
        read(&format!("/proc/{pid}/status"))
            .lines()
            .filter(|l| l.starts_with("THP") || l.starts_with("Vm") || l.starts_with("Rss"))
            .map(|l| format!("{l}\n")),
    );
    let thp = "/sys/kernel/mm/transparent_hugepage";
    out.push_str(&format!(
        "{thp}/enabled: {}",
        read(&format!("{thp}/enabled"))
    ));
    if let Ok(dir) = std::fs::read_dir(thp) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("hugepages-") {
                let mode = read(&format!("{thp}/{name}/enabled"));
                out.push_str(&format!("{name}: {mode}"));
            }
        }
    }
    // The ten mappings with the most PSS.
    let smaps = read(&format!("/proc/{pid}/smaps"));
    let mut maps: Vec<(u64, u64, String)> = Vec::new();
    let mut head = String::new();
    let kb = |v: &str| {
        v.split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    for l in smaps.lines() {
        if l.split_whitespace()
            .next()
            .is_some_and(|w| w.contains('-') && !w.ends_with(':'))
        {
            head = l.to_string();
        } else if let Some(v) = l.strip_prefix("Pss:") {
            maps.push((kb(v), 0, head.clone()));
        } else if let Some(v) = l.strip_prefix("AnonHugePages:")
            && let Some(m) = maps.last_mut()
        {
            m.1 = kb(v);
        }
    }
    maps.sort_by_key(|m| std::cmp::Reverse(m.0));
    for (kb, huge, m) in maps.iter().take(10) {
        let huge = if *huge > 0 {
            format!(" ({huge} kB huge)")
        } else {
            String::new()
        };
        out.push_str(&format!("{kb:>7} kB{huge}  {m}\n"));
    }
    out
}

/// strand turns transparent huge pages off for itself (`main`): THP in
/// `always` mode otherwise fills mimalloc's arenas with 2 MiB pages.
fn assert_thp_off(pid: u32) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let line = status.lines().find(|l| l.starts_with("THP_enabled:"));
    // Kernels before 6.x have no such line; nothing to check there.
    if let Some(l) = line {
        assert_eq!(l.split_whitespace().nth(1), Some("0"), "THP is on: {l}");
    }
    // Off from before the first allocation (`main.rs`, `NO_THP`): with
    // THP `always`, pages the runtime touched before `main` were huge.
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    let huge = rollup
        .lines()
        .find_map(|l| l.strip_prefix("AnonHugePages:"))
        .map(str::trim);
    assert!(
        huge.is_none_or(|h| h == "0 kB"),
        "huge pages resident: {huge:?}\n{}",
        memory_report(pid)
    );
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
    assert_thp_off(pid);
    let pss = pss_kb(pid);
    let (limit, what) = pss_limit();
    eprintln!("strand PSS with two 2560x1440 bars: {pss} kB ({what} {limit} kB)");
    warn_over_target("M0 bar", pss);
    assert!(
        pss <= limit,
        "PSS {pss} kB over the {limit} kB {what}\n{}",
        memory_report(pid)
    );

    // Idle: no thread of the process wakes. Skip a window that would
    // contain a minute boundary (the clock tick is the one wakeup).
    if seconds_into_minute() >= 56 {
        std::thread::sleep(Duration::from_secs(6));
    }
    let frames = damage_lines(&log).len();
    let before = switches(pid);
    let threads = per_thread(pid);
    std::thread::sleep(Duration::from_secs(2));
    let after = switches(pid);
    let woke: Vec<String> = per_thread(pid)
        .into_iter()
        .filter(|(t, n)| threads.get(t) != Some(n))
        .map(|(t, n)| format!("{t}: {} -> {n}", threads.get(&t).copied().unwrap_or(0)))
        .collect();
    assert_eq!(after - before, 0, "woke while idle: {woke:?}");
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

/// The M0 budget on design.md's own bar (theme.strand and bar.strand,
/// unchanged, on the mock desktop with the real clock), not the M0 demo:
/// two 2560x1440 outputs at 1.0 and 1.25 within the PSS budget (34 MB
/// target warned, 38 MB ceiling fails; in a release run; the debug ceiling otherwise); once boot work is done, no
/// thread waking from then (by :45) to :57 of the minute (no idle-cache
/// or other timer of its own: the next wake is the minute tick); and the
/// tick itself repainting at most 2,000 px² over both outputs. Several
/// whole minutes are measured by `scripts/m2-exit.sh`.
#[test]
fn the_design_bar_keeps_the_m0_budget() {
    let Some(sway) = Sway::start_as("budget") else {
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
    // The config outside /tmp: the watcher's light watches on its
    // ancestors would wake for other tests' directories coming and going
    // there (a desktop's ~/.config has no such neighbours).
    let home =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    // The font and icon directories the cache sources name, made, so the
    // watcher watches each itself and not, while one is missing, its
    // nearest existing ancestor for creations (HOME would be one).
    for d in [
        ".fonts",
        ".icons",
        ".config/fontconfig",
        ".config/gtk-3.0",
        ".config/gtk-4.0",
        ".local/share/fonts",
        ".local/share/icons",
        ".local/share/applications",
    ] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
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
        .env("STRAND_LOG", "damage")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut strand = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    while damage_lines(&log).len() < 2 {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(Instant::now() < deadline, "bars did not paint");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The tick measured below is a steady one: a surface's buffer of
    // age 2 repaints its previous frame's damage too, so the first tick
    // after boot also repaints whatever the boot's last frame drew (icons
    // decoded late on a loaded runner: CI run 37764492027, 2,196 px²).
    // The first tick is let by, as budgets.rs does.
    let minute = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0)
    };
    let booted = minute();
    while minute() == booted {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    // Boot work done (late icons and glyphs, a loaded machine): a whole
    // second with no wakeup and no frame, early enough in the minute
    // that the window below ends before the next tick.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (f, w) = (damage_lines(&log).len(), switches(pid));
        std::thread::sleep(Duration::from_secs(1));
        if damage_lines(&log).len() == f && switches(pid) == w {
            if seconds_into_minute() <= 45 {
                break;
            }
            // Past the tick, then settle again.
            std::thread::sleep(Duration::from_secs(63 - seconds_into_minute()));
        }
        assert!(Instant::now() < deadline, "boot never settled");
    }
    // Measured once boot work is done and the bar is idle (the steady
    // state the M0 gate is about): a loaded runner can still be decoding
    // icons or shaping late text 1.5 s in.
    assert_thp_off(pid);
    let pss = pss_kb(pid);
    // A debug build of the whole language path carries about 25 MB more
    // than release (49 against 14–27 MB here): its own ceiling.
    let (limit, what) = if cfg!(debug_assertions) {
        (64 * 1024, "debug ceiling")
    } else {
        pss_limit()
    };
    eprintln!("design bar PSS on two 2560x1440 outputs: {pss} kB ({what} {limit} kB)");
    warn_over_target("design bar", pss);
    assert!(
        pss <= limit,
        "PSS {pss} kB over the {limit} kB {what}\n{}",
        memory_report(pid)
    );
    // Then nothing until :57 (at least 12 s).
    let frames = damage_lines(&log).len();
    let before = switches(pid);
    let threads = per_thread(pid);
    std::thread::sleep(Duration::from_secs(57 - seconds_into_minute()));
    let after = switches(pid);
    let woke: Vec<String> = per_thread(pid)
        .into_iter()
        .filter(|(t, n)| threads.get(t) != Some(n))
        .map(|(t, n)| format!("{t}: {} -> {n}", threads.get(&t).copied().unwrap_or(0)))
        .collect();
    assert_eq!(after - before, 0, "woke while idle: {woke:?}");
    assert_eq!(damage_lines(&log).len(), frames, "painted while idle");
    // The minute tick: both clocks repaint, at most 2,000 px² in all.
    let deadline = Instant::now() + Duration::from_secs(15);
    while damage_lines(&log).len() < frames + 2 {
        assert!(Instant::now() < deadline, "the clocks did not tick");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    let tick = &damage_lines(&log)[frames..];
    let area: u64 = tick
        .iter()
        .filter_map(|l| {
            l.split(" area=")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .sum();
    eprintln!(
        "the minute tick: {} frames, {area} px²: {tick:?}",
        tick.len()
    );
    // The day change (local 00:00, and 00:01 whose age-2 buffer still
    // holds the old day) moves the centred clock: it repaints whole, the
    // documented midnight exception (decisions.md, wave3-pixels exit fixer
    // r3; `damage.rs::the_midnight_tick_damages_only_the_centred_clock`).
    let now = std::process::Command::new("date")
        .arg("+%H%M")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let gate = if now == "0000" || now == "0001" {
        4000
    } else {
        2000
    };
    assert!(
        area <= gate,
        "a tick at {now} repainted {area} px²: {tick:?}"
    );
    drop(strand);
    let _ = std::fs::remove_dir_all(&home);
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
    // tray icon (the mock's `network-wireless`, 16 px) ends at margin and
    // pad: 2560 - 8 - 12.
    let first = cols[0];
    let last = *cols.last().unwrap();
    assert!((18..40).contains(&first), "start ink at {first}");
    assert!(
        (shot.w - 26..=shot.w - 18).contains(&last),
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
        // (On failure, the whole log: which frame painted early, and
        // what arrived after it.)
        assert!(
            sizes.iter().all(|b| *b == sizes[0]),
            "{s} changed size: {sizes:?}\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
    let bright_in =
        |s: &Shot, x: usize, y: usize| s.px(x, y).iter().map(|c| *c as u32).sum::<u32>() > 450;
    // Its `enter { opacity: 0; scale: 0.96 }` settled: two shots alike
    // (its box's rows and its colour at their middle) a moment apart.
    let settled = |x: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = None;
        loop {
            let s = Shot::take(&sway, "HEADLESS-1");
            let rows: Vec<usize> = (60..s.h).filter(|&y| bright_in(&s, x, y)).collect();
            let key = rows
                .first()
                .zip(rows.last())
                .map(|(&t, &b)| (t, b, s.px(x, (t + b) / 2)));
            if key.is_some() && key == last {
                return s;
            }
            assert!(Instant::now() < deadline, "the launcher never settled");
            last = key;
            std::thread::sleep(Duration::from_millis(150));
        }
    };
    let shot = settled(Shot::take(&sway, "HEADLESS-1").w / 2 + 250);
    let bright = |x: usize, y: usize| bright_in(&shot, x, y);
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
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(&[], &PathBuf::from(&dir).join("design_launcher.png"));
    }
    // The box's left edge, and the rows of ink in its `image h.app.icon
    // { size: 32 }` column (pad 12 + row pad 8, 32 wide), below the
    // input: the mock apps' three icons (Adwaita's `-symbolic` ones).
    let mid = (top + bottom) / 2;
    let left = (0..shot.w).find(|&x| bright(x, mid)).unwrap();
    let dark =
        |s: &Shot, x: usize, y: usize| s.px(x, y).iter().map(|c| *c as u32).sum::<u32>() < 200;
    let icon_bands = |s: &Shot, top: usize, bottom: usize| {
        let inked: Vec<usize> = (top + 48..bottom)
            .filter(|&y| (left + 20..left + 52).any(|x| dark(s, x, y)))
            .collect();
        inked.windows(2).filter(|p| p[1] > p[0] + 1).count() + usize::from(!inked.is_empty())
    };
    assert_eq!(icon_bands(&shot, top, bottom), 3, "three app icons");

    let mut pointer = pointer::Pointer::new(&sway.dir.join(&sway.display));
    let (w, h) = (shot.w as u32, shot.h as u32);
    // A new virtual pointer's first buttons reach no surface: its first
    // click goes to the launcher's padding (clear of its rows, so it
    // would not launch anything if it did arrive).
    let pad = (left as u32 + 5, mid as u32);
    pointer.click(pad.0, pad.1, w, h);
    std::thread::sleep(Duration::from_millis(200));
    // Hovering the first row (`when hover { bg: $surface.hi }`) repaints
    // it: events reach the launcher now.
    let first_icon = (top + 48..bottom)
        .find(|&y| (left + 20..left + 52).any(|xx| dark(&shot, xx, y)))
        .unwrap();
    let row = (x, first_icon + 12);
    let calm = shot.px(row.0, row.1);
    // (Not exact: the bg is dithered.)
    let near = |p: [u8; 3], q: [u8; 3]| (0..3).all(|i| p[i].abs_diff(q[i]) <= 3);
    pointer.motion(row.0 as u32, row.1 as u32, w, h);
    let deadline = Instant::now() + Duration::from_secs(10);
    while near(Shot::take(&sway, "HEADLESS-1").px(row.0, row.1), calm) {
        assert!(Instant::now() < deadline, "hovering a row changed nothing");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A click inside (its padding) keeps it open: the pointer leaves the
    // row on the way (its hover goes), and the launcher stays.
    pointer.click(pad.0, pad.1, w, h);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(Shot::take(&sway, "HEADLESS-1").px(row.0, row.1), calm) {
        assert!(
            Instant::now() < deadline,
            "the click never reached the launcher"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        bright_in(&Shot::take(&sway, "HEADLESS-1"), x, mid),
        "a click inside closed it"
    );

    // Typing: a keyboard appears, the `focus: true` input takes it and
    // draws its caret (`$accent`, a saturated blue on the light theme);
    // typed text filters the list to Foot and the caret follows it.
    let blue = |p: [u8; 3]| p[2] as i32 - p[0] as i32 > 60;
    let input_band = |s: &Shot, top: usize| -> (Vec<usize>, Vec<usize>) {
        let (mut carets, mut ink) = (Vec::new(), Vec::new());
        // (Clear of the rounded corners.)
        for xx in left + 4..left + 576 {
            if (top + 8..top + 38).any(|y| blue(s.px(xx, y))) {
                carets.push(xx);
            }
            if (top + 8..top + 38).any(|y| dark(s, xx, y)) {
                ink.push(xx);
            }
        }
        (carets, ink)
    };
    let mut keyboard = keyboard::Keyboard::new(&sway.dir.join(&sway.display), &sway.dir);
    let deadline = Instant::now() + Duration::from_secs(10);
    while input_band(&Shot::take(&sway, "HEADLESS-1"), top)
        .0
        .is_empty()
    {
        assert!(Instant::now() < deadline, "no caret once a keyboard exists");
        std::thread::sleep(Duration::from_millis(50));
    }
    keyboard.type_text("foo");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = Shot::take(&sway, "HEADLESS-1");
        let rows: Vec<usize> = (60..s.h).filter(|&y| bright_in(&s, x, y)).collect();
        if let (Some(&t), Some(&b)) = (rows.first(), rows.last())
            && icon_bands(&s, t, b) == 1
        {
            break;
        }
        assert!(Instant::now() < deadline, "typing did not filter the list");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Once its height spring has settled.
    let shot = settled(x);
    let rows: Vec<usize> = (60..shot.h).filter(|&y| bright_in(&shot, x, y)).collect();
    let (top, bottom) = (rows[0], *rows.last().unwrap());
    assert_eq!(icon_bands(&shot, top, bottom), 1, "one hit for `foo`");
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(&[], &PathBuf::from(&dir).join("design_launcher_typed.png"));
    }
    let (carets, ink) = input_band(&shot, top);
    assert!(
        !ink.is_empty() && !carets.is_empty(),
        "no text or caret: {ink:?} {carets:?}"
    );
    let text_end = *ink.last().unwrap();
    assert!(
        carets
            .iter()
            .all(|&c| c + 3 >= text_end && c <= text_end + 8),
        "the caret ({carets:?}) is not after the typed text (ink to {text_end})"
    );
    // The one hit is selected (a focused input's list selects its first
    // row): `when selected { bg: $accent.container }`, the accent at
    // 0.22 over the launcher's surface, so bluer than the padding above
    // the input.
    let hit_icon = (top + 48..bottom)
        .find(|&y| (left + 20..left + 52).any(|xx| dark(&shot, xx, y)))
        .unwrap();
    let tint = |p: [u8; 3]| p[2] as i32 - p[0] as i32;
    let (sel, plain) = (shot.px(x, hit_icon + 16), shot.px(x, top + 4));
    assert!(
        tint(sel) >= tint(plain) + 10,
        "the hit is not drawn selected: {sel:?} vs the panel's {plain:?}"
    );
    // The hit's matched letters (`marks: h.ranges; mark_color:
    // $accent`) in blue in its name.
    let marked = (top + 44..bottom)
        .flat_map(|y| (left + 56..left + 200).map(move |x| (x, y)))
        .filter(|&(x, y)| blue(shot.px(x, y)))
        .count();
    assert!(marked > 20, "no marked letters in the hit: {marked} px");
    drop(keyboard);

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

/// The four design shells with their widgets (M2): the bar's icons, the
/// OSD's icon and meter (shown at boot here, as a volume change would),
/// the toasts' close icons, and the bar's `Clock` calendar `popup`: a
/// click on the clock opens it as an xdg_popup under the bar, centred on
/// the clock, with the month's grid and today in `$accent`; a click away
/// closes it (the grab ends, `open` is written false). Set
/// `STRAND_SHOTS` to a directory to keep the screenshots.
#[test]
fn the_design_shells_draw_their_widgets_and_the_calendar_popup() {
    let Some(sway) = Sway::start_as("widgets") else {
        return;
    };
    let home = sway.dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    let osd = include_str!("../../strand-compiler/tests/fixtures/osd.strand")
        .replace("state shown = false", "state shown = true");
    for (name, text) in [
        (
            "theme.strand",
            include_str!("../../strand-compiler/tests/fixtures/theme.strand").to_string(),
        ),
        (
            "bar.strand",
            include_str!("../../strand-compiler/tests/fixtures/bar.strand").to_string(),
        ),
        (
            "launcher.strand",
            include_str!("../../strand-compiler/tests/fixtures/launcher.strand").to_string(),
        ),
        (
            "toasts.strand",
            include_str!("../../strand-compiler/tests/fixtures/toasts.strand").to_string(),
        ),
        ("osd.strand", osd),
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
    let surfaces = |log: &Path| -> std::collections::BTreeSet<String> {
        damage_lines(log)
            .iter()
            .filter_map(|l| {
                l.split_whitespace()
                    .find(|w| w.starts_with("surface="))
                    .map(String::from)
            })
            .collect()
    };
    let wait_for = |what: &str, strand: &mut Proc, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(
                strand.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "{what}: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The bar, the toasts and the OSD.
    wait_for("three surfaces", &mut strand, &|| surfaces(&log).len() >= 3);
    std::thread::sleep(Duration::from_millis(700));
    let shot = Shot::take(&sway, "HEADLESS-1");
    let sum = |p: [u8; 3]| p.iter().map(|c| *c as u32).sum::<u32>();
    let (w, h) = (shot.w, shot.h);
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(&[], &PathBuf::from(&dir).join("design_shells.png"));
    }
    // The OSD's meter: a run of `$accent` (a saturated blue on the light
    // theme) along the middle of its pill, 96 px above the bottom.
    let blue = |p: [u8; 3]| p[2] as i32 - p[0] as i32 > 60;
    let osd_row = (h - 140..h - 96).find(|&y| {
        (w / 2 - 80..w / 2 + 80)
            .filter(|&x| blue(shot.px(x, y)))
            .count()
            > 40
    });
    assert!(osd_row.is_some(), "no meter fill in the OSD");
    // The OSD's volume icon: dark ink left of the meter, in its pill.
    let y = osd_row.unwrap();
    assert!(
        (w / 2 - 125..w / 2 - 95).any(|x| (y - 10..y + 10).any(|yy| sum(shot.px(x, yy)) < 200)),
        "no icon in the OSD"
    );
    // The toasts' close icons: dark ink near the right edge of the first.
    let toast_x = w - 12 - 30;
    assert!(
        (60..110).any(|yy| (toast_x - 10..toast_x + 10).any(|x| sum(shot.px(x, yy)) < 200)),
        "no close icon on the first toast"
    );
    // The toasts' `image n.image ?? n.app.icon { size: 36; radius:
    // $radius.sm }` (the mock apps' `mail-unread` and `battery-caution`,
    // found as Adwaita's `-symbolic` ones): ink in the 36 px slot at
    // the start of each toast (margin 12, width 380, pad 12).
    let slot = w - 12 - 380 + 12;
    for (which, ys) in [("first", 60..118), ("second", 122..185)] {
        let ink = ys
            .flat_map(|y| (slot..slot + 36).map(move |x| (x, y)))
            .filter(|&(x, y)| sum(shot.px(x, y)) < 200)
            .count();
        assert!(ink > 40, "no image on the {which} toast: {ink} px");
    }
    // The tray's `image item.icon { size: 16 }` (the mock's
    // `network-wireless`, found as `network-wireless-symbolic`): ink at
    // the end of the bar, after the battery (margin 8, pad 12).
    assert!(
        (w - 40..w - 16).any(|x| (16..36).any(|y| sum(shot.px(x, y)) < 200)),
        "no tray icon at the bar's end"
    );
    // No calendar yet below the clock.
    let below = (w / 2, 150);
    assert!(
        sum(shot.px(below.0, below.1)) < 450,
        "calendar open at boot"
    );

    // A click on the clock opens the calendar popup below the bar.
    let mut pointer = pointer::Pointer::new(&sway.dir.join(&sway.display));
    let before = surfaces(&log).len();
    // (A new virtual pointer's first buttons do not reach a surface:
    // the first click goes to the empty desktop.)
    pointer.click(600, 700, w as u32, h as u32);
    std::thread::sleep(Duration::from_millis(200));
    pointer.click((w / 2) as u32, 26, w as u32, h as u32);
    wait_for("the calendar's surface", &mut strand, &|| {
        surfaces(&log).len() > before
    });
    std::thread::sleep(Duration::from_millis(700));
    let open = Shot::take(&sway, "HEADLESS-1");
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(&[], &PathBuf::from(&dir).join("design_calendar.png"));
    }
    // Its light card spans the clock's centre below the bar.
    let rows: Vec<usize> = (44..400)
        .filter(|&y| sum(open.px(below.0, y)) > 600)
        .collect();
    assert!(
        rows.len() > 150,
        "no calendar below the clock: {} light rows",
        rows.len()
    );
    let top = rows[0];
    assert!((50..70).contains(&top), "calendar top at {top}");
    let cols: Vec<usize> = (w / 2 - 200..w / 2 + 200)
        .filter(|&x| sum(open.px(x, top + 20)) > 600)
        .collect();
    let centre = (cols[0] + cols.last().unwrap()) as f64 / 2.0;
    assert!(
        (centre - (w / 2) as f64).abs() <= 3.0,
        "calendar centred at {centre}"
    );
    // Today, in `$accent`, somewhere in the grid.
    let accent = (top..top + 250)
        .flat_map(|y| (cols[0]..*cols.last().unwrap()).map(move |x| (x, y)))
        .filter(|&(x, y)| blue(open.px(x, y)))
        .count();
    assert!(accent > 100, "no accent day in the calendar: {accent} px");
    let accent_px = |shot: &Shot| {
        (top..top + 250)
            .flat_map(|y| (cols[0]..*cols.last().unwrap()).map(move |x| (x, y)))
            .filter(|&(x, y)| blue(shot.px(x, y)))
            .count()
    };
    // `›` (the top row's right end) moves `state month` on; twice, as
    // the grid two months on never shows today (next month's may, among
    // its leading days).
    for _ in 0..2 {
        pointer.click(
            (*cols.last().unwrap() - 25) as u32,
            (top + 23) as u32,
            w as u32,
            h as u32,
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while accent_px(&Shot::take(&sway, "HEADLESS-1")) > accent / 4 {
        assert!(Instant::now() < deadline, "`›` did not turn the month");
        std::thread::sleep(Duration::from_millis(100));
    }

    // A click away closes it.
    pointer.click(600, 700, w as u32, h as u32);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let after = Shot::take(&sway, "HEADLESS-1");
        if sum(after.px(below.0, top + 40)) < 450 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a click away did not close the calendar"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Closed, its content was unmounted (decisions.md, carried r2): a
    // second click on the clock mounts it again, drawn whole, on the
    // month it was left on (the `Calendar`'s state was kept).
    std::thread::sleep(Duration::from_millis(300));
    pointer.click((w / 2) as u32, 26, w as u32, h as u32);
    let deadline = Instant::now() + Duration::from_secs(10);
    while sum(Shot::take(&sway, "HEADLESS-1").px(below.0, top + 40)) <= 600 {
        assert!(
            Instant::now() < deadline,
            "a second click did not open the calendar again"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(700));
    let again = Shot::take(&sway, "HEADLESS-1");
    if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
        sway.grim(&[], &PathBuf::from(&dir).join("design_calendar_again.png"));
    }
    let top_again = (44..400).find(|&y| sum(again.px(below.0, y)) > 600);
    assert_eq!(top_again, Some(top), "the reopened calendar's card moved");
    let accent_again = accent_px(&again);
    assert!(
        accent_again <= accent / 4,
        "the reopened calendar is back on this month: {accent_again} accent px"
    );
    pointer.click(600, 700, w as u32, h as u32);
    let deadline = Instant::now() + Duration::from_secs(10);
    while sum(Shot::take(&sway, "HEADLESS-1").px(below.0, top + 40)) >= 450 {
        assert!(
            Instant::now() < deadline,
            "a click away did not close the calendar again"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The pointer on the volume icon (a click: it mutes) hovers its row:
    // `if hover { slider … }` slides a slider in, drawn in `$accent`
    // in the row (it grows leftwards, the end packs to the end).
    let speaker = (w * 3 / 4..w - 20)
        .find(|&x| (14..38).any(|y| sum(shot.px(x, y)) < 200))
        .expect("ink at the bar's end");
    pointer.click(speaker as u32 + 6, 26, w as u32, h as u32);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let after = Shot::take(&sway, "HEADLESS-1");
        // (The row grows to the left as the slider slides in.)
        let fill = (w * 3 / 4..w - 20)
            .filter(|&x| (14..38).any(|y| blue(after.px(x, y))))
            .count();
        if fill > 20 {
            if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
                sway.grim(&[], &PathBuf::from(&dir).join("design_volume.png"));
            }
            break;
        }
        assert!(Instant::now() < deadline, "no slider on hover: {fill}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let errors: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("ERROR"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    drop(strand);
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

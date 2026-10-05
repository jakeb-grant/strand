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

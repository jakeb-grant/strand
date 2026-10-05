//! `strand run --demo` end to end on a headless sway: two 2560x1440 outputs
//! (the second at scale 1.25) within the 34 MB PSS budget, a bar on each
//! with its clock centred, no wakeups while idle, then a third output of
//! another width at scale 1 (hotplugged) whose bar is aligned to its own
//! width. Skipped, loudly, when sway or grim is not installed.
//! `scripts/m0-exit.sh` measures the full M0 gates on a release build (a
//! whole minute, the tick's damage).

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
        for tool in ["sway", "swaymsg", "grim"] {
            if Command::new(tool).arg("--version").output().is_err() {
                eprintln!(
                    "\n*** SKIPPED: {tool} is not installed; the M0 PSS, idle and alignment checks did not run ***\n"
                );
                return None;
            }
        }
        // Short: the IPC socket path must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-demo-{}", std::process::id()));
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
/// the two-monitor bar).
const PSS_GATE_KB: u64 = 34 * 1024;

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
    eprintln!("strand PSS with two 2560x1440 bars: {pss} kB");
    assert!(
        pss <= PSS_GATE_KB,
        "PSS {pss} kB over the {PSS_GATE_KB} kB gate"
    );

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
    painted(&mut strand, frames + 1);
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        damage_lines(&log)
            .iter()
            .any(|l| l.contains("buffer=1920x32 ")),
        "{:?}",
        damage_lines(&log)
    );
    Shot::take(&sway, "HEADLESS-3").assert_aligned("HEADLESS-3", 1.0);
    Shot::take(&sway, "HEADLESS-1").assert_aligned("HEADLESS-1", 1.0);
    Shot::take(&sway, "HEADLESS-2").assert_aligned("HEADLESS-2", 1.25);
    drop(strand);
}

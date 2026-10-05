//! `strand run --demo` end to end on a headless sway with two outputs (the
//! second at scale 1.25): a bar on each, the clock drawn, and no wakeups
//! while idle. Skipped when sway is not installed. `scripts/m0-exit.sh`
//! measures the full M0 gates (PSS, a whole minute, the tick's damage).

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
        if Command::new("sway").arg("--version").output().is_err() {
            eprintln!("skipping: sway is not installed");
            return None;
        }
        // Short: the IPC socket path must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-demo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x720 scale 1\n",
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
        "1280x720",
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

    // One full frame per output: 1280×32 at 1.0, 1280×40 at 1.25.
    let deadline = Instant::now() + Duration::from_secs(20);
    while damage_lines(&log).len() < 2 {
        assert!(
            strand.0.try_wait().unwrap().is_none(),
            "strand exited: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(Instant::now() < deadline, "bars did not paint");
        std::thread::sleep(Duration::from_millis(50));
    }
    let boot = damage_lines(&log);
    assert!(
        boot.iter().any(|l| l.contains("buffer=1280x32 ")),
        "{boot:?}"
    );
    assert!(
        boot.iter().any(|l| l.contains("buffer=1280x40 ")),
        "{boot:?}"
    );

    // The clock is drawn in the middle of the first bar: some pixel there
    // is light text on the dark background.
    let shot = sway.dir.join("bar.ppm");
    assert!(sway.grim(&["-t", "ppm", "-g", "0,0 1280x32"], &shot));
    let ppm = std::fs::read(&shot).unwrap();
    // Binary PPM: "P6\n1280 32\n255\n" then RGB.
    let header_end = ppm
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(2)
        .unwrap()
        .0
        + 1;
    let px = |x: usize, y: usize| {
        let i = header_end + (y * 1280 + x) * 3;
        [ppm[i], ppm[i + 1], ppm[i + 2]]
    };
    assert_eq!(px(400, 2), [0x1e, 0x1e, 0x2e], "bar background");
    let lit = (600..680).any(|x| (6..26).any(|y| px(x, y)[0] > 0x90));
    assert!(lit, "no clock text in the centre");

    // Idle: no thread of the process wakes. Skip a window that would
    // contain a minute boundary (the clock tick is the one wakeup).
    std::thread::sleep(Duration::from_millis(300));
    if seconds_into_minute() >= 56 {
        std::thread::sleep(Duration::from_secs(6));
    }
    let frames = damage_lines(&log).len();
    let before = switches(pid);
    std::thread::sleep(Duration::from_secs(2));
    let after = switches(pid);
    assert_eq!(after - before, 0, "woke while idle");
    assert_eq!(damage_lines(&log).len(), frames, "painted while idle");
    drop(strand);
}

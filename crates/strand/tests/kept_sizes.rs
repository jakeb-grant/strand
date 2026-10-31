//! (kept-size) A panel whose size springs maps at the size it will need,
//! end to end: `strand run` on a headless sway, the file between two runs,
//! and hot reloads between them (decisions.md kept-size). What strand asks
//! the compositor for is read from its own Wayland log (`WAYLAND_DEBUG`):
//! the layer surface's `set_size`s in order, and the first `wl_shm` buffer.
//!
//! Skipped, loudly, without sway (CI sets `STRAND_REQUIRE_SWAY`).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(20);

/// One panel a box tall, its height a state a test can raise.
fn shell(tall: u32) -> String {
    format!(
        "export state tall = {tall}\n\
         panel Card {{ anchor: top; size: auto ~ 100ms\n\
         \x20 box {{ width: 200; height: tall; bg: #222222 }}\n\
         }}\n"
    )
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Desk {
    dir: PathBuf,
    display: String,
    runs: u32,
    strand: Option<Proc>,
    _sway: Proc,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        "xwayland disable\noutput HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n",
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
        let display = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
        if let Some(d) = display {
            return (sway, d);
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `set_size(w, h)` and `create_buffer(new id, offset, w, h, stride, format)`
/// as a Wayland log prints them.
fn numbers(line: &str, call: &str) -> Option<Vec<u32>> {
    let (_, args) = line.split_once(&format!(".{call}("))?;
    let args = args.split_once(')')?.0;
    Some(
        args.split(',')
            .filter_map(|a| a.trim().parse().ok())
            .collect(),
    )
}

impl Desk {
    fn new(name: &str, config: &str) -> Option<Self> {
        if Command::new("sway").arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "sway is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: sway is not installed; kept_sizes did not run ***\n");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("strand-ks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (sway, display) = sway(&dir);
        let mut desk = Self {
            dir,
            display,
            runs: 0,
            strand: None,
            _sway: sway,
        };
        std::fs::create_dir_all(desk.config()).unwrap();
        desk.save(config);
        desk.run();
        Some(desk)
    }

    fn config(&self) -> PathBuf {
        self.dir.join("home/.config/strand")
    }

    fn save(&self, config: &str) {
        std::fs::write(self.config().join("shell.strand"), config).unwrap();
    }

    fn sizes_file(&self) -> PathBuf {
        self.dir.join("state/strand/panel-sizes.json")
    }

    fn log(&self) -> String {
        let path = self.dir.join(format!("strand-{}.log", self.runs));
        String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
    }

    /// `strand run`, ending the one before: a new session with a new log.
    fn run(&mut self) {
        self.strand.take();
        self.runs += 1;
        let log = self.dir.join(format!("strand-{}.log", self.runs));
        let child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(self.config())
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("WAYLAND_DEBUG", "client")
            .env("STRAND_SOCKET", self.dir.join("strand.sock"))
            .env("HOME", self.dir.join("home"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env_remove("STRAND_STATE_DIR")
            .env_remove("STRAND_MOCK")
            .env_remove("SWAYSOCK")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("NIRI_SOCKET")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap();
        self.strand = Some(Proc(child));
    }

    fn strand(&self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("STRAND_SOCKET", self.dir.join("strand.sock"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "strand {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The heights this run asked of the compositor, in order.
    fn heights(&self) -> Vec<u32> {
        self.log()
            .lines()
            .filter(|l| l.contains("zwlr_layer_surface_v1"))
            .filter_map(|l| numbers(l, "set_size"))
            .filter_map(|n| n.get(1).copied())
            .collect()
    }

    /// The height of the first buffer this run made.
    fn first_buffer(&self) -> Option<u32> {
        let log = self.log();
        let n = log.lines().find_map(|l| numbers(l, "create_buffer"))?;
        // Offset, width, height, stride, format (the new id is no number).
        n.get(2).copied()
    }

    fn until(&self, what: &str, ok: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !ok(self) {
            assert!(
                Instant::now() < deadline,
                "never: {what}; heights {:?}; {}; log tail: {}",
                self.heights(),
                self.saved(),
                self.log()
                    .lines()
                    .filter(|l| !l.contains('@'))
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The height asked for last is `h`.
    fn asks_for(&self, h: u32) {
        self.until(&format!("the panel asks for {h}"), |d| {
            d.heights().last() == Some(&h)
        });
    }

    fn saved(&self) -> String {
        std::fs::read_to_string(self.sizes_file()).unwrap_or_default()
    }

    /// The file's height for the panel, and the config it is under.
    fn kept(&self) -> Option<(f64, String)> {
        let v: serde_json::Value = serde_json::from_str(&self.saved()).ok()?;
        Some((
            v["sizes"]["strand-Card"][1].as_f64()?,
            v["config"].as_str()?.to_string(),
        ))
    }

    fn keeps(&self, h: u32) -> String {
        self.until(&format!("{h} kept in the file"), |d| {
            d.kept().is_some_and(|(k, _)| k == f64::from(h))
        });
        self.kept().map(|(_, c)| c).unwrap()
    }

    /// The first size and the first buffer of a new run.
    fn maps_at(&self, h: u32, why: &str) {
        self.until("the first buffer", |d| d.first_buffer().is_some());
        assert_eq!(self.heights().first(), Some(&h), "first set_size: {why}");
        assert_eq!(self.first_buffer(), Some(h), "first buffer: {why}");
    }
}

/// A reload that changes the config starts the kept sizes again at once,
/// and what the panel measures from then on is that config's: the file
/// never holds a size under a config that did not measure it, here the
/// original's 200 against an edit's 400. A reload that changes nothing
/// keeps them.
#[test]
fn a_reload_that_changes_the_config_starts_the_kept_sizes_again() {
    let Some(mut desk) = Desk::new("reload", &shell(200)) else {
        return;
    };
    desk.maps_at(200, "nothing is kept yet");
    let first = desk.keeps(200);

    // Reloaded with nothing changed: still that config, its size kept.
    desk.strand(&["set", "shell.tall", "300"]);
    desk.asks_for(300);
    desk.keeps(300);
    desk.strand(&["set", "shell.tall", "200"]);
    desk.strand(&["reload"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(desk.heights().last(), Some(&300), "the same config keeps");
    assert_eq!(desk.kept(), Some((300.0, first.clone())));

    // Made smaller: the buffer follows, with no restart.
    desk.save(&shell(100));
    desk.asks_for(100);
    let smaller = desk.keeps(100);
    assert_ne!(smaller, first, "saved under the config that measured it");

    // Larger, then back to the text it started with.
    desk.save(&shell(400));
    desk.asks_for(400);
    let larger = desk.keeps(400);
    assert_ne!(larger, smaller);
    desk.save(&shell(200));
    desk.asks_for(200);
    assert_eq!(desk.keeps(200), first, "the first config's hash again");

    desk.run();
    desk.maps_at(200, "not the 400 an edit measured");
}

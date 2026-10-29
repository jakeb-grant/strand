//! (screens-focus) `screens.focused` and each `Screen.focused` follow the
//! compositor's focused monitor, end to end: `strand run` on a headless
//! sway with HEADLESS-1 (1280×720, at 0,0) and HEADLESS-2 (1600×900,
//! right of it), the focus moved between them with `swaymsg focus
//! output`. The config exports what it reads of the focus (read back
//! with `strand get`) and shows a panel whose `screens:` evaluates to
//! `focused`, half its `screen`'s width wide: it must show on the
//! focused monitor, sized from that monitor, and move with the focus.
//! Without sway's IPC (no compositor adapter) the first monitor is the
//! focused one, wherever sway's focus is.
//!
//! Skipped, loudly, without sway, swaymsg or grim (CI sets
//! `STRAND_REQUIRE_SWAY`).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SHELL: &str = r#"
state wide = false
export let named = screens.focused?.name ?? "none"
export let marked = screens.all.filter(s => s.focused).first?.name ?? "none"
export let marks = screens.all.filter(s => s.focused).len

panel Here { screens: wide ? Screens.all : Screens.focused; anchor: top_left
  width: screen.width / 2; height: 40; open: true
  box { width: screen.width / 2; height: 40; bg: #ff0000 }
}
"#;

/// A shell that reads nothing of the focus.
const PLAIN: &str = "export let n = 1\nbar Top { height: 30\n  text screen.name\n}\n";

/// A shell whose only reader of the focus is a binding of
/// `screens.focused`.
const READER: &str = "export let n = 2\nexport let named = screens.focused?.name ?? \"none\"\n\
                      bar Top { height: 30\n  text screen.name\n}\n";

/// The panel's row of pixels looked at, and its colour.
const ROW: usize = 20;
const RED: [u8; 3] = [0xff, 0x00, 0x00];

/// How long a state may take to show.
const PATIENCE: Duration = Duration::from_secs(20);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Desk {
    dir: PathBuf,
    config: PathBuf,
    display: String,
    sock: PathBuf,
    log: PathBuf,
    strand: Option<Proc>,
    _sway: Proc,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; screens_focus did not run ***\n");
            return false;
        }
    }
    true
}

fn swaymsg(sock: &Path, args: &[&str]) -> String {
    let out = Command::new("swaymsg")
        .args(args)
        .env("SWAYSOCK", sock)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "swaymsg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A headless sway in `dir` with both outputs, its display and IPC
/// socket.
fn sway(dir: &Path) -> (Proc, String, PathBuf) {
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
            let up = Command::new("swaymsg")
                .args(["-t", "get_version"])
                .env("SWAYSOCK", &sock)
                .output()
                .is_ok_and(|o| o.status.success());
            if up {
                swaymsg(&sock, &["create_output"]);
                swaymsg(
                    &sock,
                    &[
                        "output",
                        "HEADLESS-2",
                        "resolution",
                        "1600x900",
                        "position",
                        "1280",
                        "0",
                        "scale",
                        "1",
                    ],
                );
                return (sway, d.clone(), sock);
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// sway with the focus on `focus`, then `strand run` on `shell`, with
/// sway's IPC (`adapter`) or without it.
fn start(name: &str, shell: &str, focus: &str, adapter: bool) -> Option<Desk> {
    if !tools() {
        return None;
    }
    // Short: the IPC socket path must fit in sun_path.
    let dir = std::env::temp_dir().join(format!("strand-sf-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, sock) = sway(&dir);
    swaymsg(&sock, &["focus", "output", focus]);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("shell.strand"), shell).unwrap();
    let log = dir.join("strand.log");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
    cmd.arg("run")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("WAYLAND_DISPLAY", &display)
        .env("STRAND_SOCKET", dir.join("strand.sock"))
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env(
            "STRAND_LOG",
            std::env::var("STRAND_TEST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .env_remove("STRAND_MOCK")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .env_remove("NIRI_SOCKET")
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    if adapter {
        cmd.env("SWAYSOCK", &sock);
    } else {
        cmd.env_remove("SWAYSOCK");
    }
    let strand = Proc(cmd.spawn().unwrap());
    Some(Desk {
        dir,
        config,
        display,
        sock,
        log,
        strand: Some(strand),
        _sway: sway,
    })
}

impl Desk {
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn alive(&mut self) {
        if let Some(p) = &mut self.strand {
            assert!(
                p.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                self.log_text()
            );
        }
    }

    /// The compositor service runs: its `strand-toplevel` thread is up.
    fn hub_runs(&self) -> bool {
        let Some(pid) = self.strand.as_ref().map(|p| p.0.id()) else {
            return false;
        };
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
            .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
            .any(|comm| comm.trim_end() == "strand-toplevel")
    }

    /// `shell` saved over the config (a reload), and running: its `n` is
    /// `n`.
    fn reload(&mut self, shell: &str, n: &str) {
        std::fs::write(self.config.join("shell.strand"), shell).unwrap();
        self.until("the reload", |d| match d.get("n") {
            Some(got) if got == n => Ok(()),
            got => Err(format!("n = {got:?}")),
        });
    }

    /// `strand get shell.<name>`, once strand answers.
    fn get(&self, name: &str) -> Option<String> {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["get", &format!("shell.{name}")])
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("STRAND_SOCKET", self.dir.join("strand.sock"))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The exports: `screens.focused`'s name, the one `Screen` flagged
    /// focused and how many are.
    fn focus(&self) -> Option<(String, String, String)> {
        Some((self.get("named")?, self.get("marked")?, self.get("marks")?))
    }

    /// How many pixels of [`ROW`] on `output` are the panel's red.
    fn red(&self, output: &str) -> usize {
        let out = Command::new("grim")
            .args(["-t", "ppm", "-o", output, "-"])
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .output()
            .unwrap();
        assert!(out.status.success(), "grim -o {output}");
        let (w, px) = ppm(&out.stdout);
        px[ROW * w * 3..(ROW + 1) * w * 3]
            .chunks(3)
            .filter(|p| *p == RED)
            .count()
    }

    /// Waits for `cond` (at most [`PATIENCE`]), strand alive throughout.
    fn until(&mut self, what: &str, mut cond: impl FnMut(&Self) -> Result<(), String>) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.alive();
            let why = match cond(self) {
                Ok(()) => return,
                Err(why) => why,
            };
            assert!(
                Instant::now() < deadline,
                "never: {what}: {why}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The focus is `output` in every export, and the panel is on it,
    /// `wide` pixels wide (half that monitor), and not on `other`.
    fn focused_on(&mut self, output: &str, wide: usize, other: &str) {
        let want = (output.to_string(), output.to_string(), "1".to_string());
        self.until(&format!("the focus on {output}"), |d| {
            let got = d.focus();
            let (here, there) = (d.red(output), d.red(other));
            if got.as_ref() == Some(&want) && here == wide && there == 0 {
                Ok(())
            } else {
                Err(format!(
                    "exports {got:?}, red on {output} {here}, on {other} {there}"
                ))
            }
        });
    }
}

/// A binary PPM (`P6`): its width and its RGB bytes.
fn ppm(bytes: &[u8]) -> (usize, &[u8]) {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(std::str::from_utf8(&bytes[start..at]).unwrap().to_string());
    }
    assert_eq!(fields[0], "P6");
    let w: usize = fields[1].parse().unwrap();
    (w, &bytes[at + 1..])
}

/// `screens.focused`, the `focused` flags and a `screens: focused`
/// panel's `screen` and place follow sway's focus from one monitor to
/// the other and back.
#[test]
fn the_focused_screen_follows_the_compositor_across_two_monitors() {
    let Some(mut desk) = start("follow", SHELL, "HEADLESS-1", true) else {
        return;
    };
    desk.focused_on("HEADLESS-1", 640, "HEADLESS-2");
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-2"]);
    desk.focused_on("HEADLESS-2", 800, "HEADLESS-1");
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-1"]);
    desk.focused_on("HEADLESS-1", 640, "HEADLESS-2");
}

/// Without the compositor's IPC nothing names the focused monitor: the
/// first one is `screens.focused` and the only one flagged, though
/// sway's focus is on the second, and moving it changes nothing.
#[test]
fn without_an_adapter_the_first_monitor_is_focused() {
    let Some(mut desk) = start("first", SHELL, "HEADLESS-2", false) else {
        return;
    };
    let first = (
        "HEADLESS-1".to_string(),
        "HEADLESS-1".to_string(),
        "1".to_string(),
    );
    desk.until("the first monitor focused", |d| match d.focus() {
        Some(f) if f == first => Ok(()),
        got => Err(format!("{got:?}")),
    });
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-1"]);
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-2"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(desk.focus(), Some(first), "{}", desk.log_text());
}

/// The compositor service runs for the focus only while something reads
/// it: not for a shell that reads none, from a reload that adds a reader
/// of `screens.focused` on (which then follows sway), and no longer once
/// a reload takes the reader away again (5 s after, as a service stops).
#[test]
fn the_compositor_runs_for_the_focus_only_while_it_is_read() {
    let Some(mut desk) = start("cost", PLAIN, "HEADLESS-2", true) else {
        return;
    };
    desk.until("strand answers", |d| match d.get("n") {
        Some(n) if n == "1" => Ok(()),
        got => Err(format!("n = {got:?}")),
    });
    let quiet = Instant::now() + Duration::from_secs(1);
    while Instant::now() < quiet {
        assert!(!desk.hub_runs(), "the compositor service runs for nobody");
        std::thread::sleep(Duration::from_millis(50));
    }
    desk.reload(READER, "2");
    desk.until("the focus followed", |d| match d.get("named") {
        Some(n) if n == "HEADLESS-2" && d.hub_runs() => Ok(()),
        got => Err(format!("named = {got:?}, hub {}", d.hub_runs())),
    });
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-1"]);
    desk.until("the focus moved", |d| match d.get("named") {
        Some(n) if n == "HEADLESS-1" => Ok(()),
        got => Err(format!("named = {got:?}")),
    });
    desk.reload(PLAIN, "1");
    desk.until("the compositor service stopped", |d| {
        if d.hub_runs() {
            Err("strand-toplevel still runs".into())
        } else {
            Ok(())
        }
    });
}

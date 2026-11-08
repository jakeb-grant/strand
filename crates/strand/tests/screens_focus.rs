//! (screens-focus) `screens.focused` and each `Screen.focused` follow the
//! compositor's focused monitor, end to end: `strand run` on a headless
//! sway with HEADLESS-1 (1280×720, at 0,0) and HEADLESS-2 (1600×900,
//! right of it), the focus moved between them with `swaymsg focus
//! output`. The config exports what it reads of the focus (read back
//! with `strand get`) and shows a panel whose `screens:` evaluates to
//! `focused`, half its `screen`'s width wide: it must show on the
//! focused monitor, sized from that monitor, and move with the focus,
//! while a panel with no `screens:` stays where sway put it.
//! Without sway's IPC (no compositor adapter) the first monitor is the
//! focused one, wherever sway's focus is. (screen-position) Each
//! `Screen` has its place in the layout, and `screens.all` follows the
//! layout as sway moves the monitors.
//!
//! Skipped, loudly, without sway, swaymsg or grim (CI sets
//! `STRAND_REQUIRE_SWAY`).

use std::os::unix::fs::PermissionsExt;
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

panel Plain { anchor: top_right; width: 100; height: 40; open: true
  box { width: 100; height: 40; bg: #00ff00 }
}
"#;

/// A shell that reads nothing of the focus.
const PLAIN: &str = "export let n = 1\nbar Top { height: 30\n  text screen.name\n}\n";

/// A shell whose only reader of the focus is a binding of
/// `screens.focused`.
const READER: &str = "export let n = 2\nexport let named = screens.focused?.name ?? \"none\"\n\
                      bar Top { height: 30\n  text screen.name\n}\n";

/// The panels' row of pixels looked at, and their colours: `Here`'s
/// (on the focused monitor, following it) and `Plain`'s (no `screens:`:
/// where the compositor put it).
const ROW: usize = 20;
const RED: [u8; 3] = [0xff, 0x00, 0x00];
const GREEN: [u8; 3] = [0x00, 0xff, 0x00];

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
    let mut cmd = strand_test_support::sway::command(dir, &cfg, log);
    let child = cmd.spawn().unwrap();
    let sway = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((d, i)) = &strand_test_support::sway::sockets_in(dir) {
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
    start_with(name, &[("shell.strand", shell)], focus, adapter, &[])
}

/// [`start`] with the config's `files`, and sway's `layout` commands run
/// before strand starts.
fn start_with(
    name: &str,
    files: &[(&str, &str)],
    focus: &str,
    adapter: bool,
    layout: &[&str],
) -> Option<Desk> {
    if !tools() {
        return None;
    }
    // Short: the IPC socket path must fit in sun_path.
    let dir = std::env::temp_dir().join(format!("strand-sf-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, sock) = sway(&dir);
    for cmd in layout {
        swaymsg(&sock, &[cmd]);
    }
    swaymsg(&sock, &["focus", "output", focus]);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    for (name, text) in files {
        std::fs::write(config.join(name), text).unwrap();
    }
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

    /// How many pixels of [`ROW`] on `output` are `Here`'s red.
    fn red(&self, output: &str) -> usize {
        self.count(output, RED)
    }

    /// How many pixels of [`ROW`] on `output` are `colour`.
    fn count(&self, output: &str, colour: [u8; 3]) -> usize {
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
            .filter(|p| *p == colour)
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

    /// The focus is `output` in every export, and `Here` is on it,
    /// `wide` pixels wide (half that monitor), and not on `other`;
    /// `Plain` is on `plain` only, 100 pixels wide.
    fn focused_on(&mut self, output: &str, wide: usize, other: &str, plain: &str) {
        let want = (output.to_string(), output.to_string(), "1".to_string());
        self.until(&format!("the focus on {output}"), |d| {
            let got = d.focus();
            let (here, there) = (d.red(output), d.red(other));
            let greens = [
                ("HEADLESS-1", d.count("HEADLESS-1", GREEN)),
                ("HEADLESS-2", d.count("HEADLESS-2", GREEN)),
            ];
            let plain_ok = greens
                .iter()
                .all(|(o, n)| *n == if *o == plain { 100 } else { 0 });
            if got.as_ref() == Some(&want) && here == wide && there == 0 && plain_ok {
                Ok(())
            } else {
                Err(format!(
                    "exports {got:?}, red on {output} {here}, on {other} {there}, \
                     Plain's green {greens:?}"
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
/// the other and back; a panel with no `screens:`, opened on the first
/// while the focus is followed, stays where sway put it.
#[test]
fn the_focused_screen_follows_the_compositor_across_two_monitors() {
    let Some(mut desk) = start("follow", SHELL, "HEADLESS-1", true) else {
        return;
    };
    desk.focused_on("HEADLESS-1", 640, "HEADLESS-2", "HEADLESS-1");
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-2"]);
    desk.focused_on("HEADLESS-2", 800, "HEADLESS-1", "HEADLESS-1");
    swaymsg(&desk.sock, &["focus", "output", "HEADLESS-1"]);
    desk.focused_on("HEADLESS-1", 640, "HEADLESS-2", "HEADLESS-1");
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

/// (screen-position) A shell that exports `screens.all` as the monitors
/// in its order, each with its place, and a bar on each showing it.
const PLACES: &str = r#"
export let n = 3
export let order = screens.all.map(s => join(" ", s.name, s.x, s.y)).join(", ")
export let first = screens.focused?.name ?? "none"
bar Top { height: 30
  text join(" ", screen.name, screen.x, screen.y)
}
"#;

/// (screen-position) Each `Screen` has its place in sway's layout and
/// `screens.all` lists them in layout order as sway moves them: left to
/// right first (HEADLESS-2 moved left of HEADLESS-1, then raised to its
/// right: it stays after it), then top to bottom at one `x`; with no
/// compositor adapter the first of that order is `screens.focused`.
#[test]
fn screens_all_follows_the_layout_as_the_monitors_move() {
    let Some(mut desk) = start("place", PLACES, "HEADLESS-1", false) else {
        return;
    };
    let layout = |desk: &mut Desk, cmd: Option<&str>, want: &str, first: &str| {
        if let Some(cmd) = cmd {
            swaymsg(&desk.sock, &[cmd]);
        }
        desk.until(&format!("screens.all {want}"), |d| {
            match (d.get("order"), d.get("first")) {
                (Some(o), Some(f)) if o == want && f == first => Ok(()),
                got => Err(format!("{got:?}")),
            }
        });
    };
    layout(
        &mut desk,
        None,
        "HEADLESS-1 0 0, HEADLESS-2 1280 0",
        "HEADLESS-1",
    );
    // HEADLESS-2 left of HEADLESS-1: it comes first, and is the focused
    // one while nothing names another.
    layout(
        &mut desk,
        Some("output HEADLESS-2 position 0 0, output HEADLESS-1 position 1600 0"),
        "HEADLESS-2 0 0, HEADLESS-1 1600 0",
        "HEADLESS-2",
    );
    // HEADLESS-2 to the right again but raised above HEADLESS-1's top:
    // left to right still decides.
    layout(
        &mut desk,
        Some("output HEADLESS-1 position 0 200, output HEADLESS-2 position 1280 0"),
        "HEADLESS-1 0 200, HEADLESS-2 1280 0",
        "HEADLESS-1",
    );
    // One above the other: top to bottom.
    layout(
        &mut desk,
        Some("output HEADLESS-2 position 0 0, output HEADLESS-1 position 0 900"),
        "HEADLESS-2 0 0, HEADLESS-1 0 900",
        "HEADLESS-2",
    );
    // A reload keeps it.
    desk.reload(&PLACES.replace("let n = 3", "let n = 4"), "4");
    layout(
        &mut desk,
        None,
        "HEADLESS-2 0 0, HEADLESS-1 0 900",
        "HEADLESS-2",
    );
}

/// (output-change) A panel centred on one axis, with a shadow: it takes
/// its output's width at any overhang (decisions.md centred-surface).
const SPANNED: &str = r#"
export state tall = 40
panel Island { screens: all; anchor: top; margin: 6, 0, 0
  size: auto ~ spring(400, 0.8)
  box { width: 200; height: tall; radius: 20; bg: #222222; shadow: 0 6px 18px #000000 }
}
"#;

impl Desk {
    /// The size `strand report` gives the only surface on `output`.
    fn panel_size(&self, output: &str) -> Option<(u32, u32)> {
        let reports = self.dir.join("state/strand/reports");
        let _ = std::fs::remove_dir_all(&reports);
        let run = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["report", "probe"])
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("STRAND_SOCKET", self.dir.join("strand.sock"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !run.status.success() {
            return None;
        }
        let bundle = std::fs::read_dir(&reports).ok()?.next()?.ok()?.path();
        let text = std::fs::read_to_string(bundle.join("diagnostics.txt")).ok()?;
        let line = text
            .lines()
            .find(|l| l.contains("(panel)") && l.contains(&format!("on {output}:")))?;
        let size = line.split(": ").nth(1)?.split(' ').next()?;
        let (w, h) = size.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }

    fn panel_is(&mut self, output: &str, width: u32) {
        self.until(
            &format!("the panel {width} wide on {output}"),
            |d| match d.panel_size(output) {
                Some(s) if s.0 == width => Ok(()),
                got => Err(format!("{got:?}")),
            },
        )
    }
}

/// (output-change) An output's mode and scale changing after its surface
/// was made reach that surface, whether or not anything on it changes: it
/// stays as wide as its output, where it kept the width it was given (1920
/// on an output of 1706 logical pixels, or 1762 once its content changed).
#[test]
fn a_spanned_panel_follows_its_outputs_mode_and_scale() {
    let Some(mut desk) = start("span", SPANNED, "HEADLESS-1", true) else {
        return;
    };
    desk.panel_is("HEADLESS-2", 1600);
    let sock = desk.sock.clone();
    let mode = |w: &str, h: &str, scale: &str| {
        swaymsg(
            &sock,
            &[
                "output",
                "HEADLESS-2",
                "resolution",
                &format!("{w}x{h}"),
                "scale",
                scale,
            ],
        );
    };
    mode("2560", "1440", "1.5");
    desk.panel_is("HEADLESS-2", 1706);
    let before = desk.panel_size("HEADLESS-2").unwrap();
    // Its content changes, and it stays so.
    let out = Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(["set", "shell.tall", "60"])
        .env("XDG_RUNTIME_DIR", &desk.dir)
        .env("WAYLAND_DISPLAY", &desk.display)
        .env("STRAND_SOCKET", desk.dir.join("strand.sock"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    desk.until("the panel grows", |d| match d.panel_size("HEADLESS-2") {
        Some(s) if s.1 > before.1 && s.0 == 1706 => Ok(()),
        got => Err(format!("{got:?}")),
    });
    mode("2560", "1440", "1");
    desk.panel_is("HEADLESS-2", 2560);
    // The other output was left alone.
    desk.panel_is("HEADLESS-1", 1280);
}

/// Sets HEADLESS-2's mode and scale.
fn set_mode(desk: &Desk, width: &str, height: &str, scale: &str) {
    swaymsg(
        &desk.sock,
        &[
            "output",
            "HEADLESS-2",
            "resolution",
            &format!("{width}x{height}"),
            "scale",
            scale,
        ],
    );
}

/// (output-change) A spanned panel, as its output shrinks and grows again.
#[test]
fn a_spanned_panel_shrinks_and_grows_with_its_output() {
    let Some(mut desk) = start("spsh", SPANNED, "HEADLESS-1", true) else {
        return;
    };
    desk.panel_is("HEADLESS-2", 1600);
    set_mode(&desk, "2560", "1440", "1");
    desk.panel_is("HEADLESS-2", 2560);
    set_mode(&desk, "2560", "1440", "1.5");
    desk.panel_is("HEADLESS-2", 1706);
    set_mode(&desk, "1280", "720", "1");
    desk.panel_is("HEADLESS-2", 1280);
    set_mode(&desk, "2560", "1440", "1");
    desk.panel_is("HEADLESS-2", 2560);
    desk.panel_is("HEADLESS-1", 1280);
}

/// (output-change) A panel wider than its output is capped to the output as
/// it is now, not as it was when the panel was made: it follows the output
/// down and, up to what it asks for, back up.
fn a_wide_panel_follows(name: &str, shell: &str) {
    let Some(mut desk) = start(name, shell, "HEADLESS-1", true) else {
        return;
    };
    desk.panel_is("HEADLESS-2", 1600);
    set_mode(&desk, "2560", "1440", "1");
    desk.panel_is("HEADLESS-2", 2000);
    set_mode(&desk, "2560", "1440", "1.5");
    desk.panel_is("HEADLESS-2", 1706);
    set_mode(&desk, "1280", "720", "1");
    desk.panel_is("HEADLESS-2", 1280);
    set_mode(&desk, "2560", "1440", "1");
    desk.panel_is("HEADLESS-2", 2000);
    desk.panel_is("HEADLESS-1", 1280);
}

#[test]
fn a_content_sized_panel_is_capped_to_its_output_as_it_changes() {
    a_wide_panel_follows(
        "wide",
        r#"
panel Wide { screens: all; anchor: top_left; size: auto
  box { width: 2000; height: 40; bg: #222222 }
}
"#,
    );
}

#[test]
fn a_fixed_panel_is_capped_to_its_output_as_it_changes() {
    a_wide_panel_follows(
        "fixed",
        r#"
panel Wide { screens: all; anchor: top_left; width: 2000; height: 40; bg: #222222 }
"#,
    );
}

/// (output-id) A shell that exports each monitor's id as `outputs` and as
/// `screens` see it, by connector.
const IDS: &str = r#"
export let n = 5
export let outs = outputs.all.sort_by(o => o.name).map(o => o.name + "=" + o.id).join(", ")
export let scrs = screens.all.sort_by(s => s.name).map(s => s.name + "=" + s.id).join(", ")
export let first = outputs.all.first?.id == screens.all.filter(s => s.name == outputs.all.first?.name).first?.id
bar Top { height: 30
  text screen.name
}
"#;

/// (output-id) `Output.id` is `Screen.id` for every monitor shown, on a
/// real compositor: a saved layout can name a monitor the way `screens:`
/// does.
#[test]
fn an_outputs_id_is_its_screens_id_on_sway() {
    let Some(mut desk) = start("ids", IDS, "HEADLESS-1", false) else {
        return;
    };
    desk.until("outputs and screens agree on ids", |d| {
        match (d.get("outs"), d.get("scrs"), d.get("first")) {
            (Some(o), Some(s), Some(f)) if o.contains("HEADLESS-2=") && o == s && f == "true" => {
                Ok(())
            }
            got => Err(format!("{got:?}")),
        }
    });
    let outs = desk.get("outs").unwrap_or_default();
    assert!(
        outs.contains(" | "),
        "an id is make | model | description: {outs}"
    );
}

/// (blocks) site/how-to/monitor-profiles.md, whose `strand file=` blocks
/// the test below runs as they are written there.
const MONITOR_PROFILES_PAGE: &str = include_str!("../../../site/how-to/monitor-profiles.md");

/// The `strand file=NAME` blocks of a site page, in order.
fn page_files(page: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("```strand file=") else {
            continue;
        };
        let body: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        files.push((name.trim().to_string(), body.join("\n") + "\n"));
    }
    files
}

/// (blocks) What the test reads of `outputs`: the set's key as
/// monitors.strand makes it, and each monitor's id and mode.
const PROBE: &str = r#"
export let n = 6
export let setup = outputs.all.map(o => o.id).sort_by(i => i).join(" + ")
export let ids = outputs.all.map(o => o.name + "=" + o.id).join(";")
export let modes = outputs.all.map(o => join(" ", o.name, o.mode?.width, o.mode?.height, o.mode?.refresh)).join(";")
"#;

/// sway's outputs: name to (x, y, logical width, scale).
fn sway_rects(sock: &Path) -> std::collections::BTreeMap<String, (i64, i64, i64, f64)> {
    let all: serde_json::Value =
        serde_json::from_str(&swaymsg(sock, &["-t", "get_outputs"])).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .map(|o| {
            let r = &o["rect"];
            (
                o["name"].as_str().unwrap().to_string(),
                (
                    r["x"].as_i64().unwrap(),
                    r["y"].as_i64().unwrap(),
                    r["width"].as_i64().unwrap(),
                    o["scale"].as_f64().unwrap_or(1.0),
                ),
            )
        })
        .collect()
}

/// (blocks) The monitor profiles page on a real compositor: its files run
/// on headless sway with HEADLESS-1 at scale 2 and HEADLESS-2 below it.
/// Once `outputs` is ready the best guess puts them side by side (x the
/// running total of logical widths, y 0) through `outputs.apply`; the
/// compositor file and the JSON file are written from `outputs.all` with
/// each monitor's `id`; workspaces.strand, its big monitor HEADLESS-2,
/// puts workspace 1 there when the shell starts (review round 1: it named
/// no `ready` and did nothing at start); a profile written by hand as `[[…]]` tables,
/// naming the monitors by `id`, is read live and applied, and the
/// compositor file follows.
#[test]
fn the_monitor_profiles_page_arranges_sways_monitors() {
    // The big monitor is HEADLESS-2 here.
    let dell = "s.id.starts_with(\"Dell Inc. | DELL U2720Q\")";
    let page: Vec<(String, String)> = page_files(MONITOR_PROFILES_PAGE)
        .into_iter()
        .map(|(n, t)| {
            if n == "workspaces.strand" {
                assert!(t.contains(dell), "{t}");
            }
            (n, t.replace(dell, "s.name == \"HEADLESS-2\""))
        })
        .collect();
    let mut files: Vec<(&str, &str)> = page.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
    assert_eq!(files.len(), 4, "{files:?}");
    files.push(("probe.strand", PROBE));
    let Some(mut desk) = start_with(
        "prof",
        &files,
        "HEADLESS-1",
        true,
        &[
            "output HEADLESS-1 scale 2",
            "output HEADLESS-2 position 0 720",
        ],
    ) else {
        return;
    };
    let get = |d: &Desk, name: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["get", &format!("probe.{name}")])
            .env("XDG_RUNTIME_DIR", &d.dir)
            .env("WAYLAND_DISPLAY", &d.display)
            .env("STRAND_SOCKET", d.dir.join("strand.sock"))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_matches('"')
                .to_string()
        })
    };
    // The best guess: side by side, the second at the first's logical
    // width.
    desk.until("the best guess on sway", |d| {
        let r = sway_rects(&d.sock);
        let mut v: Vec<_> = r.values().copied().collect();
        v.sort_by_key(|o| o.0);
        match v.as_slice() {
            [a, b] if a.0 == 0 && a.1 == 0 && b.1 == 0 && b.0 == a.2 => Ok(()),
            _ => Err(format!("{r:?}")),
        }
    });
    // Workspace 1, on HEADLESS-1 at start, is on the big monitor.
    desk.until("workspace 1 on the big monitor from the start", |d| {
        let all: serde_json::Value =
            serde_json::from_str(&swaymsg(&d.sock, &["-t", "get_workspaces"])).unwrap();
        match all.as_array().unwrap().iter().find(|w| w["name"] == "1") {
            Some(w) if w["output"] == "HEADLESS-2" => Ok(()),
            w => Err(format!("{w:?}")),
        }
    });
    let home = desk.dir.join("home");
    let conf = home.join(".config/hypr/monitors.lua");
    // The compositor file follows sway's layout: a line for each monitor
    // at its place and scale (a whole number written whole).
    let follows = |d: &Desk| -> Result<(), String> {
        let text = std::fs::read_to_string(&conf).unwrap_or_default();
        for (name, (x, y, _, s)) in sway_rects(&d.sock) {
            let scale = if s.fract() == 0.0 {
                format!("{}", s as i64)
            } else {
                format!("{s}")
            };
            let place = format!("position = \"{x}x{y}\", scale = {scale}, transform = 0 }})");
            if !text.lines().any(|l| {
                l.starts_with(&format!("hl.monitor({{ output = \"{name}\", "))
                    && l.ends_with(&place)
            }) {
                return Err(format!("no line for {name}{place} in {text:?}"));
            }
        }
        Ok(())
    };
    desk.until("monitors.lua follows sway", follows);
    // The JSON file has every monitor's id.
    let ids = get(&desk, "ids").unwrap_or_default();
    let json = std::fs::read_to_string(home.join(".cache/monitors.json")).unwrap();
    let pairs: Vec<(String, String)> = ids
        .split(';')
        .map(|p| {
            let (n, i) = p.split_once('=').unwrap();
            (n.to_string(), i.to_string())
        })
        .collect();
    assert_eq!(pairs.len(), 2, "{ids}");
    for (name, id) in &pairs {
        assert!(
            json.contains(&format!("[\"{id}\",\"{name}\"]")),
            "{name}={id} in {json}"
        );
    }
    // A profile by hand, in `[[…]]` form, naming the monitors by id:
    // HEADLESS-2 first, HEADLESS-1 right of it at scale 1.
    let setup = get(&desk, "setup").unwrap();
    let modes = get(&desk, "modes").unwrap();
    let id_of = |name: &str| pairs.iter().find(|(n, _)| n == name).unwrap().1.clone();
    let mode_of = |name: &str| -> (String, String, f64) {
        let m = modes
            .split(';')
            .find(|m| m.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} in {modes}"));
        let p: Vec<&str> = m.split(' ').collect();
        (p[1].into(), p[2].into(), p[3].parse().unwrap())
    };
    let mon = |name: &str, x: i64| {
        let (w, h, r) = mode_of(name);
        format!(
            "[[profiles.\"{setup}\".mons]]\nid = \"{}\"\non = true\nwidth = {w}\nheight = {h}\n\
             refresh = {r:?}\nx = {x}\ny = 0\nscale = 1.0\ntransform = \"normal\"\n",
            id_of(name)
        )
    };
    let profile = format!(
        "# Monitor profiles.\n\n# HEADLESS-2 on the left.\n{}\n{}",
        mon("HEADLESS-2", 0),
        mon("HEADLESS-1", 1600)
    );
    std::fs::write(desk.config.join("monitors.toml"), profile).unwrap();
    desk.until("the profile on sway", |d| {
        let r = sway_rects(&d.sock);
        match (r.get("HEADLESS-1"), r.get("HEADLESS-2")) {
            (Some(&(1600, 0, 1280, 1.0)), Some(&(0, 0, _, _))) => Ok(()),
            _ => Err(format!("{r:?}")),
        }
    });
    desk.until("monitors.lua follows the profile", follows);
    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(
        text.contains("position = \"1600x0\", scale = 1, transform = 0 })"),
        "{text}"
    );
}

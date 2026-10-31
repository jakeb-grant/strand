//! (presented-quiet) What follows a frame must follow one that draws nothing,
//! on a surface the GPU thread presents, under frames that send nothing:
//! `strand run` on a headless sway, the one path where a presented surface's
//! paint can send no frame (decisions.md wave-cost, gpu-stay, presented-quiet).
//! Every shell here promotes its panel with a large fade and then ends the
//! fade, so the panel stays presented and goes idle; what the test changes
//! next draws nothing, or nothing at first.
//!
//! An `eval-hooks` build draws every frame, and `cargo test --workspace`
//! builds strand with that feature on through the `eval` crate, so there these
//! tests prove only that the behaviour holds. CI's own step
//! (`cargo test -p strand --test presented_quiet`) is the proof: [`Desk::proved`]
//! asserts it ran the path where a paint sent nothing.
//!
//! Lavapipe presents on a pixman sway, which is what CI runs. The hardware
//! leg (`scripts/container/gpu.sh` with `STRAND_GPU_PRESENT=1`) starts the
//! sway on the render node, since ANV presents only to a compositor offering
//! linux-dmabuf. Skipped, loudly, without sway or grim or a device (CI sets
//! `STRAND_REQUIRE_SWAY` and `STRAND_REQUIRE_GPU`).

#![cfg(feature = "gpu")]
#![allow(dead_code)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::pointer::Pointer;

/// The layout's logical extent: HEADLESS-1 1280×720, then HEADLESS-2 1600×900.
const LW: u32 = 2880;
const LH: u32 = 900;

const RED: [u8; 3] = [0xff, 0x00, 0x00];
const YELLOW: [u8; 3] = [0xff, 0xff, 0x00];

/// How long a thing that must happen is waited for. The failures here last
/// for good, so this only has to outlast a loaded machine.
const SOON: Duration = Duration::from_secs(10);

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
            eprintln!("\n*** SKIPPED: {tool} is not installed; presented_quiet did not run ***\n");
            return false;
        }
    }
    true
}

fn swaymsg(sock: &Path, args: &[&str]) {
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
}

/// As `gpu_idle.rs`'s: pixman, unless the hardware leg asks to present and its
/// session names a GPU renderer that started on the render node.
fn sway_renderer() -> Option<String> {
    let on = |v: &str| std::env::var(v).as_deref() == Ok("1");
    std::env::var("STRAND_GPU_SWAY_RENDERER").ok().filter(|r| {
        on("STRAND_GPU_HARDWARE")
            && on("STRAND_GPU_PRESENT")
            && matches!(r.as_str(), "gles2" | "vulkan")
    })
}

/// A headless sway in `dir` with both outputs, its display and IPC socket.
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
    match sway_renderer() {
        Some(renderer) => cmd
            .env("WLR_RENDERER", renderer)
            .env("WLR_RENDER_DRM_DEVICE", "/dev/dri/renderD128"),
        None => cmd.env("WLR_RENDERER", "pixman"),
    };
    let child = cmd
        .arg("-c")
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WLR_BACKENDS", "headless")
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

/// `strand run` on `shell`, with the sway its panels go on. `None`: skipped.
fn start(name: &str, shell: &str) -> Option<Desk> {
    if !tools() {
        return None;
    }
    // Short: the IPC socket path must fit in sun_path.
    let dir = std::env::temp_dir().join(format!("strand-pq-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sway, display, sock) = sway(&dir);
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
            .env("STRAND_SOCKET", dir.join("strand.sock"))
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("STRAND_LOG", "debug")
            .env_remove("STRAND_MOCK")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("NIRI_SOCKET")
            .env_remove("SWAYSOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    Some(Desk {
        dir,
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

    fn set(&self, name: &str, value: &str) -> bool {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["set", &format!("shell.{name}"), value])
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("STRAND_SOCKET", self.dir.join("strand.sock"))
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn set_state(&self, name: &str, value: &str) {
        assert!(self.set(name, value), "strand set {name} {value}");
    }

    /// Waits for `what` to hold, with the log on failure.
    fn until<T>(&mut self, what: &str, mut look: impl FnMut(&Self) -> Option<T>) -> T {
        let deadline = Instant::now() + SOON;
        loop {
            if let Some(v) = look(self) {
                return v;
            }
            if let Some(p) = &mut self.strand {
                assert!(
                    p.0.try_wait().unwrap().is_none(),
                    "strand exited: {}",
                    self.log_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "never: {what}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for the fade to promote the panel to the swapchain and then ends
    /// it: a panel that presents, idle. False when there is no device or the
    /// hardware leg's sway cannot be presented to (skipped).
    fn promoted(&mut self) -> bool {
        let required = std::env::var_os("STRAND_REQUIRE_GPU").is_some();
        let hardware = std::env::var("STRAND_GPU_HARDWARE").as_deref() == Ok("1");
        let mode = self.until("the panel promoted", |d| {
            let text = d.log_text();
            if text.contains("attached (Present)") {
                return Some(true);
            }
            if text.contains("attached (Readback)") || text.contains("GPU unavailable") {
                return Some(false);
            }
            None
        });
        if !mode {
            assert!(
                !required || (hardware && sway_renderer().is_none()),
                "the panel does not present:\n{}",
                self.log_text()
            );
            eprintln!(
                "\n*** SKIPPED: the panel does not present; presented_quiet did not run ***\n"
            );
            return false;
        }
        let d: &Self = self;
        d.set_state("boost", "false");
        // The last frame of the fade is on its way, and the next paint of the
        // panel has no frame to send.
        std::thread::sleep(Duration::from_millis(500));
        true
    }

    /// In a build with quiet frames, the path under test was run: a paint of a
    /// presented surface sent nothing.
    fn proved(&self) {
        let text = self.log_text();
        assert!(
            !strand_render::QUIET_FRAMES_BUILT || text.contains("a paint that sent nothing"),
            "no paint sent nothing, so this proved nothing:\n{text}"
        );
    }

    /// The panel's size on `output`, as `strand report` says the surface
    /// manager has it. `None`: no such panel.
    fn panel_size(&self, output: &str) -> Result<Option<(u32, u32)>, String> {
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
            .map_err(|e| e.to_string())?;
        if !run.status.success() {
            return Err(format!("{run:?}"));
        }
        let bundle = std::fs::read_dir(&reports)
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no bundle")?
            .map_err(|e| e.to_string())?
            .path();
        let text =
            std::fs::read_to_string(bundle.join("diagnostics.txt")).map_err(|e| e.to_string())?;
        let Some(line) = text
            .lines()
            .find(|l| l.contains("(panel)") && l.contains(&format!("on {output}:")))
        else {
            return Ok(None);
        };
        let size = line.split(": ").nth(1).ok_or("no size")?;
        let size = size.split(' ').next().ok_or("no size")?;
        let (w, h) = size.split_once('x').ok_or("no size")?;
        Ok(Some((
            w.parse().map_err(|_| "no width")?,
            h.parse().map_err(|_| "no height")?,
        )))
    }

    fn shot(&self, output: &str) -> Img {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    fn pointer(&self) -> Pointer {
        Pointer::new(&self.dir.join(&self.display))
    }
}

struct Img {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Img {
    /// A binary PPM: "P6\n<w> <h>\n255\n", then the pixels.
    fn ppm(data: &[u8]) -> Img {
        let mut at = 0;
        let mut fields = Vec::new();
        while fields.len() < 4 {
            let start = at;
            while !data[at].is_ascii_whitespace() {
                at += 1;
            }
            fields.push(std::str::from_utf8(&data[start..at]).unwrap().to_string());
            at += 1;
        }
        Img {
            w: fields[1].parse().unwrap(),
            h: fields[2].parse().unwrap(),
            rgb: data[at..].to_vec(),
        }
    }

    /// The leftmost column holding at least `min` pixels of `colour`.
    fn left_of(&self, colour: [u8; 3], min: usize) -> Option<usize> {
        (0..self.w).find(|x| {
            (0..self.h)
                .filter(|y| {
                    let i = (y * self.w + x) * 3;
                    self.rgb[i..i + 3] == colour
                })
                .count()
                >= min
        })
    }

    fn has(&self, colour: [u8; 3], min: usize) -> bool {
        self.rgb.chunks_exact(3).filter(|p| *p == colour).count() >= min
    }
}

/// A box of 0.6 Mpx fading while `boost`: what promotes the panel.
const FADE: &str = "box { place: absolute; width: 1000; height: 600; bg: #303030
    opacity: boost ? 0.5 + 0.5 * wave(600ms) : 1 }";

/// (presented-quiet) A time signal moving less than a pixel a frame keeps
/// moving on a presented surface: its frames that draw nothing send nothing,
/// and it used to stop at the first of them, for good.
#[test]
fn a_time_signal_keeps_moving_on_a_presented_surface() {
    let shell = format!(
        r#"
export state boost = true
panel P {{ anchor: top_left; width: 1100; height: 620
  {FADE}
  box {{ place: absolute; x: 300 + 200 * wave(16s); y: 0; width: 20; height: 20; bg: #ff0000 }}
}}
"#
    );
    let Some(mut d) = start("clock", &shell) else {
        return;
    };
    if !d.promoted() {
        return;
    }
    let mut seen = Vec::new();
    for _ in 0..5 {
        let x = d.until("the box", |d| d.shot("HEADLESS-1").left_of(RED, 10));
        seen.push(x);
        std::thread::sleep(Duration::from_millis(1500));
    }
    let span = seen.iter().max().unwrap() - seen.iter().min().unwrap();
    assert!(span >= 20, "it stopped: {seen:?}\n{}", d.log_text());
    d.proved();
}

/// A panel closing with an exit that only moves it: its last frames draw
/// nothing new, and that paint is the one that ends the exit.
#[test]
fn a_presented_panel_whose_exit_only_moves_it_closes() {
    let shell = format!(
        r#"
export state boost = true
export state shown = true
panel P {{ anchor: top_left; width: 1000; height: 620; open: shown
  exit {{ y: -20 }}
  {FADE}
}}
"#
    );
    let Some(mut d) = start("exit", &shell) else {
        return;
    };
    if !d.promoted() {
        return;
    }
    assert!(matches!(d.panel_size("HEADLESS-1"), Ok(Some(_))));
    d.set_state("shown", "false");
    d.until("the panel closed", |d| {
        matches!(d.panel_size("HEADLESS-1"), Ok(None)).then_some(())
    });
    d.proved();
}

/// A content-sized panel whose box shrinks with no spring on the panel's size:
/// the shrink is queued by a paint that draws nothing.
#[test]
fn a_presented_panel_shrinks_when_its_last_frame_draws_nothing() {
    let shell = format!(
        r#"
export state boost = true
export state w = 1000
panel P {{ anchor: top_left; size: auto
  box {{ width: w; height: 620; bg: #202020 }}
  {FADE}
}}
"#
    );
    let Some(mut d) = start("shrink", &shell) else {
        return;
    };
    if !d.promoted() {
        return;
    }
    d.set_state("w", "300");
    d.until("the panel shrunk", |d| {
        matches!(d.panel_size("HEADLESS-1"), Ok(Some((300, _)))).then_some(())
    });
    d.proved();
}

/// (hit-through) A panel behind, and a promoted one over it whose root's
/// `hit: none` is set by a `when`. A big fading box at x 540 leaves the panel's
/// empty area free to click.
fn through_shell(through: bool) -> String {
    format!(
        r#"
export state boost = true
export state through = {through}
state back = false
panel Behind {{ anchor: top_left; layer: bottom; width: 300; height: 100
  box {{ width: 300; height: 100; bg: #ff0000
    when back {{ bg: #ffff00 }}
    on click {{ back = !back }}
  }}
}}
panel Front {{ anchor: top_left; width: 1000; height: 620
  when through {{ hit: none }}
  box {{ place: absolute; x: 540; width: 440; height: 600; bg: #303030
    opacity: boost ? 0.5 + 0.5 * wave(600ms) : 1 }}
}}
"#
    )
}

/// (hit-through) Turned on with nothing else on the panel changing, after the
/// panel has sent presented frames: a click on its empty area passes through.
#[test]
fn a_reactive_hit_none_on_a_presented_surface_lets_clicks_through() {
    let Some(mut d) = start("thon", &through_shell(false)) else {
        return;
    };
    if !d.promoted() {
        return;
    }
    let mut pointer = d.pointer();
    pointer.motion(50, 50, LW, LH);
    std::thread::sleep(Duration::from_millis(200));
    pointer.click(50, 50, LW, LH);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !d.shot("HEADLESS-1").has(YELLOW, 1),
        "the front panel takes it while it is off"
    );
    d.set_state("through", "true");
    std::thread::sleep(Duration::from_millis(500));
    pointer.motion(60, 50, LW, LH);
    std::thread::sleep(Duration::from_millis(200));
    pointer.click(60, 50, LW, LH);
    d.until("the click reached the panel behind", |d| {
        d.shot("HEADLESS-1").has(YELLOW, 2000).then_some(())
    });
    d.proved();
}

/// (hit-through) Turned off the same way: the panel takes its clicks again, and
/// the panel behind hears nothing.
#[test]
fn a_reactive_hit_none_turned_off_on_a_presented_surface_takes_clicks_again() {
    let Some(mut d) = start("thoff", &through_shell(true)) else {
        return;
    };
    if !d.promoted() {
        return;
    }
    d.set_state("through", "false");
    std::thread::sleep(Duration::from_millis(500));
    let mut pointer = d.pointer();
    pointer.motion(60, 50, LW, LH);
    std::thread::sleep(Duration::from_millis(200));
    pointer.click(60, 50, LW, LH);
    std::thread::sleep(Duration::from_millis(500));
    let one = d.shot("HEADLESS-1");
    assert!(
        !one.has(YELLOW, 1),
        "the click went through the front panel"
    );
    assert!(one.has(RED, 2000), "the panel behind is there");
    d.proved();
}

//! S-docs: the newcomer exit's automated layers ("Newcomer builds a
//! multi-monitor bar in 15 minutes", features.md M5).
//!
//! - `the_guide_builds_bar_a_on_two_outputs` (layer 1, a gate): a fresh
//!   `$HOME`, `strand new`, then `strand run` on a headless sway with two
//!   outputs (`HEADLESS-1` 2560x1440 at scale 1, `HEADLESS-2` 1920x1080
//!   at 1.25, as in acceptance.rs) on the deterministic mock services
//!   (`STRAND_MOCK=acceptance`: the clock frozen at Mon 5 Oct 2026 09:41
//!   UTC, a fixed window title). Each step of the guide's levels 1 to 3,
//!   in page order, is saved into the running shell, its code block byte
//!   for byte (`docs/guide/level-N.md`, read by strand-dev's
//!   `tests/support/guide.rs`). After each step: `strand check` reports
//!   no diagnostic, `strand watch --json` hears the save's reload with
//!   the kept and reset cells design.md's identity rules predict for it
//!   ([`PREDICTED`]), and each output's bar matches its reference
//!   (`tests/refs/newcomer/`; one settled shot per output). Last, a
//!   click on the clock opens the calendar, and the agent proxy's
//!   checklist (`docs/guide/target-checklist.md`) scores the result in
//!   full, which proves the scorer on the guide's own bar. The whole test
//!   is meant to take 120 s or less in the container: a `timing gate
//!   missed` panic at its end says by how much it did not. It runs in
//!   the strict `test` step (not a timing step), so a miss fails there.
//! - `agent_proxy_score` (layer 2, advisory, ignored): scores another
//!   config directory against the same checklist, for the agent-proxy
//!   harness (`scripts/agent-proxy/`): `STRAND_PROXY_CONFIG=<dir>`.
//!
//! `STRAND_UPDATE_REFS=1` rewrites the references (read every one before
//! committing it), `STRAND_SHOTS=<dir>` keeps every shot. Skipped, loudly,
//! without sway or grim (CI sets `STRAND_REQUIRE_SWAY`).

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

#[allow(dead_code)]
#[path = "../../../strand-dev/tests/support/guide.rs"]
mod guide;
use crate::support;

/// The cursor theme and size of every process on the desk, pinned: the
/// calendar shot is taken with the pointer on the desktop.
const CURSOR_THEME: &str = "Adwaita";
const CURSOR_SIZE: &str = "24";

/// The layout: `HEADLESS-1` at 0,0 (2560x1440 logical), `HEADLESS-2`
/// right of it (1920x1080 at 1.25: 1536x864 logical).
const LAYOUT: (u32, u32) = (2560 + 1536, 1440);

/// The bar of `HEADLESS-1` with its margin and shadow, and of
/// `HEADLESS-2` in its 1.25 buffer pixels.
const BAR_1: Rect = rect(0, 0, 2560, 64);
const BAR_2: Rect = rect(0, 0, 1920, 80);

/// The whole test's budget in the container.
const BUDGET: Duration = Duration::from_secs(120);

/// A wall-clock gate's miss (`scripts/container/gate-misses.sh`).
const GATE_MISS: &str = "timing gate missed";

/// The guide's steps the newcomer test applies: levels 1 to 3, which end
/// at design.md's bar (a) (m5-owner Q15).
fn steps() -> Vec<guide::Block> {
    guide::blocks()
        .into_iter()
        .filter(|b| b.level <= 3)
        .collect()
}

/// The state cells each step's reload keeps and resets (design.md,
/// "Identity and state"; spec.md 10.1 and 10.2), by the step's place in
/// [`steps`], from the second step on (the first is what `strand new`
/// wrote, so saving it changes nothing and no reload runs).
///
/// Levels 1 and 2 declare no `state`, so nothing is kept or reset. Level
/// 3's first step adds `clock.strand`, which no surface uses yet; the
/// next puts `Clock` in the bar, so its `open` is new on each monitor's
/// bar, and the old centre `text` that `Clock` replaced had no state:
/// still nothing kept or reset. From then on every save keeps each
/// monitor's `Clock.open` (the calendar's `month` is not mounted: its
/// popup is closed) and resets nothing: no step moves state into a new
/// component.
const PREDICTED: &[(&str, &[&str], &[&str])] = &[
    // (block's file, kept, reset)
    ("bar.strand", &[], &[]),
    ("bar.strand", &[], &[]),
    ("dot.strand", &[], &[]),
    ("bar.strand", &[], &[]),
    ("bar.strand", &[], &[]),
    ("clock.strand", &[], &[]),
    ("bar.strand", &[], &[]),
    ("volume.strand", &[CLOCK_OPEN, CLOCK_OPEN], &[]),
    ("bar.strand", &[CLOCK_OPEN, CLOCK_OPEN], &[]),
    ("battery.strand", &[CLOCK_OPEN, CLOCK_OPEN], &[]),
    ("bar.strand", &[CLOCK_OPEN, CLOCK_OPEN], &[]),
    ("dot.strand", &[CLOCK_OPEN, CLOCK_OPEN], &[]),
];

/// `Clock.open`, as `strand watch` names it: by the component and the
/// cell, once per instance (one `Clock` on each monitor's bar).
const CLOCK_OPEN: &str = "Clock.open";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether sway and grim are here (CI requires them).
fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the newcomer test did not run ***\n"
            );
            return false;
        }
    }
    true
}

/// A headless sway with two outputs, a fresh `$HOME`, and (once
/// [`Desk::run`] is called) `strand run` and `strand watch --json` on its
/// config.
struct Desk {
    strand: Option<Proc>,
    watch: Option<(Proc, mpsc::Receiver<Json>)>,
    _sway: Proc,
    dir: PathBuf,
    display: String,
    ipc: PathBuf,
    log: PathBuf,
    tag: &'static str,
    shots: std::cell::Cell<u32>,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.watch.take();
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Desk {
    fn start(tag: &'static str) -> Desk {
        // Short: the socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-nc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            format!(
                "xwayland disable\nseat * xcursor_theme {CURSOR_THEME} {CURSOR_SIZE}\n\
                 output HEADLESS-1 resolution 2560x1440 position 0 0 scale 1\n"
            ),
        )
        .unwrap();
        let log = std::fs::File::create(dir.join("sway.log")).unwrap();
        let mut cmd = Command::new("sway");
        // SAFETY: the hook runs between fork and exec and makes one
        // async-signal-safe syscall.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(std::io::Error::from)
            });
        }
        let child = cmd
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("XCURSOR_THEME", CURSOR_THEME)
            .env("XCURSOR_SIZE", CURSOR_SIZE)
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
        let mut desk = Desk {
            strand: None,
            watch: None,
            _sway: Proc(child),
            log: dir.join("strand.log"),
            dir,
            display: String::new(),
            ipc: PathBuf::new(),
            tag,
            shots: std::cell::Cell::new(0),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: Vec<String> = std::fs::read_dir(&desk.dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = names
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc) {
                desk.display = d.clone();
                desk.ipc = desk.dir.join(i);
                if desk.msg(&["-t", "get_version"]).is_some() {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        desk.msg(&["create_output"]).unwrap();
        desk.msg(&[
            "output",
            "HEADLESS-2",
            "resolution",
            "1920x1080",
            "position",
            "2560",
            "0",
            "scale",
            "1.25",
        ])
        .unwrap();
        desk.msg(&["focus", "output", "HEADLESS-1"]).unwrap();
        std::fs::create_dir_all(desk.home()).unwrap();
        desk
    }

    /// The newcomer's `$HOME`, empty until `strand new`.
    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    /// `strand new`'s default directory: `~/.config/strand`.
    fn config(&self) -> PathBuf {
        self.home().join(".config/strand")
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
            ("HOME", self.home()),
            ("XDG_CACHE_HOME", self.dir.join("cache")),
            ("XDG_STATE_HOME", self.dir.join("state")),
            ("XCURSOR_THEME", PathBuf::from(CURSOR_THEME)),
            ("XCURSOR_SIZE", PathBuf::from(CURSOR_SIZE)),
        ]
    }

    /// `strand <args>` as the newcomer types it: their `$HOME`, no
    /// `XDG_CONFIG_HOME`, the desk's display.
    fn strand(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("STRAND_SOCKET")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `strand run` (with no directory: the default one `strand new`
    /// wrote) on the acceptance mock, then `strand watch --json`; ready
    /// once a bar is painted on each output.
    fn run(&mut self, config: Option<&Path>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
        cmd.arg("run");
        if let Some(dir) = config {
            cmd.arg(dir);
        }
        let child = cmd
            .envs(self.env())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("STRAND_SOCKET")
            .env("STRAND_MOCK", "acceptance")
            .env("STRAND_LOG", "damage")
            // No portal: the built-in theme's light scheme.
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&self.log).unwrap())
            .spawn()
            .unwrap();
        self.strand = Some(Proc(child));
        self.wait("a surface on each output", 30, |d| {
            d.surface_count() >= 2 && d.surfaces().iter().any(|b| b.contains("scale=1.25 "))
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let (child, events) = loop {
            let mut child = Command::new(env!("CARGO_BIN_EXE_strand"))
                .args(["watch", "--json"])
                .envs(self.env())
                .env_remove("STRAND_SOCKET")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let out = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(v) = serde_json::from_str::<Json>(&line)
                        && tx.send(v).is_err()
                    {
                        break;
                    }
                }
            });
            // A watcher that could not connect exits; one that did stays.
            std::thread::sleep(Duration::from_millis(300));
            if child.try_wait().unwrap().is_none() {
                break (child, rx);
            }
            assert!(Instant::now() < deadline, "strand watch never connected");
        };
        self.watch = Some((Proc(child), events));
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

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Each painted frame's surface, buffer and scale, as
    /// `STRAND_LOG=damage` prints them, each word followed by a space.
    fn surfaces(&self) -> Vec<String> {
        self.log_text()
            .lines()
            .filter(|l| l.starts_with("strand: damage"))
            .map(|l| {
                l.split_whitespace()
                    .filter(|w| {
                        w.starts_with("surface=")
                            || w.starts_with("buffer=")
                            || w.starts_with("scale=")
                    })
                    .map(|w| format!("{w} "))
                    .collect::<String>()
            })
            .collect()
    }

    /// Distinct surfaces painted so far.
    fn surface_count(&self) -> usize {
        let mut s: Vec<String> = self
            .surfaces()
            .iter()
            .filter_map(|l| l.split_whitespace().next().map(String::from))
            .collect();
        s.sort();
        s.dedup();
        s.len()
    }

    fn frames(&self) -> usize {
        self.log_text()
            .lines()
            .filter(|l| l.starts_with("strand: damage"))
            .count()
    }

    fn assert_alive(&mut self) {
        if let Some(p) = &mut self.strand {
            assert!(
                p.0.try_wait().unwrap().is_none(),
                "{}: strand exited: {}",
                self.tag,
                self.log_text()
            );
        }
    }

    /// Waits up to `secs` for `done`, failing with the log if strand exits.
    fn wait(&mut self, what: &str, secs: u64, done: impl Fn(&Desk) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(self) {
            self.assert_alive();
            assert!(
                Instant::now() < deadline,
                "{}: {what}\n{}",
                self.tag,
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// The next `strand watch` event of kind `kind`, up to `secs`.
    fn event(&mut self, kind: &str, secs: u64) -> Json {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.assert_alive();
            let left = deadline.saturating_duration_since(Instant::now());
            let (_, rx) = self.watch.as_ref().expect("Desk::run started the watch");
            match rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(ev) if ev["event"] == kind => return ev,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{}: strand watch stopped\n{}", self.tag, self.log_text())
                }
            }
            assert!(
                Instant::now() < deadline,
                "{}: no `{kind}` event\n{}",
                self.tag,
                self.log_text()
            );
        }
    }

    /// One request over the socket, its answer.
    fn request(&self, req: Json) -> Json {
        let mut req = req;
        req["v"] = 1.into();
        let sock = self.dir.join(format!("strand-{}.sock", self.display));
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The inspector's dump of every surface (`inspect.dump`).
    fn dump(&self) -> Json {
        let ans = self.request(serde_json::json!({"cmd": "inspect.dump"}));
        assert_eq!(ans["ok"], true, "inspect.dump: {ans}");
        ans
    }

    fn pointer(&self) -> support::pointer::Pointer {
        support::pointer::Pointer::new(&self.dir.join(&self.display))
    }

    /// The region `r` of `output`, in its buffer pixels.
    fn region(&mut self, output: &str, r: Rect) -> Img {
        self.assert_alive();
        let path = self.dir.join(format!("{output}.ppm"));
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim {output}");
        Img::ppm(&std::fs::read(&path).unwrap()).crop(r)
    }

    /// The region once neither it nor anything strand paints has changed
    /// for 500 ms.
    fn settled(&mut self, output: &str, r: Rect) -> Img {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = self.region(output, r);
        let mut frames = self.frames();
        let mut since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(60));
            let next = self.region(output, r);
            let painted = self.frames();
            if next != last || painted != frames {
                last = next;
                frames = painted;
                since = Instant::now();
            } else if since.elapsed() >= Duration::from_millis(500) {
                if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
                    let n = self.shots.get() + 1;
                    self.shots.set(n);
                    next.save(&PathBuf::from(dir).join(format!("{}-{n:02}.png", self.tag)));
                }
                return next;
            }
            assert!(
                Instant::now() < deadline,
                "{}: {output} {r:?} never settled",
                self.tag
            );
        }
    }

    /// The region once it has reached the reference `name` (up to 15 s),
    /// settled, and matched against it.
    fn settled_ref(&mut self, output: &str, r: Rect, name: &str) -> Img {
        if std::env::var_os("STRAND_UPDATE_REFS").is_none() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && self.region(output, r).compare(name).is_err() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let img = self.settled(output, r);
        img.matches_ref(name);
        img
    }

    fn errors(&self) -> Vec<String> {
        self.log_text()
            .lines()
            .filter(|l| l.contains("ERROR") || l.contains("panicked"))
            .map(String::from)
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

const fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
    Rect { x, y, w, h }
}

/// RGB pixels.
#[derive(Clone, PartialEq)]
struct Img {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

/// The comparison of acceptance.rs: a pixel differs when a channel is
/// more than 24 apart; at most 0.5% of the pixels may, and at most 4 in
/// any 4x4 block (a changed glyph fails, scattered antialiasing passes).
const CHANNEL_TOLERANCE: u8 = 24;
const PIXEL_TOLERANCE: f64 = 0.005;
const BLOCK_TOLERANCE: usize = 4;

impl Img {
    fn ppm(ppm: &[u8]) -> Img {
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
        Img {
            w,
            h,
            rgb: ppm[c + 1..c + 1 + w * h * 3].to_vec(),
        }
    }

    fn crop(&self, r: Rect) -> Img {
        assert!(r.x + r.w <= self.w && r.y + r.h <= self.h, "{r:?}");
        let mut rgb = Vec::with_capacity(r.w * r.h * 3);
        for y in r.y..r.y + r.h {
            let i = (y * self.w + r.x) * 3;
            rgb.extend_from_slice(&self.rgb[i..i + r.w * 3]);
        }
        Img {
            w: r.w,
            h: r.h,
            rgb,
        }
    }

    fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        image::RgbImage::from_raw(self.w as u32, self.h as u32, self.rgb.clone())
            .unwrap()
            .save(path)
            .unwrap();
    }

    fn refs() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/refs/newcomer")
    }

    fn compare(&self, name: &str) -> Result<(), (String, Img)> {
        let path = Img::refs().join(format!("{name}.png"));
        let reference = image::open(&path)
            .map_err(|e| (format!("reference {}: {e}", path.display()), self.clone()))?
            .to_rgb8();
        let (rw, rh) = (reference.width() as usize, reference.height() as usize);
        if (rw, rh) != (self.w, self.h) {
            return Err((
                format!(
                    "{name}: {}x{} against the reference's {rw}x{rh}",
                    self.w, self.h
                ),
                self.clone(),
            ));
        }
        let mut diff = self.clone();
        let mut differ = 0usize;
        let bw = self.w.div_ceil(4);
        let mut blocks = vec![0usize; bw * self.h.div_ceil(4)];
        for (i, (p, q)) in self
            .rgb
            .chunks(3)
            .zip(reference.as_raw().chunks(3))
            .enumerate()
        {
            if p.iter()
                .zip(q)
                .any(|(a, b)| a.abs_diff(*b) > CHANNEL_TOLERANCE)
            {
                differ += 1;
                let (x, y) = (i % self.w, i / self.w);
                blocks[(y / 4) * bw + x / 4] += 1;
                diff.rgb[i * 3..i * 3 + 3].copy_from_slice(&[255, 0, 0]);
            }
        }
        let share = differ as f64 / (self.w * self.h) as f64;
        let worst = blocks.iter().copied().max().unwrap_or(0);
        if share > PIXEL_TOLERANCE || worst > BLOCK_TOLERANCE {
            return Err((
                format!(
                    "{name}: {differ} pixels ({:.2}%) differ from the reference, {worst} in one \
                     4x4 block; see target/newcomer/",
                    share * 100.0
                ),
                diff,
            ));
        }
        Ok(())
    }

    /// The same image as the reference `name`, within the tolerance;
    /// `STRAND_UPDATE_REFS=1` writes it instead. A mismatch leaves the
    /// shot and a diff under `target/newcomer/`.
    fn matches_ref(&self, name: &str) {
        if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
            self.save(&PathBuf::from(dir).join(format!("{name}.png")));
        }
        if std::env::var_os("STRAND_UPDATE_REFS").is_some() {
            self.save(&Img::refs().join(format!("{name}.png")));
            return;
        }
        if let Err((why, diff)) = self.compare(name) {
            let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/newcomer");
            self.save(&out.join(format!("{name}.png")));
            diff.save(&out.join(format!("{name}.diff.png")));
            panic!("{why}");
        }
    }
}

/// The cells a reload event lists, sorted: its `kept` paths and its
/// `reset` cells.
fn cells(ev: &Json) -> (Vec<String>, Vec<String>) {
    let list = |v: &Json, f: fn(&Json) -> Option<&str>| {
        let mut out: Vec<String> = v
            .as_array()
            .map(|a| a.iter().filter_map(f).map(String::from).collect())
            .unwrap_or_default();
        out.sort();
        out
    };
    (
        list(&ev["kept"], Json::as_str),
        list(&ev["reset"], |c| c["cell"].as_str()),
    )
}

#[test]
fn the_guide_builds_bar_a_on_two_outputs() {
    if !tools() {
        return;
    }
    let began = Instant::now();
    let steps = steps();
    assert_eq!(
        steps.len(),
        PREDICTED.len() + 1,
        "the guide's levels 1-3 have {} steps; PREDICTED says what each one's reload keeps \
         and resets",
        steps.len()
    );
    let mut desk = Desk::start("guide");
    // `strand new` in the fresh `$HOME`: the guide's first block.
    let new = desk.strand(&["new"]);
    assert!(
        new.status.success(),
        "strand new: {}",
        String::from_utf8_lossy(&new.stderr)
    );
    let config = desk.config();
    let bar = std::fs::read_to_string(config.join("bar.strand")).unwrap();
    assert_eq!(
        (steps[0].file.as_str(), steps[0].text.as_str()),
        ("bar.strand", bar.as_str()),
        "level 1's first block is what `strand new` writes"
    );
    desk.run(None);
    let check = |desk: &Desk, at: &str| {
        let out = desk.strand(&["check"]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success() && text.contains(": 0 errors, 0 warnings"),
            "{at}: strand check:\n{text}"
        );
    };
    let shots = |desk: &mut Desk, n: usize| {
        desk.settled_ref("HEADLESS-1", BAR_1, &format!("step{n:02}_headless1"));
        desk.settled_ref("HEADLESS-2", BAR_2, &format!("step{n:02}_headless2"));
    };
    check(&desk, &steps[0].at);
    shots(&mut desk, 1);
    for (i, (b, (file, kept, reset))) in steps[1..].iter().zip(PREDICTED).enumerate() {
        let n = i + 2;
        assert_eq!(
            &b.file, file,
            "{}: PREDICTED's step {n} is another file",
            b.at
        );
        // A live save: the block over the file, in place.
        std::fs::write(config.join(&b.file), &b.text).unwrap();
        let ev = desk.event("reload", 15);
        let committed: Vec<&str> = ev["committed"]
            .as_array()
            .map(|a| a.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default();
        assert!(
            committed
                .iter()
                .any(|p| p.ends_with(&format!("/{}", b.file))),
            "{}: the reload did not commit {}: {ev}",
            b.at,
            b.file
        );
        assert_eq!(
            ev["diagnostics"].as_array().map(Vec::len),
            Some(0),
            "{}: {ev}",
            b.at
        );
        let want = (
            kept.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            reset.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        );
        assert_eq!(
            cells(&ev),
            want,
            "{} (step {n}): kept and reset cells",
            b.at
        );
        check(&desk, &b.at);
        shots(&mut desk, n);
    }

    // Every item but the calendar is scored before any click (a click
    // may focus a workspace or mute the speaker).
    let before = desk.dump();
    // Bar (a) runs: the calendar opens from the clock on HEADLESS-1.
    let mut pointer = desk.pointer();
    // (A new virtual pointer's first buttons reach no surface.)
    pointer.click(1200, 700, LAYOUT.0, LAYOUT.1);
    std::thread::sleep(Duration::from_millis(200));
    let surfaces = desk.surface_count();
    pointer.click(1280, 26, LAYOUT.0, LAYOUT.1);
    desk.wait("the calendar popup", 10, |d| d.surface_count() > surfaces);
    pointer.motion(1800, 900, LAYOUT.0, LAYOUT.1);
    desk.settled_ref("HEADLESS-1", rect(1280 - 200, 44, 400, 320), "calendar");

    // The agent proxy's checklist passes in full on the guide's own bar.
    let dump = desk.dump();
    let score = with_calendar(score(&before), &dump);
    let failed: Vec<_> = score.iter().filter(|s| !s.ok).collect();
    if !failed.is_empty() {
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/newcomer");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("dump.json"), format!("{dump:#}")).unwrap();
    }
    assert!(
        failed.is_empty(),
        "the checklist on the guide's bar (the dump is in target/newcomer/dump.json): \
         {failed:#?}"
    );
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());

    let took = began.elapsed();
    eprintln!("the newcomer path took {took:.1?} (budget {BUDGET:?})");
    if took > BUDGET {
        panic!("{GATE_MISS}: the newcomer test took {took:.1?}, over its {BUDGET:?} budget");
    }
}

// ---------------------------------------------------------------------------
// The agent proxy's checklist (docs/guide/target-checklist.md).

/// One checklist item's result.
#[derive(Debug)]
struct Scored {
    id: &'static str,
    ok: bool,
    why: String,
}

/// The checklist's items, by the id each has in
/// `docs/guide/target-checklist.md` (`the_checklist_page_names_every_item`
/// holds the two together).
const ITEMS: &[&str] = &[
    "bar-per-output",
    "dots",
    "title",
    "clock",
    "calendar",
    "volume",
    "battery",
    "tray",
    "themed",
];

/// The dump's nodes.
fn nodes(dump: &Json) -> &[Json] {
    dump["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn prop<'a>(n: &'a Json, p: &str) -> Option<&'a str> {
    n["props"][p].as_str()
}

/// The monitor a node's bar instance is on: the first bracketed
/// qualifier of its identity's instance path (`[make | model |
/// description]`), or `None` outside a per-monitor surface.
fn monitor(n: &Json) -> Option<String> {
    let path = n["identity"]["instance_path"].as_str()?;
    let start = path.find('[')?;
    let end = path[start..].find(']')? + start;
    Some(path[start + 1..end].to_string())
}

/// Scores an inspector dump of a running config against the checklist,
/// on the acceptance mock (four workspaces on `HEADLESS-1` with the
/// second focused, two on `HEADLESS-2`; the window title "strand —
/// docs/design.md — Helix"; the clock at Mon 5 Oct 2026 09:41; the
/// battery at 87%; one tray item, `network-wireless`; the volume at 0.6
/// with `audio-volume-medium-symbolic`). It reads what the shell shows,
/// never how the config is written: files, component names and layout
/// are the author's.
fn score(dump: &Json) -> Vec<Scored> {
    let ns = nodes(dump);
    let texts = |pred: &dyn Fn(&str) -> bool| -> Vec<&Json> {
        ns.iter()
            .filter(|n| prop(n, "text").is_some_and(pred))
            .collect()
    };
    let mut out = Vec::new();
    let mut item = |id: &'static str, ok: bool, why: String| out.push(Scored { id, ok, why });

    // Surfaces: one bar root per output, along the top. The monitors are
    // named as the compositor describes them (`Unknown | Unknown |
    // Headless output 1`), in the order sway made them: `HEADLESS-1`
    // first.
    let roots: Vec<&Json> = ns.iter().filter(|n| n["parent"].is_null()).collect();
    let bars: Vec<&Json> = roots
        .iter()
        .copied()
        .filter(|n| prop(n, "edge") == Some("top"))
        .collect();
    let mut mons: Vec<String> = bars.iter().filter_map(|b| monitor(b)).collect();
    mons.sort();
    mons.dedup();
    item(
        "bar-per-output",
        bars.len() == 2 && mons.len() == 2,
        format!(
            "{} top-edge surfaces of {} roots, on {mons:?}",
            bars.len(),
            roots.len()
        ),
    );
    // Found on both monitors' bars.
    let on_both = |found: &[&Json]| -> bool {
        mons.len() == 2
            && mons
                .iter()
                .all(|m| found.iter().any(|n| monitor(n).as_ref() == Some(m)))
    };

    // Workspace dots: a row per workspace on its own monitor, keyed by
    // the workspace's id, however each dot is drawn (a box, a glyph, an
    // icon, several nodes): the distinct numeric keys on each monitor.
    let dots_on = |i: usize| -> Vec<&str> {
        let mut keys: Vec<&str> = ns
            .iter()
            .filter(|n| mons.get(i).is_some_and(|m| monitor(n).as_ref() == Some(m)))
            .filter_map(|n| n["identity"]["key"].as_str())
            .filter(|k| k.parse::<u32>().is_ok())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    };
    let (d1, d2) = (dots_on(0), dots_on(1));
    item(
        "dots",
        d1.len() == 4 && d2.len() == 2,
        format!("workspace keys {d1:?} on HEADLESS-1 (want 4), {d2:?} on HEADLESS-2 (want 2)"),
    );

    let title = texts(&|t| t.contains("docs/design.md"));
    item(
        "title",
        on_both(&title),
        format!("{} texts with the window title", title.len()),
    );

    let clock = texts(&|t| t.contains("09:41") && t.contains("05"));
    item(
        "clock",
        on_both(&clock),
        format!(
            "{:?}",
            clock
                .iter()
                .filter_map(|n| prop(n, "text"))
                .collect::<Vec<_>>()
        ),
    );

    let october = texts(&|t| t.contains("October 2026"));
    let days = texts(&|t| t.trim() == "5");
    item(
        "calendar",
        !october.is_empty() && !days.is_empty(),
        format!(
            "{} month titles, {} day cells for the 5th (the calendar is opened before scoring)",
            october.len(),
            days.len()
        ),
    );

    let volume: Vec<&Json> = ns
        .iter()
        .filter(|n| {
            ["source", "icon", "text"]
                .iter()
                .any(|p| prop(n, p).is_some_and(|v| v.contains("audio-volume")))
        })
        .collect();
    item(
        "volume",
        on_both(&volume),
        format!("{} volume icons", volume.len()),
    );

    let battery = texts(&|t| t.contains("87%"));
    item(
        "battery",
        on_both(&battery),
        format!("{} texts with the charge", battery.len()),
    );

    let tray: Vec<&Json> = ns
        .iter()
        .filter(|n| {
            ["source", "icon"]
                .iter()
                .any(|p| prop(n, p).is_some_and(|v| v.contains("network-wireless")))
        })
        .collect();
    item("tray", on_both(&tray), format!("{} tray icons", tray.len()));

    // Themed: the bars' background reads the theme's tokens.
    let themed = bars.iter().all(|b| {
        b["tokens"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["prop"] == "bg"))
    });
    item(
        "themed",
        !bars.is_empty() && themed,
        "each bar's `bg` reads a $token".to_string(),
    );
    out
}

/// Whether `dump` shows the calendar: the month's title and the 5th.
fn calendar_shown(dump: &Json) -> bool {
    score(dump).iter().any(|s| s.id == "calendar" && s.ok)
}

/// `scored` (taken before any click) with its `calendar` item replaced by
/// the one scored on `after` (taken with the calendar opened).
fn with_calendar(mut scored: Vec<Scored>, after: &Json) -> Vec<Scored> {
    if let (Some(at), Some(cal)) = (
        scored.iter().position(|s| s.id == "calendar"),
        score(after).into_iter().find(|s| s.id == "calendar"),
    ) {
        scored[at] = cal;
    }
    scored
}

/// The points a click tries on `HEADLESS-1`'s bar to open the calendar,
/// in order: the guide's clock (the top centre) first, then outwards in
/// both directions, each row in turn. The scorer knows no layout: the
/// clock may be anywhere along the bar.
fn bar_points() -> Vec<(u32, u32)> {
    const STEP: u32 = 40;
    let mut xs = vec![1280];
    for i in 1..=(1280 - 8) / STEP {
        xs.extend([1280 - i * STEP, 1280 + i * STEP]);
    }
    [26, 16, 36, 8]
        .into_iter()
        .flat_map(|y| xs.iter().map(move |&x| (x, y)))
        .collect()
}

/// How long the scan watches each click for the calendar.
struct Looks {
    /// A click that raised no surface.
    quick: Duration,
    /// A click that painted a new surface, before the surface is taken for
    /// another popup (the wait the fixed top-centre click had), and every
    /// click of the slow pass: on a loaded runner the calendar can show
    /// after the quick look, or be painted before the dump shows it.
    popup: Duration,
    /// After a click on the desktop that closes another popup.
    settle: Duration,
    /// Between two looks at the dump.
    poll: Duration,
    /// The slow pass's whole run, so a bar with no clock fails in bounded
    /// time.
    slow: Duration,
}

/// The scan's looks on the real desk.
const LOOKS: Looks = Looks {
    quick: Duration::from_millis(250),
    popup: Duration::from_secs(2),
    settle: Duration::from_millis(150),
    poll: Duration::from_millis(40),
    slow: Duration::from_secs(150),
};

/// What [`open_calendar_on`] clicks and watches: the running shell
/// ([`DeskBar`]) or, in its own test, a simulated one.
trait Bar {
    /// Whether the shell shows the calendar now.
    fn calendar_shown(&self) -> bool;
    /// How many surfaces have ever been painted (it never falls).
    fn surfaces(&self) -> usize;
    /// A click at `at` on `HEADLESS-1`.
    fn click(&mut self, at: (u32, u32));
    /// A click on the desktop, which closes any popup.
    fn away(&mut self);
    /// Fails when the shell has died.
    fn alive(&mut self) {}
}

/// The running shell on `desk`, clicked with a virtual pointer.
struct DeskBar<'a> {
    desk: &'a mut Desk,
    pointer: support::pointer::Pointer,
}

impl Bar for DeskBar<'_> {
    fn calendar_shown(&self) -> bool {
        calendar_shown(&self.desk.dump())
    }
    fn surfaces(&self) -> usize {
        self.desk.surface_count()
    }
    fn click(&mut self, (x, y): (u32, u32)) {
        self.pointer.click(x, y, LAYOUT.0, LAYOUT.1);
    }
    fn away(&mut self) {
        self.pointer.click(1200, 700, LAYOUT.0, LAYOUT.1);
    }
    fn alive(&mut self) {
        self.desk.assert_alive();
    }
}

/// Watches for the calendar after a click made at `start`, when `before`
/// surfaces had been painted: `looks.quick` while no new surface has been
/// painted, `looks.popup` once one has, or always when `slow`.
fn calendar_after_click(
    bar: &impl Bar,
    looks: &Looks,
    start: Instant,
    before: usize,
    slow: bool,
) -> bool {
    loop {
        if bar.calendar_shown() {
            return true;
        }
        let look = if slow || bar.surfaces() > before {
            looks.popup
        } else {
            looks.quick
        };
        if start.elapsed() >= look {
            return false;
        }
        std::thread::sleep(looks.poll);
    }
}

/// One pass of clicks over `points`: the point whose click showed the
/// calendar, or the points whose click may have opened a popup the pass
/// closed (a surface was painted before the click after next), for the
/// slow pass to try first. `slow` watches every click for `looks.popup`
/// and gives up after `looks.slow`.
fn scan(
    bar: &mut impl Bar,
    points: &[(u32, u32)],
    looks: &Looks,
    slow: bool,
) -> Result<(u32, u32), Vec<(u32, u32)>> {
    let began = Instant::now();
    let mut before = Vec::with_capacity(points.len() + 1);
    let mut last = None;
    for &at in points {
        // A calendar that showed after the last click's look ended is
        // found before the next click could close it.
        if let Some(at) = last
            && bar.calendar_shown()
        {
            return Ok(at);
        }
        if slow && began.elapsed() >= looks.slow {
            break;
        }
        last = Some(at);
        let surfaces = bar.surfaces();
        before.push(surfaces);
        bar.click(at);
        let start = Instant::now();
        // Twice: a surface painted just as the quick look ended gets its
        // full look too, before it is dismissed.
        if calendar_after_click(bar, looks, start, surfaces, slow)
            || (bar.surfaces() > surfaces
                && calendar_after_click(bar, looks, start, surfaces, slow))
        {
            return Ok(at);
        }
        bar.alive();
        if bar.surfaces() > surfaces {
            // Some other popup (a tray menu): a click on the desktop
            // closes it before the next try.
            bar.away();
            std::thread::sleep(looks.settle);
        }
    }
    // The last click's calendar may show late too.
    std::thread::sleep(looks.popup);
    if let Some(at) = last
        && bar.calendar_shown()
    {
        return Ok(at);
    }
    before.push(bar.surfaces());
    let n = before.len() - 1;
    Err((0..n)
        .filter(|&i| before[(i + 2).min(n)] > before[i])
        .map(|i| points[i])
        .collect())
}

/// Clicks along `points` until the shell shows the calendar, dismissing
/// any other popup a click opened; the point that opened it, or `None`.
/// A quick pass first ([`scan`]); when it finds nothing, a slow pass
/// watches every click for `looks.popup`, the points after which a
/// surface was painted first, since on a loaded runner a click may reach
/// the shell after the quick look, so that the next click closes the
/// calendar as a click-away before it is painted or seen (a second
/// toggle may never paint it at all). A calendar later than `looks.popup`
/// is still lost, as it was to the fixed top-centre click.
fn open_calendar_on(
    bar: &mut impl Bar,
    points: &[(u32, u32)],
    looks: &Looks,
) -> Option<(u32, u32)> {
    let suspects = match scan(bar, points, looks, false) {
        Ok(at) => return Some(at),
        Err(suspects) => suspects,
    };
    let mut order = suspects;
    for &at in points {
        if !order.contains(&at) {
            order.push(at);
        }
    }
    bar.away();
    std::thread::sleep(looks.settle);
    scan(bar, &order, looks, true).ok()
}

/// Opens the calendar on `desk` by clicking along `HEADLESS-1`'s bar
/// ([`bar_points`], [`open_calendar_on`]).
fn open_calendar(desk: &mut Desk) -> Option<(u32, u32)> {
    let pointer = desk.pointer();
    let mut bar = DeskBar { desk, pointer };
    // (A new virtual pointer's first buttons reach no surface.)
    bar.away();
    std::thread::sleep(Duration::from_millis(200));
    let found = open_calendar_on(&mut bar, &bar_points(), &LOOKS);
    bar.pointer.motion(1800, 900, LAYOUT.0, LAYOUT.1);
    found
}

/// A shell slow to show the calendar, as on a loaded runner: a click on
/// `clock` toggles it at once, but it is painted and seen only `paint`
/// after it opened, and any other click closes it, so a calendar closed
/// before then is never painted or seen at all.
struct LaggyBar {
    clock: (u32, u32),
    paint: Duration,
    state: std::cell::RefCell<LaggyState>,
}

#[derive(Default)]
struct LaggyState {
    /// When the open calendar opened.
    opened: Option<Instant>,
    painted: bool,
    surfaces: usize,
}

impl LaggyBar {
    fn new(clock: (u32, u32)) -> LaggyBar {
        LaggyBar {
            clock,
            paint: Duration::from_millis(80),
            state: Default::default(),
        }
    }

    /// Whether the calendar is painted, painting it when it is due.
    fn painted(&self) -> bool {
        let mut s = self.state.borrow_mut();
        if !s.painted && s.opened.is_some_and(|t| t.elapsed() >= self.paint) {
            s.painted = true;
            s.surfaces += 1;
        }
        s.painted
    }
}

impl Bar for LaggyBar {
    fn calendar_shown(&self) -> bool {
        self.painted()
    }
    fn surfaces(&self) -> usize {
        self.painted();
        self.state.borrow().surfaces
    }
    fn click(&mut self, at: (u32, u32)) {
        let s = self.state.get_mut();
        if s.opened.is_some() {
            s.opened = None;
            s.painted = false;
        } else if at == self.clock {
            s.opened = Some(Instant::now());
        }
    }
    fn away(&mut self) {
        self.click((0, 0));
    }
}

/// A calendar the shell shows only after the quick look is still found:
/// the quick pass loses it (the next click closes it before it is
/// painted), and the slow pass opens it (decisions.md m5-docs3, round 3).
#[test]
fn a_calendar_the_shell_shows_late_is_still_found() {
    let looks = Looks {
        quick: Duration::from_millis(20),
        popup: Duration::from_millis(300),
        settle: Duration::from_millis(5),
        poll: Duration::from_millis(5),
        slow: Duration::from_secs(10),
    };
    let points = [(10, 1), (20, 1), (30, 1), (40, 1), (50, 1)];
    let mut bar = LaggyBar::new((40, 1));
    assert_eq!(open_calendar_on(&mut bar, &points, &looks), Some((40, 1)));
    // And a bar with no clock fails, in bounded time.
    let mut none = LaggyBar::new((99, 1));
    assert_eq!(open_calendar_on(&mut none, &points, &looks), None);
}

/// The proxy's score of the config running on `desk`: every item but the
/// calendar on a dump taken before any click, then the calendar opened
/// wherever the clock is ([`open_calendar`]) and scored on a second dump.
fn proxy_score(desk: &mut Desk) -> Vec<Scored> {
    let before = desk.dump();
    let at = open_calendar(desk);
    std::thread::sleep(Duration::from_millis(300));
    let mut scored = with_calendar(score(&before), &desk.dump());
    if let Some(s) = scored.iter_mut().find(|s| s.id == "calendar") {
        s.why = match at {
            Some((x, y)) => format!("{}; opened by a click at {x},{y}", s.why),
            None => format!("{}; no click along the bar opened it", s.why),
        };
    }
    scored
}

/// The checklist page names every item the scorer has, in order, and no
/// other.
#[test]
fn the_checklist_page_names_every_item() {
    let page = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide/target-checklist.md"),
    )
    .unwrap();
    let ids: Vec<&str> = page
        .lines()
        .filter_map(|l| l.strip_prefix("- **`"))
        .filter_map(|l| l.split('`').next())
        .collect();
    assert_eq!(ids, ITEMS, "docs/guide/target-checklist.md's items");
}

/// The guide's bar laid out and drawn otherwise passes in full: the clock
/// in the end section, top right (so the guide's top-centre click lands on
/// empty bar), and the dots drawn as glyphs with no `radius`. The scorer
/// depends on what the shell shows, never on layout or how a dot is
/// drawn (decisions.md m5-docs3).
#[test]
fn the_scorer_passes_a_bar_laid_out_otherwise() {
    if !tools() {
        return;
    }
    let mut desk = Desk::start("other");
    let config = desk.dir.join("other");
    std::fs::create_dir_all(&config).unwrap();
    for b in steps() {
        std::fs::write(config.join(&b.file), &b.text).unwrap();
    }
    let bar = std::fs::read_to_string(config.join("bar.strand")).unwrap();
    let moved = bar.replace("    center { Clock }\n", "").replace(
        "    end { gap: $space.3\n",
        "    end { gap: $space.3\n      Clock\n",
    );
    assert!(
        moved != bar && !moved.contains("center {") && moved.contains("      Clock\n"),
        "the guide's last bar.strand changed: move its clock again\n{bar}"
    );
    std::fs::write(config.join("bar.strand"), moved).unwrap();
    std::fs::write(
        config.join("dot.strand"),
        "component Dot(ws: Workspace) {\n  text \"●\" { color: $fg.muted\n    \
         when ws.focused { color: $accent }\n    on click { ws.focus() }\n  }\n}\n",
    )
    .unwrap();
    desk.run(Some(&config));
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
    let score = proxy_score(&mut desk);
    for s in &score {
        eprintln!("{} {}: {}", if s.ok { "PASS" } else { "FAIL" }, s.id, s.why);
    }
    let failed: Vec<_> = score.iter().filter(|s| !s.ok).collect();
    assert!(failed.is_empty(), "{failed:#?}");
    let calendar = score.iter().find(|s| s.id == "calendar").unwrap();
    assert!(
        !calendar.why.contains("at 1280,26"),
        "the clock is not at the top centre: {}",
        calendar.why
    );
}

/// The marker the newcomer test's budget miss starts with is the one
/// `scripts/container/gate-misses.sh` reads, and that file names this
/// test among the markers' owners (its miss fails the strict `test` step
/// all the same: decisions.md m5-docs3).
#[test]
fn the_gate_miss_marker_is_the_one_the_container_suite_reads() {
    let check = include_str!("../../../../scripts/container/gate-misses.sh");
    assert!(check.contains(&format!("index(msg, \"{GATE_MISS}\") == 1")));
    assert!(check.contains("strand/tests/m5/newcomer.rs"));
}

/// The agent proxy's score of another config (`STRAND_PROXY_CONFIG`, the
/// directory a proxy session wrote), on the same desk and mock: started
/// and scored by [`proxy_score`], which finds the clock by clicking along
/// the bar. It prints every item and fails when one fails.
/// Advisory: run by `scripts/agent-proxy/score.sh`, never in CI.
#[test]
#[ignore = "the agent proxy's scorer: STRAND_PROXY_CONFIG=<dir>"]
fn agent_proxy_score() {
    let Some(config) = std::env::var_os("STRAND_PROXY_CONFIG") else {
        panic!("set STRAND_PROXY_CONFIG to the config directory to score");
    };
    if !tools() {
        return;
    }
    let mut desk = Desk::start("proxy");
    desk.run(Some(Path::new(&config)));
    std::thread::sleep(Duration::from_secs(2));
    let score = proxy_score(&mut desk);
    for s in &score {
        println!("{} {}: {}", if s.ok { "PASS" } else { "FAIL" }, s.id, s.why);
    }
    let passed = score.iter().filter(|s| s.ok).count();
    println!("score: {passed}/{}", score.len());
    assert_eq!(
        passed,
        score.len(),
        "the proxy's bar misses checklist items"
    );
}

//! The M2 exit gate "the four example shells run unchanged": design.md's
//! bar with its calendar, launcher, notification stack and OSD, with its
//! `theme.strand`, copied byte for byte from the fixtures (which
//! `strand-compiler/tests/fixtures.rs` holds to design.md's code blocks)
//! and run by `strand run` on a headless sway with two outputs
//! (`HEADLESS-1` 2560x1440 at scale 1, `HEADLESS-2` 1920x1080 at 1.25),
//! against the deterministic mock services (`STRAND_MOCK=acceptance`:
//! the clock frozen at Mon 5 Oct 2026 09:41 UTC, no notifications until
//! the test sends them). Each test drives one shell as a user and the
//! services would: clicks, the wheel, keys from a virtual keyboard,
//! `strand set`, and service changes through the mock's IPC command.
//!
//! Settled frames are compared with the references in
//! `tests/refs/acceptance/` within a tolerance ([`Img::matches_ref`]);
//! `STRAND_UPDATE_REFS=1` rewrites them (read every one before committing
//! it), `STRAND_SHOTS=<dir>` keeps every screenshot. Skipped, loudly,
//! without sway or grim (CI sets `STRAND_REQUIRE_SWAY`).

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;
use support::{keyboard, pointer};

struct Proc(Child);

/// The cursor theme and size of every process on the desk (sway and
/// strand), pinned: the hover references show the pointer.
const CURSOR_THEME: &str = "Adwaita";
const CURSOR_SIZE: &str = "24";

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The five files of design.md, unchanged.
const SHELLS: [(&str, &str); 5] = [
    (
        "theme.strand",
        include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
    ),
    (
        "bar.strand",
        include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
    ),
    (
        "launcher.strand",
        include_str!("../../strand-compiler/tests/fixtures/launcher.strand"),
    ),
    (
        "toasts.strand",
        include_str!("../../strand-compiler/tests/fixtures/toasts.strand"),
    ),
    (
        "osd.strand",
        include_str!("../../strand-compiler/tests/fixtures/osd.strand"),
    ),
];

/// The layout: `HEADLESS-1` at 0,0 (2560x1440 logical), `HEADLESS-2`
/// right of it (1920x1080 at 1.25: 1536x864 logical).
const LAYOUT: (u32, u32) = (2560 + 1536, 1440);

/// A headless sway with two outputs and `strand run` on the five files.
struct Desk {
    strand: Option<Proc>,
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
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Desk {
    fn start(tag: &'static str) -> Option<Desk> {
        Self::start_with(tag, &SHELLS, |b| b.contains("buffer=2570x62 "))
    }

    /// [`Desk::start`] on `files` instead of the five, ready once a
    /// surface `bar` says it is the bar on `HEADLESS-1` and one is drawn
    /// at 1.25.
    fn start_with(
        tag: &'static str,
        files: &[(&str, &str)],
        bar: fn(&str) -> bool,
    ) -> Option<Desk> {
        for tool in ["sway", "swaymsg", "grim"] {
            if Command::new(tool).arg("--version").output().is_err() {
                assert!(
                    std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                    "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
                );
                eprintln!(
                    "\n*** SKIPPED: {tool} is not installed; the M2 acceptance tests did not run ***\n"
                );
                return None;
            }
        }
        // Short: the IPC socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-acc-{tag}-{}", std::process::id()));
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
        // The compositor dies with the thread that started it, so a test
        // binary killed before `Drop` never leaks it. SAFETY: the hook runs
        // between fork and exec and makes one async-signal-safe syscall.
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
            // The pointer is in the hover shots: one cursor theme and size
            // whatever the machine's default theme resolves to.
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
            _sway: Proc(child),
            dir: dir.clone(),
            display: String::new(),
            ipc: PathBuf::new(),
            log: dir.join("strand.log"),
            tag,
            shots: std::cell::Cell::new(0),
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
                desk.display = d.clone();
                desk.ipc = dir.join(i);
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
        // The files, byte for byte.
        let home = dir.join("home");
        let config = home.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        for &(name, text) in files {
            std::fs::write(config.join(name), text).unwrap();
            assert_eq!(std::fs::read(config.join(name)).unwrap(), text.as_bytes());
        }
        let child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .envs(desk.env())
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("STRAND_MOCK", "acceptance")
            .env("STRAND_LOG", "damage")
            // No portal: the theme's `auto` look is the light scheme.
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&desk.log).unwrap())
            .spawn()
            .unwrap();
        desk.strand = Some(Proc(child));
        // A bar painted on each output.
        desk.wait("a bar on each output", 30, move |d| {
            let s = d.surfaces();
            s.iter().any(|b| bar(b)) && s.iter().any(|b| b.contains("scale=1.25"))
        });
        Some(desk)
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
            ("XCURSOR_THEME", PathBuf::from(CURSOR_THEME)),
            ("XCURSOR_SIZE", PathBuf::from(CURSOR_SIZE)),
        ]
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

    /// Each painted frame's surface and buffer, as `STRAND_LOG=damage`
    /// prints them.
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
                    .collect::<Vec<_>>()
                    .join(" ")
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

    /// Wait up to `secs` for `done`, failing with the log if strand exits.
    fn wait(&mut self, what: &str, secs: u64, done: impl Fn(&Desk) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(self) {
            let exited = self
                .strand
                .as_mut()
                .is_some_and(|p| p.0.try_wait().unwrap().is_some());
            assert!(!exited, "strand exited: {}", self.log_text());
            assert!(
                Instant::now() < deadline,
                "{}: {what}\n{}",
                self.tag,
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// Fails with the log when strand has exited (a crash is reported as
    /// one, not as whatever the desktop shows without it).
    fn assert_alive(&self) {
        let Some(p) = &self.strand else {
            return;
        };
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", p.0.id())).unwrap_or_default();
        // The state follows the parenthesised command name.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("X");
        assert!(
            !matches!(state, "Z" | "X" | "x"),
            "{}: strand exited: {}",
            self.tag,
            self.log_text()
        );
    }

    /// A screenshot of one output, in its buffer pixels.
    fn shot(&self, output: &str) -> Img {
        self.assert_alive();
        let path = self.dir.join(format!("{output}.ppm"));
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim {output}");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    /// The region `r` of `output` (grim reads only that region of
    /// `HEADLESS-1`, whose scale is 1).
    fn region(&self, output: &str, r: Rect) -> Img {
        self.assert_alive();
        if output != "HEADLESS-1" {
            return self.shot(output).crop(r);
        }
        let path = self.dir.join("region.ppm");
        let geometry = format!("{},{} {}x{}", r.x, r.y, r.w, r.h);
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-g", &geometry])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim {geometry}");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    /// Painted frames so far (`STRAND_LOG=damage` lines).
    fn frames(&self) -> usize {
        self.log_text()
            .lines()
            .filter(|l| l.starts_with("strand: damage"))
            .count()
    }

    /// The region `r` of `output` once neither it nor anything strand
    /// paints has changed for 500 ms (every spring at rest, and a
    /// content-sized surface's deferred shrink done). It cannot tell a
    /// round trip that has not started yet from one that is over: after
    /// an action, wait for the state it leads to first ([`Desk::wait`],
    /// or [`Desk::settled_ref`]).
    fn settled(&self, output: &str, r: Rect) -> Img {
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
                // Every settled frame under `STRAND_SHOTS`, numbered.
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

    /// The region `r` of `output` once it has reached the committed
    /// reference `name` (up to 15 s: a slow logic → render → configure
    /// round trip is waited for, not read as settled), settled, and
    /// matched against it ([`Img::matches_ref`]).
    fn settled_ref(&self, output: &str, r: Rect, name: &str) -> Img {
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

    /// The region `r` of `output` as soon as it matches the committed
    /// reference `name` (up to 15 s), for a surface that does not stay
    /// long enough to settle under load: the OSD hides 1.2 s after the
    /// change that showed it, which a slow debug build can spend mostly
    /// on its way in. `STRAND_UPDATE_REFS=1` takes the settled region.
    fn reaches_ref(&self, output: &str, r: Rect, name: &str) -> Img {
        if std::env::var_os("STRAND_UPDATE_REFS").is_some() {
            let img = self.settled(output, r);
            img.matches_ref(name);
            return img;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let img = self.region(output, r);
            if img.compare(name).is_ok() || Instant::now() >= deadline {
                img.matches_ref(name);
                return img;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// The mock services' IPC command (`{"cmd": "mock", …}`).
    fn mock(&self, req: serde_json::Value) {
        let mut req = req;
        req["v"] = 1.into();
        req["cmd"] = "mock".into();
        let sock = self.dir.join(format!("strand-{}.sock", self.display));
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        let ans: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ans["ok"], true, "mock {req}: {line}");
    }

    /// `strand <args>` against this shell (`strand set …`).
    fn cli(&self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "strand {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn pointer(&self) -> pointer::Pointer {
        pointer::Pointer::new(&self.dir.join(&self.display))
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

/// A pixel differs from the reference when a channel is more than this
/// apart (antialiasing and dither noise stay under it).
const CHANNEL_TOLERANCE: u8 = 24;

/// At most this share of a reference's pixels may differ.
const PIXEL_TOLERANCE: f64 = 0.005;

/// No 4x4 block may hold more than this many differing pixels: a missing
/// or changed glyph (the clock's digits, the OSD's "30%", a toast's
/// summary) fails however small it is against the whole region, while
/// antialiasing noise, scattered, passes.
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

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
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

    /// The committed reference `name`.
    fn reference(name: &str) -> image::RgbImage {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/refs/acceptance")
            .join(format!("{name}.png"));
        image::open(&path)
            .unwrap_or_else(|e| panic!("reference {}: {e}", path.display()))
            .to_rgb8()
    }

    /// Compares with the reference `name`: the differing pixels (further
    /// apart than 24 in a channel, marked red in the returned diff) may
    /// be at most 0.5% of the image, and at most [`BLOCK_TOLERANCE`] in
    /// any 4x4 block.
    fn compare(&self, name: &str) -> Result<(), (String, Img)> {
        let reference = Img::reference(name);
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
        let mut blocks = vec![0usize; self.w.div_ceil(4) * self.h.div_ceil(4)];
        let bw = self.w.div_ceil(4);
        for (i, (p, q)) in self
            .rgb
            .chunks(3)
            .zip(reference.as_raw().chunks(3))
            .enumerate()
        {
            let far = p
                .iter()
                .zip(q)
                .any(|(a, b)| a.abs_diff(*b) > CHANNEL_TOLERANCE);
            if far {
                differ += 1;
                let (x, y) = (i % self.w, i / self.w);
                blocks[(y / 4) * bw + x / 4] += 1;
                diff.rgb[i * 3..i * 3 + 3].copy_from_slice(&[255, 0, 0]);
            }
        }
        let share = differ as f64 / (self.w * self.h) as f64;
        let (worst, at) = blocks
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, i))
            .max()
            .unwrap_or((0, 0));
        if share > PIXEL_TOLERANCE || worst > BLOCK_TOLERANCE {
            return Err((
                format!(
                    "{name}: {differ} pixels ({:.2}%) differ from the reference (tolerance {:.1}%), \
                     {worst} of the 4x4 block at {},{} (tolerance {BLOCK_TOLERANCE}); \
                     see target/acceptance/",
                    share * 100.0,
                    PIXEL_TOLERANCE * 100.0,
                    (at % bw) * 4,
                    (at / bw) * 4,
                ),
                diff,
            ));
        }
        Ok(())
    }

    /// The same image as the committed reference `name`, within the
    /// tolerance ([`Img::compare`]). `STRAND_UPDATE_REFS=1` writes the
    /// reference instead. A mismatch leaves the shot and a diff
    /// (differing pixels in red) under `target/acceptance/`.
    fn matches_ref(&self, name: &str) {
        if let Some(dir) = std::env::var_os("STRAND_SHOTS") {
            self.save(&PathBuf::from(dir).join(format!("{name}.png")));
        }
        if std::env::var_os("STRAND_UPDATE_REFS").is_some() {
            let refs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/refs/acceptance");
            self.save(&refs.join(format!("{name}.png")));
            return;
        }
        if let Err((why, diff)) = self.compare(name) {
            let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/acceptance");
            self.save(&out.join(format!("{name}.png")));
            diff.save(&out.join(format!("{name}.diff.png")));
            panic!("{why}");
        }
    }
}

fn sum(p: [u8; 3]) -> u32 {
    p.iter().map(|c| u32::from(*c)).sum()
}

/// `$accent` on the light scheme: a saturated blue.
fn blue(p: [u8; 3]) -> bool {
    i32::from(p[2]) - i32::from(p[0]) > 60
}

/// WCAG relative luminance of an sRGB pixel.
fn luminance(p: [u8; 3]) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(p[0]) + 0.7152 * lin(p[1]) + 0.0722 * lin(p[2])
}

fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The columns of row band `ys` in `xs` holding ink that differs from
/// `bg` by more than 120 (summed channels).
fn ink_cols(
    img: &Img,
    xs: std::ops::Range<usize>,
    ys: std::ops::Range<usize>,
    bg: [u8; 3],
) -> Vec<usize> {
    xs.filter(|&x| {
        ys.clone()
            .any(|y| sum(img.px(x, y)).abs_diff(sum(bg)) > 120)
    })
    .collect()
}

/// Runs of consecutive values.
fn runs(cols: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &c in cols {
        match out.last_mut() {
            Some((_, e)) if *e + 1 == c => *e = c,
            _ => out.push((c, c)),
        }
    }
    out
}

/// The bar of `HEADLESS-1` (box 8..2552 × 8..44, its shadow below).
const BAR_1: Rect = rect(0, 0, 2560, 64);
/// The bar of `HEADLESS-2`, in its 1.25 buffer pixels.
const BAR_2: Rect = rect(0, 0, 1920, 80);

/// design.md (a): the bar on both outputs, with the clock truly centred,
/// the workspace dots (keyed, no max: four on the first output, two on
/// the second), a click on a dot focusing it (the focused pill moves),
/// the clock's click opening the calendar popup with the month's grid
/// and today in `$accent`, its ‹ button paging to September, Escape
/// closing it; then the theme's looks: `strand set theme.look dark` and
/// `mocha` spring the bar's colours with its text above 3:1 throughout.
#[test]
fn the_bar_and_its_calendar_on_two_outputs() {
    let Some(mut desk) = Desk::start("bar") else {
        return;
    };
    let one = desk.settled_ref("HEADLESS-1", BAR_1, "bar_headless1");
    let two = desk.settled_ref("HEADLESS-2", BAR_2, "bar_headless2");
    // The clock: the ink run nearest each output's centre, centred.
    for (name, img, bar) in [("HEADLESS-1", &one, 14..38), ("HEADLESS-2", &two, 18..48)] {
        let bg = img.px(img.w / 2, bar.start - 3);
        let cols = ink_cols(img, 0..img.w, bar.clone(), bg);
        let clock = runs(&cols)
            .into_iter()
            .filter(|(s, e)| s.abs_diff(img.w / 2) < 120 || e.abs_diff(img.w / 2) < 120)
            .collect::<Vec<_>>();
        let (l, r) = (clock[0].0, clock.last().unwrap().1);
        let centre = (l + r) as f64 / 2.0;
        assert!(
            (centre - img.w as f64 / 2.0).abs() <= 3.0,
            "{name}: clock {l}..{r} not centred"
        );
    }
    // The dots: four on HEADLESS-1 (the second the focused pill), two
    // on HEADLESS-2.
    let dots = |img: &Img, y: usize, xs: std::ops::Range<usize>| {
        // The bar between the dots and the title.
        let bg = img.px(xs.end - 1, y);
        runs(&ink_cols(img, xs, y..y + 1, bg))
    };
    let d1 = dots(&one, 26, 14..88);
    assert_eq!(d1.len(), 4, "dots on HEADLESS-1: {d1:?}");
    assert!(
        d1[1].1 - d1[1].0 > 20 && blue(one.px((d1[1].0 + d1[1].1) / 2, 26)),
        "the focused workspace is not the accent pill: {d1:?}"
    );
    let d2 = dots(&two, 32, 18..60);
    assert_eq!(d2.len(), 2, "dots on HEADLESS-2: {d2:?}");

    // A click on the third dot focuses it (`ws.focus()`): the pill
    // moves there, its width springing (`width: 24` when focused).
    let mut pointer = desk.pointer();
    // (A new virtual pointer's first buttons reach no surface.)
    pointer.click(1200, 700, LAYOUT.0, LAYOUT.1);
    std::thread::sleep(Duration::from_millis(200));
    let third = (d1[2].0 + d1[2].1) / 2;
    pointer.click(third as u32, 26, LAYOUT.0, LAYOUT.1);
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the pill on the third dot", 10, |d| {
        let d = dots(&d.region("HEADLESS-1", rect(0, 0, 200, 64)), 26, 14..88);
        d.len() == 4 && d[2].1 - d[2].0 > 20 && d[1].1 - d[1].0 < 12
    });
    let after = desk.settled_ref("HEADLESS-1", rect(0, 0, 200, 64), "bar_third_workspace");
    let d = dots(&after, 26, 14..88);
    assert_eq!(d.len(), 4, "{d:?}");
    assert!(
        d[2].1 - d[2].0 > 20 && blue(after.px((d[2].0 + d[2].1) / 2, 26)),
        "the click did not focus the third workspace: {d:?}"
    );
    assert!(d[1].1 - d[1].0 < 12, "the second is still wide: {d:?}");

    // The clock's click opens the calendar below it, centred on it.
    let surfaces = desk.surface_count();
    pointer.click(1280, 26, LAYOUT.0, LAYOUT.1);
    desk.wait("the calendar popup", 10, |d| d.surface_count() > surfaces);
    pointer.motion(1800, 900, LAYOUT.0, LAYOUT.1);
    let card = rect(1280 - 200, 44, 400, 320);
    let cal = desk.settled_ref("HEADLESS-1", card, "calendar_october");
    let light: Vec<usize> = (0..cal.w).filter(|&x| sum(cal.px(x, 40)) > 600).collect();
    let (l, r) = (light[0], *light.last().unwrap());
    assert!(
        ((l + r) as f64 / 2.0 - 200.0).abs() <= 3.0,
        "calendar {l}..{r} not centred under the clock"
    );
    // Today (the 5th) in `$accent`: one blue disc.
    let accent = (0..cal.h)
        .flat_map(|y| (0..cal.w).map(move |x| (x, y)))
        .filter(|&(x, y)| blue(cal.px(x, y)))
        .count();
    assert!(accent > 200, "no accent day: {accent} px");
    // ‹ pages back a month (the button at the card's left, on its title
    // row): the grid changes and still shows the 5th of October among
    // its trailing days, in the accent.
    let top = (0..cal.h).find(|&y| sum(cal.px(200, y)) > 600).unwrap();
    pointer.click(
        (card.x + l + 26) as u32,
        (card.y + top + 26) as u32,
        LAYOUT.0,
        LAYOUT.1,
    );
    pointer.motion(1800, 900, LAYOUT.0, LAYOUT.1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while desk.region("HEADLESS-1", card) == cal {
        assert!(Instant::now() < deadline, "‹ did not page the month");
        std::thread::sleep(Duration::from_millis(50));
    }
    desk.settled_ref("HEADLESS-1", card, "calendar_september");
    // Escape closes it (the popup has the keyboard grab).
    let mut keys = keyboard::Keyboard::new(&desk.dir.join(&desk.display), &desk.dir);
    std::thread::sleep(Duration::from_millis(200));
    keys.press("Escape");
    let deadline = Instant::now() + Duration::from_secs(10);
    while sum(desk.shot("HEADLESS-1").px(1280, 44 + top + 60)) > 450 {
        assert!(
            Instant::now() < deadline,
            "Escape did not close the calendar"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(keys);

    // Volume's `if hover { slider … }`: hovering the row reveals the
    // slider (90 wide, its `$accent` fill at the mock's 0.6), springing
    // in from `width: 0`; the pointer leaving hides it again.
    let right = rect(1960, 0, 600, 64);
    let accent_px = |img: &Img| {
        (14..38)
            .flat_map(|y| (0..img.w).map(move |x| (x, y)))
            .filter(|&(x, y)| blue(img.px(x, y)))
            .count()
    };
    let before = desk.settled("HEADLESS-1", right);
    assert!(accent_px(&before) < 20, "a slider before the hover");
    let bg = before.px(10, 12);
    let speaker = runs(&ink_cols(&before, 240..580, 14..38, bg))[0];
    pointer.motion(
        (right.x + (speaker.0 + speaker.1) / 2) as u32,
        26,
        LAYOUT.0,
        LAYOUT.1,
    );
    desk.wait("the volume slider on hover", 10, |d| {
        accent_px(&d.region("HEADLESS-1", right)) > 100
    });
    let hover = desk.settled_ref("HEADLESS-1", right, "bar_volume_hover");
    let fill: Vec<usize> = (0..hover.w)
        .filter(|&x| (14..38).any(|y| blue(hover.px(x, y))))
        .collect();
    let width = fill.last().unwrap() - fill[0] + 1;
    assert!(
        (40..=70).contains(&width),
        "the slider's fill is {width} px, 0.6 of 90"
    );
    pointer.motion(1800, 900, LAYOUT.0, LAYOUT.1);
    desk.wait("the volume slider to hide", 10, |d| {
        accent_px(&d.region("HEADLESS-1", right)) < 20
    });

    // The theme's looks: dark, then Catppuccin mocha. The clock's text
    // keeps 3:1 against the bar in every frame grim catches.
    let clock_contrast = |img: &Img| {
        let bg = img.px(1280 - 100, 12);
        let ink = (14..38)
            .flat_map(|y| (1200..1360).map(move |x| (x, y)))
            .map(|(x, y)| img.px(x, y))
            .max_by(|a, b| contrast(*a, bg).total_cmp(&contrast(*b, bg)))
            .unwrap();
        contrast(ink, bg)
    };
    for look in ["dark", "mocha"] {
        desk.cli(&["set", "theme.look", look]);
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_millis(600) {
            seen.push(clock_contrast(&desk.region("HEADLESS-1", BAR_1)));
        }
        let bar = desk.settled_ref("HEADLESS-1", BAR_1, &format!("bar_{look}"));
        seen.push(clock_contrast(&bar));
        eprintln!("clock contrast through the swap to {look}: {seen:.2?}");
        // Light → dark is the swap no spring keeps readable: design.md's
        // fallback crossfades from a snapshot, and a blended frame is
        // exempt from the guard (decisions.md wave3-theme (t2)). Dark →
        // mocha springs, and every frame holds 3:1.
        let gated = if look == "dark" {
            &seen[seen.len() - 1..]
        } else {
            &seen[..]
        };
        assert!(
            gated.iter().all(|c| *c >= 3.0),
            "the clock fell below 3:1 swapping to {look}: {seen:.2?}"
        );
        // The bar's background went dark, and the tray's symbolic icon
        // (`image item.icon`) is drawn in the light foreground on it.
        assert!(sum(bar.px(1280 - 100, 12)) < 200, "{look}: bar not dark");
        let tray = (14..38)
            .flat_map(|y| (2515..2545).map(move |x| (x, y)))
            .filter(|&(x, y)| sum(bar.px(x, y)) > 450)
            .count();
        assert!(tray > 30, "{look}: the tray icon is not light: {tray} px");
        desk.settled_ref("HEADLESS-2", BAR_2, &format!("bar_{look}_headless2"));
    }
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
}

/// design.md (b): the launcher, unchanged, opened as its comment says
/// a key binding would (`strand set launcher.open true`; M5's `strand
/// toggle` is the same write): centred on the focused output in the
/// usable area, its input focused (caret) and the first hit selected;
/// typing filters with the matched letters in `$accent`, Down moves the
/// selection, Return launches and closes it (`open = false`); opened
/// again its `on show` clears the query, and Escape closes it.
#[test]
fn the_launcher_filters_selects_and_closes() {
    let Some(mut desk) = Desk::start("launcher") else {
        return;
    };
    // A keyboard on the seat before it opens (`keyboard: exclusive`).
    let mut keys = keyboard::Keyboard::new(&desk.dir.join(&desk.display), &desk.dir);
    let surfaces = desk.surface_count();
    desk.cli(&["set", "launcher.open", "true"]);
    desk.wait("the launcher", 10, |d| d.surface_count() > surfaces);
    // The box: 600 wide, centred in the usable area below the bar.
    let area = rect(980 - 40, 200, 680, 1040);
    let col = 340 + 250;
    desk.wait("the launcher's box", 10, |d| {
        let s = d.region("HEADLESS-1", area);
        (0..s.h).any(|y| sum(s.px(col, y)) > 600)
    });
    let opened = desk.settled_ref("HEADLESS-1", area, "launcher_open");
    let ws = desk.msg(&["-t", "get_workspaces"]).unwrap();
    let ws: serde_json::Value = serde_json::from_str(&ws).unwrap();
    let ws = ws
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["output"] == "HEADLESS-1")
        .unwrap();
    let (uy, uh) = (
        ws["rect"]["y"].as_f64().unwrap(),
        ws["rect"]["height"].as_f64().unwrap(),
    );
    let rows: Vec<usize> = (0..opened.h)
        .filter(|&y| sum(opened.px(col, y)) > 600)
        .collect();
    let (top, bottom) = (rows[0], *rows.last().unwrap());
    let centre = (area.y + top + area.y + bottom + 1) as f64 / 2.0;
    assert!(
        (centre - (uy + uh / 2.0)).abs() <= 2.0,
        "launcher {top}..{bottom} not centred in {uy} + {uh}"
    );
    let cols: Vec<usize> = (0..opened.w)
        .filter(|&x| sum(opened.px(x, (top + bottom) / 2)) > 600)
        .collect();
    let left = cols[0];
    assert_eq!(cols.last().unwrap() - left + 1, 600, "launcher width");
    assert_eq!(area.x + left, 980, "launcher not centred across");
    // Three rows (the mock's apps), the first selected: its
    // `$accent.container` is bluer than the launcher's own surface.
    let icons = |img: &Img, top: usize, bottom: usize| {
        let inked: Vec<usize> = (top + 48..bottom)
            .filter(|&y| (left + 20..left + 52).any(|x| sum(img.px(x, y)) < 200))
            .collect();
        runs(&inked)
    };
    let rows3 = icons(&opened, top, bottom);
    assert_eq!(rows3.len(), 3, "three apps: {rows3:?}");
    let tint = |p: [u8; 3]| i32::from(p[2]) - i32::from(p[0]);
    let plain = opened.px(col, top + 4);
    assert!(
        tint(opened.px(col, rows3[0].0 + 4)) >= tint(plain) + 10,
        "the first hit is not selected"
    );
    // The caret (`$accent`) in the input.
    let caret = (top + 8..top + 40).any(|y| (left + 4..left + 40).any(|x| blue(opened.px(x, y))));
    assert!(caret, "no caret in the focused input");

    // "fi": Firefox and Files, their "Fi" marked in the accent; Down
    // selects Files.
    keys.type_text("fi");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = desk.region("HEADLESS-1", area);
        let rows: Vec<usize> = (0..s.h).filter(|&y| sum(s.px(col, y)) > 600).collect();
        if let (Some(&t), Some(&b)) = (rows.first(), rows.last())
            && icons(&s, t, b).len() == 2
        {
            break;
        }
        assert!(Instant::now() < deadline, "typing did not filter the list");
        std::thread::sleep(Duration::from_millis(50));
    }
    keys.press("Down");
    // Two rows, the second selected.
    desk.wait("Down to select the second hit", 10, |d| {
        let s = d.region("HEADLESS-1", area);
        let rows: Vec<usize> = (0..s.h).filter(|&y| sum(s.px(col, y)) > 600).collect();
        let (Some(&t), Some(&b)) = (rows.first(), rows.last()) else {
            return false;
        };
        let two = icons(&s, t, b);
        two.len() == 2 && tint(s.px(col, two[1].0 + 4)) >= tint(s.px(col, two[0].0 + 4)) + 10
    });
    let typed = desk.settled_ref("HEADLESS-1", area, "launcher_typed");
    let rows: Vec<usize> = (0..typed.h)
        .filter(|&y| sum(typed.px(col, y)) > 600)
        .collect();
    let (t, b) = (rows[0], *rows.last().unwrap());
    let two = icons(&typed, t, b);
    assert_eq!(two.len(), 2, "{two:?}");
    assert!(
        tint(typed.px(col, two[1].0 + 4)) >= tint(typed.px(col, two[0].0 + 4)) + 10,
        "Down did not select the second hit"
    );
    let marked = (t + 44..b)
        .flat_map(|y| (left + 56..left + 120).map(move |x| (x, y)))
        .filter(|&(x, y)| blue(typed.px(x, y)))
        .count();
    assert!(marked > 20, "no marked letters: {marked} px");

    // Return launches the selected hit and closes the launcher.
    keys.press("Return");
    let deadline = Instant::now() + Duration::from_secs(10);
    while sum(desk
        .shot("HEADLESS-1")
        .px(area.x + col, area.y + (t + b) / 2))
        > 450
    {
        assert!(
            Instant::now() < deadline,
            "Return did not close the launcher"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Opened again: `on show { query = "" }` brings back all three.
    desk.cli(&["set", "launcher.open", "true"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = desk.region("HEADLESS-1", area);
        let rows: Vec<usize> = (0..s.h).filter(|&y| sum(s.px(col, y)) > 600).collect();
        if let (Some(&t), Some(&b)) = (rows.first(), rows.last())
            && icons(&s, t, b).len() == 3
        {
            break;
        }
        assert!(Instant::now() < deadline, "reopened without its three rows");
        std::thread::sleep(Duration::from_millis(50));
    }
    desk.settled_ref("HEADLESS-1", area, "launcher_open");
    // Escape closes it.
    keys.press("Escape");
    let deadline = Instant::now() + Duration::from_secs(10);
    while sum(desk
        .shot("HEADLESS-1")
        .px(area.x + col, area.y + (top + bottom) / 2))
        > 450
    {
        assert!(
            Instant::now() < deadline,
            "Escape did not close the launcher"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
}

/// The toasts' column on `HEADLESS-1`: top-right, margin 12, 380 wide
/// (with room for the shadows).
const TOASTS: Rect = rect(2560 - 12 - 380 - 40, 0, 380 + 52, 560);

/// The x of the first toast's left edge in a shot of [`TOASTS`], on its
/// summary's row band `ys` (`None` when nothing is there).
fn toast_left(img: &Img, ys: std::ops::Range<usize>) -> Option<usize> {
    (0..img.w).find(|&x| ys.clone().filter(|&y| sum(img.px(x, y)) > 600).count() > ys.len() / 2)
}

/// design.md (c): no panel until a notification arrives; arrivals slide
/// in from the right (`enter { x: 420; opacity: 0 }`) and stack, a
/// critical one bordered in `$error` with its actions as buttons; the
/// close icon dismisses one and the toasts below slide up into its
/// place; a timeout expires one by itself, a click activates one (it
/// leaves), a right click dismisses one; the panel closes when the last
/// is gone.
#[test]
fn toasts_arrive_stack_slide_and_leave() {
    let Some(mut desk) = Desk::start("toasts") else {
        return;
    };
    // Nothing at boot.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(desk.surface_count(), 2, "{:?}", desk.surfaces());
    let note = |id: i64, app: &str, icon: &str, summary: &str, body: &str| {
        serde_json::json!({"notify": {"id": id, "app": app, "icon": icon,
            "summary": summary, "body": body, "timeout_ms": 600_000}})
    };
    desk.mock(note(
        1,
        "Mail",
        "mail-unread",
        "New message",
        "<b>Ada</b>: the <i>layout</i> pass is in — see <a href=\"https://x\">the PR</a>",
    ));
    // It slides in: its left edge moves leftwards over several frames.
    let mut lefts = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(900) {
        if let Some(x) = toast_left(&desk.region("HEADLESS-1", TOASTS), 62..110) {
            lefts.push(x);
        }
    }
    let one = desk.settled_ref("HEADLESS-1", TOASTS, "toasts_one");
    let rest = toast_left(&one, 62..110).expect("no toast");
    eprintln!("first toast's left edge while entering: {lefts:?}, at rest {rest}");
    assert_eq!(rest, 40, "the toast is not at margin 12");
    // Frames of the slide itself are asserted on an optimised build
    // (CI runs these tests `--release`): a debug build paints too slowly
    // for grim to catch them reliably.
    let release = !cfg!(debug_assertions);
    assert!(
        !release || lefts.iter().filter(|&&x| x > rest + 2).count() >= 2,
        "no frames of the slide caught: {lefts:?}"
    );
    // (`$motion.spatial` is `spring(700, 0.9)`: it may overshoot by a
    // pixel or two and come back.)
    assert!(
        lefts.windows(2).all(|p| p[1] <= p[0] + 2),
        "the slide went backwards: {lefts:?}"
    );

    let mut critical = note(
        2,
        "Battery",
        "battery-caution",
        "Battery low",
        "Plug in soon",
    );
    critical["notify"]["urgency"] = "critical".into();
    critical["notify"]["actions"] = serde_json::json!(["Power settings", "Dismiss"]);
    desk.mock(critical);
    desk.mock(note(
        3,
        "Chat",
        "chat-message-new",
        "Grace",
        "Lunch at noon? I booked the place by the river.",
    ));
    // The toasts' rows on a column of their left padding, below the
    // bar and its shadow.
    let lit = |img: &Img, x: usize| -> Vec<usize> {
        (50..img.h).filter(|&y| sum(img.px(x, y)) > 600).collect()
    };
    let col = 40 + 6;
    desk.wait("three toasts", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 3
    });
    let three = desk.settled_ref("HEADLESS-1", TOASTS, "toasts_three");
    // The critical one's border is `$error` (red).
    let red = (0..three.h)
        .flat_map(|y| (0..three.w).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let p = three.px(x, y);
            i32::from(p[0]) - i32::from(p[2]) > 60
        })
        .count();
    assert!(red > 300, "no `$error` border: {red} px");
    let bands = runs(&lit(&three, col));
    assert_eq!(bands.len(), 3, "three toasts: {bands:?}");

    // The first one's close icon (14 px at its right, on its first row).
    let mut pointer = desk.pointer();
    pointer.click(1200, 700, LAYOUT.0, LAYOUT.1);
    std::thread::sleep(Duration::from_millis(200));
    let close = (TOASTS.x + 40 + 380 - 12 - 7, (bands[0].0 + bands[0].1) / 2);
    let below = bands[1].0;
    pointer.click(close.0 as u32, close.1 as u32, LAYOUT.0, LAYOUT.1);
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    // The second toast slides up into the first's place: its top moves
    // up over several frames.
    let mut tops = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1200) {
        let s = desk.region("HEADLESS-1", TOASTS);
        if let Some(&t) = lit(&s, col).first() {
            tops.push(t);
        }
    }
    desk.wait("the first toast to leave", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 2
    });
    let two = desk.settled_ref("HEADLESS-1", TOASTS, "toasts_after_dismiss");
    let bands2 = runs(&lit(&two, col));
    eprintln!("toast tops while the one above leaves: {tops:?}, at rest {bands2:?}");
    assert_eq!(bands2.len(), 2, "{bands2:?}");
    // (A critical toast's `$error` border is not lit: its top shows a
    // pixel or two lower.)
    assert!(
        bands2[0].0.abs_diff(bands[0].0) <= 3,
        "the stack does not start at the top: {bands2:?}"
    );
    assert!(
        !release
            || tops
                .iter()
                .filter(|&&t| t > bands[0].0 + 4 && t + 4 < below)
                .count()
                >= 2,
        "no frames of the slide up caught: {tops:?}"
    );

    // A short timeout: it comes and goes by itself (the pointer is away,
    // so `while !hover` lets it run).
    let mut quick = note(4, "Timer", "alarm", "Tea", "Steeped");
    quick["notify"]["timeout_ms"] = 1500.into();
    desk.mock(quick);
    desk.wait("the fourth toast", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 3
    });
    desk.wait("the fourth toast to expire", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 2
    });
    // `after … while !hover`: a toast under the pointer outlives its
    // timeout, drawn in `$surface.hi` (`when hover`), and expires once
    // the pointer leaves.
    let mut held = note(5, "Timer", "alarm", "Tea", "Steeped");
    held["notify"]["timeout_ms"] = 4000.into();
    let at = Instant::now();
    desk.mock(held);
    desk.wait("the fifth toast", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 3
    });
    let fifth = runs(&lit(&desk.region("HEADLESS-1", TOASTS), col))[2];
    pointer.motion(
        (TOASTS.x + 40 + 200) as u32,
        ((fifth.0 + fifth.1) / 2) as u32,
        LAYOUT.0,
        LAYOUT.1,
    );
    if let Some(left) = Duration::from_millis(6000).checked_sub(at.elapsed()) {
        std::thread::sleep(left);
    }
    assert_eq!(
        runs(&lit(&desk.region("HEADLESS-1", TOASTS), col)).len(),
        3,
        "the hovered toast expired"
    );
    let hovered = desk.settled_ref("HEADLESS-1", TOASTS, "toasts_hover");
    // The hovered toast's background is not the others' (`$surface.hi`).
    let fill_at = |b: (usize, usize)| hovered.px(40 + 300, b.0 + 6);
    let bands_h = runs(&lit(&hovered, col));
    assert_eq!(bands_h.len(), 3, "{bands_h:?}");
    assert_ne!(
        fill_at(bands_h[2]),
        fill_at(bands_h[1]),
        "the hovered toast is not in `$surface.hi`"
    );
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the fifth toast to expire after the hover", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 2
    });
    // A click on the chat toast activates it (it leaves); a right click
    // on the critical one dismisses it; the panel then closes.
    let bands = runs(&lit(&desk.settled("HEADLESS-1", TOASTS), col));
    pointer.click(
        (TOASTS.x + 40 + 200) as u32,
        ((bands[1].0 + bands[1].1) / 2) as u32,
        LAYOUT.0,
        LAYOUT.1,
    );
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the chat toast to leave", 10, |d| {
        runs(&lit(&d.region("HEADLESS-1", TOASTS), col)).len() == 1
    });
    pointer.right_click(
        (TOASTS.x + 40 + 200) as u32,
        (bands[0].0 + 20) as u32,
        LAYOUT.0,
        LAYOUT.1,
    );
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the last toast to leave", 10, |d| {
        lit(&d.region("HEADLESS-1", TOASTS), col).is_empty()
    });
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
}

/// The OSD's pill on `HEADLESS-1`: 260 wide, bottom-centred, 96 above
/// the bottom (with room for its shadow).
const OSD: Rect = rect(1280 - 160, 1440 - 96 - 44 - 30, 320, 110);

/// The share of the OSD's meter filled with `$accent`: the meter is
/// the longest run (at least 120 px) of pixels unlike the pill (its
/// most common light colour) with the pill on both sides, on the row
/// where it is longest; its blue pixels are the fill. (Sampling the
/// pill a few rows above each row instead picked the meter's own track
/// as "the pill" below it, and an antialiased edge of a frame caught
/// near the end of the OSD's entrance then bounded a run across the
/// whole pill with no fill in it.)
fn meter_fill(img: &Img) -> Option<f64> {
    let mut counts = std::collections::HashMap::new();
    for y in 0..img.h {
        for x in 0..img.w {
            let p = img.px(x, y);
            if sum(p) >= 450 {
                *counts.entry(p).or_insert(0usize) += 1;
            }
        }
    }
    let pill = sum(counts.into_iter().max_by_key(|(_, n)| *n)?.0);
    let mut best: Option<(usize, (usize, usize))> = None;
    for y in 0..img.h {
        let cols: Vec<usize> = (0..img.w)
            .filter(|&x| sum(img.px(x, y)).abs_diff(pill) > 9)
            .collect();
        // Inside the pill: pill-coloured on both sides.
        let inside = |(a, b): (usize, usize)| {
            a > 0
                && b + 1 < img.w
                && sum(img.px(a - 1, y)).abs_diff(pill) <= 9
                && sum(img.px(b + 1, y)).abs_diff(pill) <= 9
        };
        if let Some(run) = runs(&cols)
            .into_iter()
            .filter(|r| inside(*r))
            .max_by_key(|(a, b)| b - a)
            && run.1 - run.0 >= 120
            && best.is_none_or(|(_, (a, b))| run.1 - run.0 > b - a)
        {
            best = Some((y, run));
        }
    }
    let (y, (a, b)) = best?;
    let fill = (a..=b).filter(|&x| blue(img.px(x, y))).count();
    Some(fill as f64 / (b - a + 1) as f64)
}

/// design.md (d): the OSD never shows at boot; a volume change from the
/// audio service shows it with the level in its meter and text, and it
/// hides 1.2 s after the last change; the wheel on the bar's volume row
/// (`audio.sink.volume -= dy * 0.05`) shows it too, 5% a notch; a
/// brightness change shows the brightness icon and level; the bar's
/// speaker icon mutes (level 0); on the focused output only.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "the OSD hides 1.2 s after the change that showed it, which a loaded debug build \
              can spend before its first frame; CI runs it in the release step"
)]
fn the_osd_follows_volume_and_brightness() {
    let Some(mut desk) = Desk::start("osd") else {
        return;
    };
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        desk.surface_count(),
        2,
        "an OSD at boot: {:?}",
        desk.surfaces()
    );
    let shown = |d: &Desk| sum(d.region("HEADLESS-1", OSD).px(160, 55)) > 450;

    desk.mock(serde_json::json!({"volume": 0.3}));
    let at = Instant::now();
    desk.wait("the OSD on a volume change", 10, shown);
    let vol = desk.reaches_ref("HEADLESS-1", OSD, "osd_volume_30");
    let fill = meter_fill(&vol).expect("no meter");
    assert!((fill - 0.3).abs() < 0.04, "meter at {fill:.2}, volume 0.3");
    // Hidden 1.2 s after the change (and not before 1 s).
    desk.wait("the OSD to hide", 10, |d| !shown(d));
    let gone = at.elapsed();
    assert!(
        gone >= Duration::from_millis(1000) && gone < Duration::from_millis(2600),
        "hid after {gone:?}"
    );

    // Two wheel notches down on the bar's volume row: 0.3 - 2 × 0.05.
    let mut pointer = desk.pointer();
    pointer.click(1200, 700, LAYOUT.0, LAYOUT.1);
    std::thread::sleep(Duration::from_millis(200));
    let bar = desk.settled("HEADLESS-1", BAR_1);
    let bg = bar.px(2000, 12);
    let speaker = runs(&ink_cols(&bar, 2200..2540, 14..38, bg))[0];
    let x = (speaker.0 + speaker.1) / 2;
    pointer.wheel(2, x as u32, 26, LAYOUT.0, LAYOUT.1);
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the OSD on a wheel", 10, shown);
    let wheel = desk.reaches_ref("HEADLESS-1", OSD, "osd_volume_20");
    let fill = meter_fill(&wheel).expect("no meter");
    assert!(
        (fill - 0.2).abs() < 0.04,
        "meter at {fill:.2} after two notches"
    );
    desk.wait("the OSD to hide", 10, |d| !shown(d));

    // Brightness: its own icon and level.
    desk.mock(serde_json::json!({"brightness": 0.8}));
    desk.wait("the OSD on a brightness change", 10, shown);
    let bright = desk.reaches_ref("HEADLESS-1", OSD, "osd_brightness_80");
    let fill = meter_fill(&bright).expect("no meter");
    assert!(
        (fill - 0.8).abs() < 0.04,
        "meter at {fill:.2}, brightness 0.8"
    );
    desk.wait("the OSD to hide", 10, |d| !shown(d));

    // The speaker icon on the bar mutes: the OSD shows level 0.
    pointer.click(x as u32, 26, LAYOUT.0, LAYOUT.1);
    pointer.motion(1200, 700, LAYOUT.0, LAYOUT.1);
    desk.wait("the OSD on mute", 10, shown);
    let muted = desk.reaches_ref("HEADLESS-1", OSD, "osd_muted");
    let fill = meter_fill(&muted).expect("no meter");
    assert!(fill < 0.02, "the meter is at {fill:.2} when muted");
    desk.wait("the OSD to hide", 10, |d| !shown(d));

    // On the focused output: focus HEADLESS-2, change the volume.
    desk.msg(&["focus", "output", "HEADLESS-2"]).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    desk.mock(serde_json::json!({"muted": false, "volume": 0.5}));
    let osd2 = rect(960 - 200, 1080 - 230, 400, 200);
    desk.wait("the OSD on HEADLESS-2", 10, |d| {
        let s = d.shot("HEADLESS-2").crop(osd2);
        (0..s.w).filter(|&x| blue(s.px(x, s.h / 2))).count() > 0
            || (0..s.h).any(|y| (0..s.w).filter(|&x| blue(s.px(x, y))).count() > 20)
    });
    assert!(!shown(&desk), "the OSD also showed on HEADLESS-1");
    desk.reaches_ref("HEADLESS-2", osd2, "osd_headless2");
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
}

/// The comparison fails on one glyph's worth of change in the clock
/// (well under 0.5% of the bar) and passes antialiasing-sized noise
/// spread over the whole bar.
#[test]
fn the_comparison_catches_one_glyph_but_not_noise() {
    let reference = Img::reference("bar_headless1");
    let img = Img {
        w: reference.width() as usize,
        h: reference.height() as usize,
        rgb: reference.into_raw(),
    };
    assert!(img.compare("bar_headless1").is_ok());
    // The clock's first glyph run nearest the centre, painted over with
    // the bar's background.
    let bg = img.px(img.w / 2, 11);
    let cols = ink_cols(&img, img.w / 2 - 120..img.w / 2 + 120, 14..38, bg);
    let glyph = runs(&cols)[0];
    let mut erased = img.clone();
    let mut changed = 0;
    for y in 14..38 {
        for x in glyph.0..=glyph.1 {
            if erased.px(x, y) != bg {
                changed += 1;
            }
            let i = (y * erased.w + x) * 3;
            erased.rgb[i..i + 3].copy_from_slice(&bg);
        }
    }
    let share = changed as f64 / (img.w * img.h) as f64;
    assert!(
        share < PIXEL_TOLERANCE,
        "one glyph is {share:.4} of the bar"
    );
    assert!(
        erased.compare("bar_headless1").is_err(),
        "a missing glyph ({changed} px) passed"
    );
    // Noise: one pixel in 401 off by 60 in a channel.
    let mut sparse = img.clone();
    for (i, p) in sparse.rgb.chunks_mut(3).enumerate() {
        if i % 401 == 0 {
            p[0] = p[0].wrapping_add(60);
        }
    }
    assert!(sparse.compare("bar_headless1").is_ok());
}

/// design.md's hello bar alone (no theme file), on the acceptance mock
/// (clock frozen): laid out by `split` (the window title at the start,
/// the clock truly centred, the battery at the end) and themed by the
/// built-in theme with the default seed's light palette (`$surface`
/// under it, its text in `$fg`), on both outputs, against references.
#[test]
fn the_hello_bar_alone_is_laid_out_and_themed() {
    const HELLO: [(&str, &str); 1] = [(
        "hello_bar.strand",
        include_str!("../../strand-compiler/tests/fixtures/hello_bar.strand"),
    )];
    let Some(desk) = Desk::start_with("hello", &HELLO, |b| b.contains("buffer=2560x32 ")) else {
        return;
    };
    let one = desk.settled_ref("HEADLESS-1", rect(0, 0, 2560, 32), "hello_bar_headless1");
    let two = desk.settled_ref("HEADLESS-2", rect(0, 0, 1920, 40), "hello_bar_headless2");
    // The built-in theme's palette: the default seed, light (no portal).
    let mut tokens = strand_theme::defaults::base_tokens();
    strand_theme::from_seed(
        strand_scene::Color::from_hex(strand_theme::defaults::DEFAULT_SEED).unwrap(),
        strand_theme::Options::default(),
    )
    .insert_into(&mut tokens);
    let (Some(strand_scene::PropValue::Color(surface)), Some(strand_scene::PropValue::Color(fg))) =
        (tokens.lookup("surface"), tokens.lookup("fg"))
    else {
        panic!("no $surface or $fg in the built-in theme");
    };
    let rgb = |c: strand_scene::Color| {
        let [r, g, b, _] = c.to_rgba8();
        [r, g, b]
    };
    let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2);
    for (name, img, scale) in [("HEADLESS-1", &one, 1.0), ("HEADLESS-2", &two, 1.25)] {
        let (w, h) = (img.w, img.h);
        // `$surface` behind it all: the corners and the empty stretches
        // between the three texts.
        for (x, y) in [(0, 0), (w - 1, h - 1), (w / 4, h / 2), (3 * w / 4, h / 2)] {
            assert!(
                close(img.px(x, y), rgb(surface)),
                "{name}: {:?} at {x},{y} is not $surface {:?}",
                img.px(x, y),
                rgb(surface)
            );
        }
        let runs = runs(&ink_cols(img, 0..w, 0..h, rgb(surface)));
        let start: Vec<_> = runs.iter().filter(|r| r.1 < w / 4).collect();
        let centre: Vec<_> = runs
            .iter()
            .filter(|r| r.0 > w / 4 && r.1 < 3 * w / 4)
            .collect();
        let end: Vec<_> = runs.iter().filter(|r| r.0 > 3 * w / 4).collect();
        assert_eq!(
            start.len() + centre.len() + end.len(),
            runs.len(),
            "{name}: ink outside start, centre and end: {runs:?}"
        );
        let (Some(s), Some(c0), Some(c1), Some(e)) =
            (start.first(), centre.first(), centre.last(), end.last())
        else {
            panic!("{name}: start, centre or end is empty: {runs:?}");
        };
        // Start at the bar's left edge, end at its right one (the bar has
        // no padding: a glyph's side bearing away), the clock centred.
        let edge = (4.0 * scale) as usize;
        assert!(s.0 <= edge, "{name}: start ink at {}", s.0);
        assert!(e.1 >= w - 1 - edge, "{name}: end ink at {}", e.1);
        let mid = (c0.0 + c1.1) as f64 / 2.0;
        assert!(
            (mid - w as f64 / 2.0).abs() <= 2.0 * scale,
            "{name}: clock {}..{} not centred on {}",
            c0.0,
            c1.1,
            w / 2
        );
        // The text is `$fg`: its darkest pixels (glyph stems) are fg.
        let darkest = (0..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .map(|(x, y)| img.px(x, y))
            .min_by_key(|p| sum(*p))
            .unwrap();
        assert!(
            darkest.iter().zip(rgb(fg)).all(|(x, y)| x.abs_diff(y) <= 6),
            "{name}: darkest ink {darkest:?} is not $fg {:?}",
            rgb(fg)
        );
    }
    assert!(desk.errors().is_empty(), "{:?}", desk.errors());
}

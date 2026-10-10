//! "The GPU leaves nothing running" (M4, docs/architecture.md,
//! "`strand-gpu`", "Budgets and tests"; m4-plan's
//! `gpu_is_released_when_idle`): `strand run` on headless sway. Panel
//! `Glow` (640×420) shows, while `on`, a 640×320 animated `shader` above
//! a strip (a translucent row with a gradient box, a still `shader` and
//! text); panel `Ref` (320×100) shows the same strip. Before anything
//! needs the GPU no Vulkan library is mapped and no `strand-gpu` thread
//! runs. Each cycle: `on` is set; `Glow` is promoted and presented
//! through the surface hand-off (lavapipe presents on CI's pixman sway;
//! the hardware leg may read back there), while `Ref`, too small to
//! promote, draws its still shader by reading the pass back; the
//! screenshot shows the still shader's colour in both (premultiplied,
//! red and blue in place), and `Glow`'s presented strip matches `Ref`'s
//! CPU-drawn strip within the GPU tolerance. Then `on` is cleared, which
//! hides the shaders with both panels left open: `Glow` is demoted (its
//! swapchain released, the surface taken back) and the device drops
//! once idle, the thread gone and the process taking no wakeups. PSS
//! after the second cycle is within 3 MiB of the PSS after the first.
//!
//! Lavapipe tier: `STRAND_REQUIRE_GPU=1` (with `STRAND_GPU_SOFTWARE=1`,
//! which accepts lavapipe) makes a device that does not come up a
//! failure; without it the test is skipped then. The device's idle time
//! is shortened with `STRAND_GPU_IDLE_MS` (tests only).

#![cfg(feature = "gpu")]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const GLOW: &str = r#"export state on = false

component Strip {
  row { width: 320; height: 100; pad: 10; gap: 10; radius: 12; bg: $surface.alpha(0.8)
    box { width: 80; height: 80; radius: 12; bg: linear(90deg, $accent, $error) }
    box { width: 60; height: 60
      if on { shader "still.wgsl" { width: 60; height: 60 } }
    }
    text "Strand" { color: $fg }
  }
}

panel Glow { anchor: top_left; width: 640; height: 420; open: true
  col {
    box { width: 640; height: 320
      if on { shader "aurora.wgsl" { width: 640; height: 320; u_speed: 1 } }
    }
    Strip
  }
}

panel Ref { anchor: bottom_right; width: 320; height: 100; open: true
  Strip
}
"#;

const AURORA: &str = "\
@group(1) @binding(0) var<uniform> u_speed: f32;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let t = fract(strand.time * u_speed + v.uv.x);
    return vec4<f32>(t * 0.5, 0.2, 0.6 * (1.0 - t), 1.0);
}
";

/// Premultiplied orange at half alpha (see [`still_over`]): red and blue
/// swapped, or straight alpha taken for premultiplied, would show
/// otherwise.
const STILL: &str = "\
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    return vec4<f32>(0.5, 0.25, 0.0, 0.5);
}
";

/// What [`STILL`] shows over `bg`: its premultiplied colour plus half
/// of what is under it.
fn still_over(bg: [u8; 3]) -> [u8; 3] {
    let src = [127.5, 63.75, 0.0];
    let mut out = [0u8; 3];
    for i in 0..3 {
        out[i] = (src[i] + bg[i] as f64 * 0.5).round().min(255.0) as u8;
    }
    out
}

/// The strip's top-left corner in `Glow` (below the 320 px shader box)
/// and in `Ref` (the bottom-right 320×100 of the 1920×1080 output).
const GLOW_STRIP: (usize, usize) = (0, 320);
const REF_STRIP: (usize, usize) = (1600, 980);
const STRIP: (usize, usize) = (320, 100);
/// Inside the still shader's box, from the strip's corner (10 px pad,
/// the 80 px gradient box, a 10 px gap: x 100..160; y within 10..80
/// however the row aligns it).
const STILL_AT: (usize, usize) = (130, 45);

/// Per-channel difference allowed between the GPU's and the CPU's strip
/// (render's `tests/gpu.rs`), and the share of pixels (edges) allowed
/// past it.
const GPU_TOLERANCE: u8 = 6;
const EDGE_SHARE: f64 = 0.02;

/// Two cycles' PSS may differ by this much (no growth).
const PSS_SLACK_KB: u64 = 3 * 1024;
/// The hardware leg: PSS after the drop over the pre-GPU baseline.
const HARDWARE_SLACK_KB: u64 = 6 * 1024;
/// The device outlives its last use this long (30 s outside tests).
const IDLE_MS: u64 = 1500;
const STEP: Duration = Duration::from_secs(30);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        if std::env::var_os("STRAND_KEEP_TMP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn until(what: &str, limit: Duration, mut ok: impl FnMut() -> bool, log: &dyn Fn() -> String) {
    let end = Instant::now() + limit;
    while !ok() {
        assert!(
            Instant::now() < end,
            "timed out waiting for {what}\n{}",
            log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        "xwayland disable\noutput HEADLESS-1 resolution 1920x1080 position 0 0 scale 1\n",
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
    let end = Instant::now() + Duration::from_secs(10);
    let display = loop {
        let found = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
        if let Some(d) = found {
            break d;
        }
        assert!(Instant::now() < end, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    (sway, display)
}

fn threads(pid: u32) -> Vec<String> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .map(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .collect()
}

fn gpu_thread(pid: u32) -> bool {
    threads(pid).iter().any(|t| t == "strand-gpu")
}

/// Context switches of every thread of `pid`.
fn switches(pid: u32) -> u64 {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return 0;
    };
    tasks
        .flatten()
        .map(|t| {
            std::fs::read_to_string(t.path().join("status"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("ctxt_switches:"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .sum::<u64>()
        })
        .sum()
}

fn pss_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("Pss:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

/// PSS in kB per mapping name (`[heap]`, a library, `[anon]`).
fn pss_by_mapping(pid: u32) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap_or_default();
    let mut name = String::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or("");
        if first.contains('-') && !first.ends_with(':') {
            // A mapping header: range perms offset dev inode [name].
            name = line
                .split_whitespace()
                .nth(5)
                .unwrap_or("[anon]")
                .to_string();
        } else if first == "Pss:" {
            let kb: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            *out.entry(name.clone()).or_default() += kb;
        }
    }
    out
}

/// The mappings whose PSS changed most from `a` to `b`.
fn growth(
    a: &std::collections::BTreeMap<String, u64>,
    b: &std::collections::BTreeMap<String, u64>,
) -> String {
    let mut d: Vec<(i64, &String)> = b
        .iter()
        .map(|(k, v)| (*v as i64 - a.get(k).copied().unwrap_or(0) as i64, k))
        .chain(
            a.iter()
                .filter(|(k, _)| !b.contains_key(*k))
                .map(|(k, v)| (-(*v as i64), k)),
        )
        .filter(|(d, _)| *d != 0)
        .collect();
    d.sort_by_key(|(d, _)| -d.abs());
    d.iter()
        .take(12)
        .map(|(d, k)| format!("{d:+} kB {k}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn vulkan_mapped(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .unwrap_or_default()
        .contains("libvulkan")
}

/// Waits until `pid` takes no context switch for a whole second (within
/// [`STEP`]).
fn quiet(pid: u32, what: &str, log: &dyn Fn() -> String) {
    let end = Instant::now() + STEP;
    loop {
        let before = switches(pid);
        std::thread::sleep(Duration::from_secs(1));
        if switches(pid) == before {
            return;
        }
        assert!(
            Instant::now() < end,
            "{what}: strand never went quiet\n{}",
            log()
        );
    }
}

/// A screenshot of `HEADLESS-1` (RGB).
struct Img {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Img {
    /// A binary PPM (`P6`, maxval 255), as `grim -t ppm` writes.
    fn ppm(bytes: &[u8]) -> Option<Img> {
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while bytes.get(at)?.is_ascii_whitespace() {
                at += 1;
            }
            let start = at;
            while !bytes.get(at)?.is_ascii_whitespace() {
                at += 1;
            }
            fields.push(String::from_utf8_lossy(&bytes[start..at]).into_owned());
        }
        if fields[0] != "P6" {
            return None;
        }
        let (w, h) = (fields[1].parse().ok()?, fields[2].parse().ok()?);
        let rgb = bytes.get(at + 1..)?.to_vec();
        (rgb.len() >= w * h * 3).then_some(Img { w, h, rgb })
    }

    fn px(&self, (x, y): (usize, usize)) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// The `size` region at `at`, row by row.
    fn region(&self, at: (usize, usize), size: (usize, usize)) -> Vec<[u8; 3]> {
        let mut out = Vec::with_capacity(size.0 * size.1);
        for y in at.1..at.1 + size.1 {
            for x in at.0..at.0 + size.0 {
                out.push(self.px((x, y)));
            }
        }
        out
    }

    fn save(&self, path: &Path) {
        let mut out = format!("P6\n{} {}\n255\n", self.w, self.h).into_bytes();
        out.extend_from_slice(&self.rgb);
        let _ = std::fs::write(path, out);
    }
}

fn shot(dir: &Path, display: &str) -> Option<Img> {
    let path = dir.join("shot.ppm");
    let ok = Command::new("grim")
        .args(["-t", "ppm", "-o", "HEADLESS-1"])
        .arg(&path)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WAYLAND_DISPLAY", display)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return None;
    }
    Img::ppm(&std::fs::read(&path).ok()?)
}

fn near(a: [u8; 3], b: [u8; 3], tol: u8) -> bool {
    a.iter().zip(b).all(|(p, q)| p.abs_diff(q) <= tol)
}

/// Pixels of `a` and `b` past [`GPU_TOLERANCE`], and the worst channel
/// difference.
fn compare(a: &[[u8; 3]], b: &[[u8; 3]]) -> (usize, u8) {
    let mut bad = 0;
    let mut worst = 0;
    for (p, q) in a.iter().zip(b) {
        let d = p
            .iter()
            .zip(q)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        if d > GPU_TOLERANCE {
            bad += 1;
        }
    }
    (bad, worst)
}

#[test]
fn gpu_is_released_when_idle() {
    for tool in ["sway", "grim"] {
        if !have(tool) {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed ***\n");
            return;
        }
    }
    let required = std::env::var_os("STRAND_REQUIRE_GPU").is_some();
    // The hardware leg (`gpu.sh`): ANV cannot present on a pixman sway,
    // so the promoted panel may be read back there. Lavapipe presents.
    let hardware = std::env::var("STRAND_GPU_HARDWARE").as_deref() == Ok("1");
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let tmp = TmpDir(target_tmp.join(format!("strand-gpu-idle-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    let dir = tmp.0.join("run");
    std::fs::create_dir_all(&dir).unwrap();
    rustix::fs::chmod(&dir, rustix::fs::Mode::from_raw_mode(0o700)).unwrap();
    let config = tmp.0.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("glow.strand"), GLOW).unwrap();
    std::fs::write(config.join("aurora.wgsl"), AURORA).unwrap();
    std::fs::write(config.join("still.wgsl"), STILL).unwrap();
    let (_sway, display) = sway(&dir);
    let socket = dir.join("strand.sock");
    let log_path = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("HOME", &tmp.0)
            .env("XDG_CONFIG_HOME", tmp.0.join("xdg-config"))
            .env("XDG_CACHE_HOME", tmp.0.join("cache"))
            .env("XDG_STATE_HOME", tmp.0.join("state"))
            .env("STRAND_MOCK", "acceptance")
            .env("STRAND_SOCKET", &socket)
            .env("STRAND_LOG", "info")
            .env("STRAND_GPU_IDLE_MS", IDLE_MS.to_string())
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let pid = strand.0.id();
    let log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let set = |on: bool| {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["set", "glow.on", if on { "true" } else { "false" }])
            .env("STRAND_SOCKET", &socket)
            .output()
            .unwrap();
        out.status.success()
    };
    until("the shell's socket", STEP, || set(false), &log);
    quiet(pid, "at boot", &log);
    assert!(!gpu_thread(pid), "a GPU thread before anything needs one");
    assert!(
        !vulkan_mapped(pid),
        "a Vulkan library is mapped before anything needs the GPU"
    );
    // Both strips are on screen (CPU-drawn, no shader yet).
    let mut img = None;
    until(
        "both panels on screen",
        STEP,
        || {
            img = shot(&dir, &display);
            img.as_ref().is_some_and(|i| {
                let bg = i.px((960, 540));
                i.px((GLOW_STRIP.0 + 40, GLOW_STRIP.1 + 50)) != bg
                    && i.px((REF_STRIP.0 + 40, REF_STRIP.1 + 50)) != bg
            })
        },
        &log,
    );
    let bg = img.as_ref().map_or([0; 3], |i| i.px((960, 540)));
    // The still shader is drawn over the strip's own background.
    let under = img.as_ref().map_or([0; 3], |i| {
        i.px((REF_STRIP.0 + STILL_AT.0, REF_STRIP.1 + STILL_AT.1))
    });
    let still = still_over(under);
    eprintln!("the strip {under:?} under the still shader: it shows {still:?}");

    // Lavapipe and LLVM stay mapped after the drop (about 80 MiB), so
    // the baseline is reported, not asserted against (gpu.sh's hardware
    // leg compares it).
    let baseline = pss_kb(pid);
    let baseline_maps = pss_by_mapping(pid);
    eprintln!("before the GPU: PSS {baseline} kB");
    let mut pss = Vec::new();
    let mut maps = Vec::new();
    let cycles: u32 = std::env::var("STRAND_GPU_IDLE_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    for cycle in 1..=cycles {
        let text = log();
        let attached = text.matches("attached (").count();
        let released = text.matches(" released").count();
        assert!(set(true), "strand set glow.on true");
        // The panel animates 0.2 Mpx a frame: promoted after 500 ms.
        let end = Instant::now() + STEP;
        let mode = loop {
            let text = log();
            if text.matches("attached (").count() > attached {
                let last = text.rsplit("attached (").next().unwrap_or("");
                break if last.starts_with("Present") {
                    "Present"
                } else {
                    "Readback"
                };
            }
            if text.contains("GPU unavailable") {
                assert!(!required, "STRAND_REQUIRE_GPU is set but:\n{text}");
                eprintln!("\n*** SKIPPED: no GPU device ***\n{text}");
                return;
            }
            assert!(
                Instant::now() < end,
                "cycle {cycle}: the panel was never promoted\n{text}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            mode == "Present" || hardware,
            "cycle {cycle}: lavapipe presents on a pixman sway, but the panel was attached \
             for {mode}\n{}",
            log()
        );
        assert!(
            gpu_thread(pid),
            "cycle {cycle}: promoted without a GPU thread"
        );
        assert_eq!(
            log().matches("attached (").count(),
            attached + 1,
            "cycle {cycle}: only Glow is promoted ({mode}); Ref reads its pass back"
        );
        // Shown three times promotion's 500 ms idle window: a presented
        // animation is not idle, so it stays on the GPU.
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            log().matches(" released").count(),
            released,
            "cycle {cycle}: Glow went back to the CPU while its shader animated\n{}",
            log()
        );
        // Then the screen: Glow's strip presented by the GPU, Ref's drawn
        // by the CPU with its pass read back, both showing the still
        // shader's colour.
        let img: std::cell::RefCell<Option<Img>> = std::cell::RefCell::new(None);
        let at = |c: (usize, usize)| (c.0 + STILL_AT.0, c.1 + STILL_AT.1);
        let stills = || {
            img.borrow()
                .as_ref()
                .map(|i: &Img| (i.px(at(GLOW_STRIP)), i.px(at(REF_STRIP))))
        };
        until(
            &format!("cycle {cycle}: the still shader in both strips"),
            STEP,
            || {
                *img.borrow_mut() = shot(&dir, &display);
                stills().is_some_and(|(g, r)| near(g, still, 3) && near(r, still, 3))
            },
            &|| {
                format!(
                    "Glow's and Ref's still shader: {:?}, want {still:?}\n{}",
                    stills(),
                    log()
                )
            },
        );
        let img = img.into_inner().expect("a screenshot");
        let glow = img.region(GLOW_STRIP, STRIP);
        let reference = img.region(REF_STRIP, STRIP);
        let (bad, worst) = compare(&glow, &reference);
        let share = bad as f64 / (STRIP.0 * STRIP.1) as f64;
        eprintln!(
            "cycle {cycle}: Glow's strip ({mode}) against Ref's (CPU): {bad} pixels past \
             {GPU_TOLERANCE}, worst {worst}"
        );
        if share > EDGE_SHARE {
            img.save(&tmp.0.join("gpu_idle.actual.ppm"));
        }
        assert!(
            share <= EDGE_SHARE,
            "cycle {cycle}: Glow's strip ({mode}) differs from Ref's in {bad} pixels by more \
             than {GPU_TOLERANCE} (worst {worst}); STRAND_KEEP_TMP keeps the screenshot"
        );
        assert_ne!(
            img.px((320, 160)),
            bg,
            "cycle {cycle}: the animated shader is on screen"
        );
        // Hidden (both panels stay open): Glow goes back to the CPU, its
        // swapchain released before the manager commits it again.
        assert!(set(false), "strand set glow.on false");
        if mode == "Present" {
            until(
                &format!("cycle {cycle}: Glow demoted and taken back"),
                STEP,
                || log().matches(" released").count() > released,
                &log,
            );
        }
        until(
            &format!("cycle {cycle}: the GPU thread to end"),
            STEP,
            || !gpu_thread(pid),
            &log,
        );
        quiet(pid, &format!("cycle {cycle}"), &log);
        // No wakeups: nothing is left to tick.
        let before = switches(pid);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(
            switches(pid),
            before,
            "cycle {cycle}: woke after the device dropped\n{}",
            log()
        );
        // The CPU draws both strips again, without the still shader.
        let img = shot(&dir, &display).expect("a screenshot");
        let (bad, worst) = compare(
            &img.region(GLOW_STRIP, STRIP),
            &img.region(REF_STRIP, STRIP),
        );
        assert_eq!(
            (bad, worst),
            (0, 0),
            "cycle {cycle}: both strips drawn by the CPU again"
        );
        let kb = pss_kb(pid);
        eprintln!("cycle {cycle}: PSS {kb} kB, threads {:?}", threads(pid));
        pss.push(kb);
        maps.push(pss_by_mapping(pid));
    }
    let (first, second) = (pss[0], pss[1]);
    assert!(
        second <= first + PSS_SLACK_KB,
        "PSS grew from {first} kB after the first cycle to {second} kB after the second:\n{}",
        growth(&maps[0], &maps[1])
    );
    let text = log();
    assert!(text.contains("GPU: the thread ended"), "{text}");
    // The hardware leg (`scripts/container/gpu.sh`), in release: a
    // hardware driver unmaps its libraries with the device, so PSS goes
    // back to within about 6 MiB of the baseline (the spike's ANV left
    // 5.2-6.3 MiB): what stays is strand's own GPU code paged in. A debug
    // binary's is several times larger (8.9 MiB of it on ANV), so a debug
    // run only reports it.
    eprintln!(
        "after the drop: {first} kB, {} kB over the baseline:\n{}",
        first.saturating_sub(baseline),
        growth(&baseline_maps, &maps[0])
    );
    if hardware && !cfg!(debug_assertions) {
        assert!(
            first <= baseline + HARDWARE_SLACK_KB,
            "PSS {first} kB after the drop, more than {HARDWARE_SLACK_KB} kB over the \
             {baseline} kB before the GPU:\n{}",
            growth(&baseline_maps, &maps[0])
        );
    }
}

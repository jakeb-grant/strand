//! "A settled surface goes back, and the device stays warm" (M4's
//! `gpu_is_released_when_idle`, turned by the owner's gpu-first ruling:
//! docs/architecture.md, "`strand-gpu`", "Budgets and tests";
//! decisions.md gpu-first): `strand run` on headless sway. Panel `Glow`
//! (640×420) shows, while `on`, a 640×320 animated `shader` above a
//! strip (a translucent row with a gradient box, a still `shader` and
//! text), and over the animation a box with `filter: bloom(8)`, a
//! bundled pass whose input is read back and whose pixels come back to
//! render (m4-gpu-effects), beside a plain box of the same colour;
//! panel `Ref` (320×100) shows the same strip. At boot the device is
//! started before anything needs it: the `strand-gpu` thread runs and
//! Vulkan is mapped, and the shell takes no wakeups. Each cycle: `on` is
//! set; `Glow` is promoted and presented through the surface hand-off
//! (lavapipe presents on CI's pixman sway; the hardware leg reads back on
//! a pixman sway, and presents on the GPU sway `gpu-session.sh` names
//! with `STRAND_GPU_PRESENT=1`), while `Ref`, too small to promote, draws
//! its still shader by reading the pass back; the screenshot shows the
//! still shader's colour in both (premultiplied, red and blue in place),
//! the bloomed box is brighter inside than the plain one (the GPU's
//! bloom screens its glow over the box; the CPU's fallback glow is drawn
//! under it, leaving it as plain), and `Glow`'s presented strip matches
//! `Ref`'s CPU-drawn strip within the GPU tolerance. Then `on` is
//! cleared, which hides the shaders with both panels left open: settled,
//! `Glow` goes back to the CPU (its swapchain released, the surface taken
//! back) at its one wake, and the device stays up: the thread keeps
//! running and the process takes no wakeups. The memory strand holds
//! (its PSS less what it shares of mapped files, [`held_kb`]) after
//! every later cycle is within 4 MiB of that after the first (no
//! growth); the hardware leg runs three cycles, and that is its leak
//! tripwire (decisions.md m5-open-budgets, pss-flake).
//!
//! Lavapipe tier: `STRAND_REQUIRE_GPU=1` (with `STRAND_GPU_SOFTWARE=1`,
//! which accepts lavapipe) makes a device that does not come up a
//! failure; without it the test is skipped then.

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
      if on {
        shader "aurora.wgsl" { width: 640; height: 320; u_speed: 1 }
        box { place: absolute; x: 20; y: 20; width: 120; height: 60; radius: 12;
          bg: #f5c2e7; filter: bloom(8) }
        box { place: absolute; x: 160; y: 20; width: 60; height: 60; radius: 12;
          bg: #f5c2e7 }
      }
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

/// Inside the bloomed box and the plain box beside it, in `Glow`.
const BLOOMED: (usize, usize) = (80, 50);
const PLAIN: (usize, usize) = (190, 50);
/// How much brighter (summed channels) the bloomed box's inside must be:
/// the GPU's bloom screens #f5c2e7's glow over it (about +60); the
/// CPU's glow leaves it as the plain box (+0).
const BLOOM_GAIN: i32 = 24;

/// The bloomed box's brightness over the plain box's (summed channels).
fn bloom_gain(i: &Img) -> i32 {
    let sum = |p: [u8; 3]| p.iter().map(|&c| i32::from(c)).sum::<i32>();
    sum(i.px(BLOOMED)) - sum(i.px(PLAIN))
}

/// Per-channel difference allowed between the GPU's and the CPU's strip
/// (render's `tests/gpu.rs`), and the share of pixels (edges) allowed
/// past it.
const GPU_TOLERANCE: u8 = 6;
const EDGE_SHARE: f64 = 0.02;

/// A later cycle's held memory ([`held_kb`]) after the drop may exceed
/// the first cycle's by this much: the leak tripwire. What the first cycle leaves (strand's
/// GPU code paged in, mimalloc's pages, libstdc++, a glibc arena of
/// Mesa's) is paid once; on ANV later cycles came back within 36-66 kB
/// of the first, except that the arena's resident part moves by 2-2.7
/// MiB between cycles in some runs (+1,960 and +2,048 kB, -2,053 and
/// -2,742 kB), so 4 MiB, while a leaked device (20-40 MB) adds itself
/// every cycle and a leaked swapchain (3.2 MB) passes the bound by the
/// third (decisions.md m5-open-budgets).
const PSS_SLACK_KB: u64 = 4 * 1024;
/// Cycles the hardware leg runs by default (two elsewhere), so that the
/// growth bound sees a leak twice.
const HARDWARE_CYCLES: u32 = 3;
/// The readback hardware leg's backstop after a demotion over the
/// baseline (before gpu-first, after the device's drop over the pre-GPU
/// baseline): the spike's ANV left 5.2-6.3 MiB, and four runs here
/// 4,611-6,123 kB after the first cycle and up to 7,466 kB after a
/// later one, the arena's swing again; 6 MiB was 21 kB from failing.
/// 12 MiB, the presented leg's old bound: a device held past the drop
/// (20-40 MB) still trips it (decisions.md m5-open-budgets).
const HARDWARE_SLACK_KB: u64 = 12 * 1024;
/// The presented hardware leg's backstop after a demotion (before
/// gpu-first, after the device's drop): ANV's presented
/// path left 4.6-8.1 MiB in six earlier runs and 4,726-8,506 kB after
/// the first cycle in thirteen here (up to 8,833 kB after a later one),
/// the arena's swing included; growth between cycles
/// ([`PSS_SLACK_KB`]) is the tripwire, and this bound only catches a
/// device held past the drop (20-40 MB) (decisions.md
/// m5-speed-gpu-bound, m5-open-budgets).
const PRESENTED_SLACK_KB: u64 = 16 * 1024;
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

/// The renderer of the test's own sway: pixman, as every sway test,
/// unless the hardware leg asks to present (`STRAND_GPU_PRESENT=1`) and
/// its session names a GPU renderer that started on the render node
/// (`STRAND_GPU_SWAY_RENDERER`, exported by
/// `scripts/container/gpu-session.sh`). ANV's WSI presents only to a
/// compositor offering linux-dmabuf, which a pixman sway does not
/// (decisions.md m5-infra, spike 1). Opt-in: on the presented path the
/// laptop's ANV left 4.6-8.8 MiB after the drop (past the 6 MiB the
/// readback leg was held to then), so that leg is held to its own 16 MiB
/// backstop ([`PRESENTED_SLACK_KB`]) beside the growth tripwire
/// ([`PSS_SLACK_KB`]; decisions.md m5-speed-gpu-bound,
/// m5-open-budgets); the default leg stays the readback one it was.
fn sway_renderer() -> Option<String> {
    let on = |v: &str| std::env::var(v).as_deref() == Ok("1");
    std::env::var("STRAND_GPU_SWAY_RENDERER").ok().filter(|r| {
        on("STRAND_GPU_HARDWARE")
            && on("STRAND_GPU_PRESENT")
            && matches!(r.as_str(), "gles2" | "vulkan")
    })
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

/// A `smaps_rollup` field of `pid`, in kB (0 when missing).
fn rollup_kb(pid: u32, field: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .find(|l| l.split_whitespace().next() == Some(field))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

fn pss_kb(pid: u32) -> u64 {
    rollup_kb(pid, "Pss:")
}

/// (pss-flake) What `pid` holds: its anonymous memory (heap, the
/// device's and the driver's allocations, a file page it wrote) and its
/// shared memory (wl_shm and memfd buffers), each proportional as PSS
/// is, and of those what is swapped out (`SwapPss`: a page swapped out
/// under load in one cycle and back in by the next is not growth), but
/// not its share of mapped files' pages (strand's code, Mesa,
/// libLLVM). A file page is divided among every process mapping it,
/// whoever they are: on a loaded machine the other test processes
/// running strand or lavapipe start and end beside this one, and
/// strand's share of libLLVM's tens of MB moved by 27 MB between two
/// cycles with nothing of its own changed. A leak is memory strand
/// holds (a device, a swapchain's buffers, an arena), never a file's
/// page, so the tripwire reads this.
fn held_kb(pid: u32) -> u64 {
    ["Pss_Anon:", "Pss_Shmem:", "SwapPss:"]
        .iter()
        .map(|f| rollup_kb(pid, f))
        .sum()
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
/// Fails unless `p` is still running (a zombie is reaped and fails).
fn alive(p: &mut Proc, what: &str, log: &dyn Fn() -> String) {
    match p.0.try_wait() {
        Ok(None) => {}
        Ok(Some(status)) => panic!("{what}: strand exited ({status})\n{}", log()),
        Err(e) => panic!("{what}: strand's status: {e}\n{}", log()),
    }
}

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
fn a_settled_surface_goes_back_and_the_device_stays_warm() {
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
    // so the promoted panel may be read back there. Lavapipe presents,
    // and so does hardware on a sway rendering on the render node
    // (`sway_renderer`).
    let hardware = std::env::var("STRAND_GPU_HARDWARE").as_deref() == Ok("1");
    let gpu_sway = sway_renderer();
    if let Some(renderer) = &gpu_sway {
        eprintln!("the test's sway renders with {renderer} on /dev/dri/renderD128");
    }
    // Under the system's temp dir, not the target dir: sway's IPC socket
    // path (`<run>/sway-ipc.<uid>.<pid>.sock`) must fit `sun_path`'s 108
    // bytes, which a worktree's target dir can pass.
    let tmp = TmpDir(std::env::temp_dir().join(format!("strand-gpu-idle-{}", std::process::id())));
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
    let mut strand = Proc(
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
    // (gpu-first) The warm start: the device comes up with the shell,
    // before anything needs it, and the shell then goes quiet.
    until(
        "the warm device",
        STEP,
        || {
            let text = log();
            text.contains("GPU: the device is up") || text.contains("everything draws on the CPU")
        },
        &log,
    );
    if !log().contains("GPU: the device is up") {
        assert!(!required, "STRAND_REQUIRE_GPU is set but:\n{}", log());
        eprintln!("\n*** SKIPPED: no GPU device ***\n{}", log());
        return;
    }
    quiet(pid, "at boot", &log);
    assert!(gpu_thread(pid), "the warm device's thread runs at boot");
    assert!(vulkan_mapped(pid), "the warm device has Vulkan mapped");
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

    // The baseline holds the warm device (lavapipe and LLVM on the
    // lavapipe tier): reported, and compared on gpu.sh's hardware leg.
    let baseline = pss_kb(pid);
    let baseline_maps = pss_by_mapping(pid);
    eprintln!("warm, before any promotion: PSS {baseline} kB");
    let mut pss = Vec::new();
    let mut helds = Vec::new();
    let mut maps = Vec::new();
    let cycles: u32 = std::env::var("STRAND_GPU_IDLE_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if hardware { HARDWARE_CYCLES } else { 2 });
    assert!(cycles >= 2, "STRAND_GPU_IDLE_CYCLES: at least two cycles");
    for cycle in 1..=cycles {
        let text = log();
        let attached = text.matches("attached (").count();
        let released = text.matches(" released").count();
        assert!(set(true), "strand set glow.on true");
        // The panel animates 0.2 Mpx a frame: promoted at its third.
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
            mode == "Present" || (hardware && gpu_sway.is_none()),
            "cycle {cycle}: lavapipe presents on a pixman sway, and hardware on a GPU sway \
             ({gpu_sway:?}), but the panel was attached for {mode}\n{}",
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
        // A presented animation is not settled: it stays on the GPU.
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
        let gain = || img.borrow().as_ref().map(bloom_gain);
        until(
            &format!("cycle {cycle}: the still shader in both strips, the box bloomed"),
            STEP,
            || {
                *img.borrow_mut() = shot(&dir, &display);
                stills().is_some_and(|(g, r)| near(g, still, 3) && near(r, still, 3))
                    && gain().is_some_and(|g| g >= BLOOM_GAIN)
            },
            &|| {
                format!(
                    "Glow's and Ref's still shader: {:?}, want {still:?}; the bloomed box \
                     over the plain one: {:?}, want {BLOOM_GAIN} or more (the GPU's bloom)\n{}",
                    stills(),
                    gain(),
                    log()
                )
            },
        );
        eprintln!(
            "cycle {cycle}: the bloomed box over the plain one: {:?}",
            gain()
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
        // The promoted cost (features.md M5, "The promoted GPU cost on
        // hardware"): PSS with the panel promoted and animating, against
        // the baseline before the GPU. Reported, never gated here.
        let promoted = pss_kb(pid);
        eprintln!(
            "cycle {cycle}: promoted ({mode}): PSS {promoted} kB, {} kB over the baseline \
             of {baseline} kB",
            promoted.saturating_sub(baseline)
        );
        // Its frame time (the same box): `strand report`'s p50/p95/p99
        // for Glow, promoted, over the frames drawn among its last 600
        // paints (the CPU's frames before the promotion among them),
        // each from its paint through the GPU's run of it, the waits for
        // a swapchain image left out (decisions.md m5-tools). Reported,
        // never gated: the run is the record.
        let report = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["report", "--json"])
            .env("STRAND_SOCKET", &socket)
            .output()
            .unwrap();
        assert!(report.status.success(), "strand report --json: {report:?}");
        let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
        let glow = report["surfaces"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["namespace"] == "strand-Glow"))
            .unwrap_or_else(|| panic!("cycle {cycle}: no strand-Glow in the report: {report}"));
        assert!(
            !glow["promoted"].is_null(),
            "cycle {cycle}: the report says Glow is not promoted ({mode}): {glow}"
        );
        let f = &glow["frames"];
        let us = |k: &str| f[k].as_u64().unwrap_or(0);
        assert!(us("drawn") > 0, "cycle {cycle}: no frames drawn: {glow}");
        eprintln!(
            "cycle {cycle}: promoted ({mode}) frame time: p50 {} us, p95 {} us, p99 {} us \
             over {} drawn ({} unchanged); backend {}, gpu {}",
            us("p50_us"),
            us("p95_us"),
            us("p99_us"),
            us("drawn"),
            us("unchanged"),
            glow["backend"],
            report["gpu"]
        );
        // Hidden (both panels stay open): settled, Glow goes back to the
        // CPU at its wake, its swapchain released before the manager
        // commits it again.
        assert!(set(false), "strand set glow.on false");
        until(
            &format!("cycle {cycle}: Glow demoted and taken back"),
            STEP,
            || log().matches(" released").count() > released,
            &log,
        );
        quiet(pid, &format!("cycle {cycle}"), &log);
        // The device stays warm, and nothing is left to tick: no wakeups.
        assert!(gpu_thread(pid), "cycle {cycle}: the device is kept");
        let before = switches(pid);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(
            switches(pid),
            before,
            "cycle {cycle}: woke after the demotion\n{}",
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
        let held = held_kb(pid);
        eprintln!(
            "cycle {cycle}: PSS {kb} kB (held {held} kB, mapped files {} kB), threads {:?}",
            rollup_kb(pid, "Pss_File:"),
            threads(pid)
        );
        // Every check since the demotion passes for a strand that died
        // during it (no threads, no switches, two blank strips, PSS 0):
        // it must still run and answer.
        alive(&mut strand, &format!("cycle {cycle}"), &log);
        assert!(
            set(false),
            "cycle {cycle}: strand stopped answering after the demotion\n{}",
            log()
        );
        assert!(kb > 0 && held > 0, "cycle {cycle}: no PSS read for strand");
        pss.push(kb);
        helds.push(held);
        maps.push(pss_by_mapping(pid));
    }
    // No growth: the first cycle pays the one-time costs, a leak adds
    // itself every cycle ([`PSS_SLACK_KB`]), in what strand holds
    // ([`held_kb`]).
    let first = helds[0];
    for (i, &kb) in helds.iter().enumerate().skip(1) {
        let cycle = i + 1;
        eprintln!(
            "cycle {cycle}: held {:+} kB against the first cycle (bound {PSS_SLACK_KB} kB), \
             PSS {:+} kB",
            kb as i64 - first as i64,
            pss[i] as i64 - pss[0] as i64
        );
        assert!(
            kb <= first + PSS_SLACK_KB,
            "the memory strand holds grew from {first} kB after the first cycle to {kb} kB \
             after cycle {cycle}, more than {PSS_SLACK_KB} kB (PSS by mapping, files \
             included):\n{}",
            growth(&maps[0], &maps[i])
        );
    }
    let first = pss[0];
    let text = log();
    assert!(
        !text.contains("GPU: the thread ended"),
        "the warm device was dropped\n{text}"
    );
    // The hardware leg (`scripts/container/gpu.sh`), in release: after
    // a demotion PSS is back within 12 MiB (16 presenting) of the warm
    // baseline: what a promotion leaves is the vello_gpu scene's and the
    // frame target's pages, mimalloc's pages and a glibc arena of Mesa's,
    // never a swapchain or a second device. A debug binary's residue is
    // several times larger, so a debug run only reports it. The bounds
    // are backstops ([`HARDWARE_SLACK_KB`], [`PRESENTED_SLACK_KB`]);
    // growth between cycles is the tripwire.
    eprintln!(
        "after the demotion: {first} kB, {} kB over the warm baseline:\n{}",
        first.saturating_sub(baseline),
        growth(&baseline_maps, &maps[0])
    );
    // The presented leg (`STRAND_GPU_PRESENT=1` on a GPU sway) is held
    // to a 16 MiB backstop: growth between cycles above is the leak
    // tripwire (decisions.md m5-speed-gpu-bound, m5-open-budgets,
    // gpu-first). Every cycle is checked, the highest reported.
    let (worst_at, worst) = pss
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(_, kb)| kb)
        .unwrap_or((0, first));
    let bound = if gpu_sway.is_some() {
        PRESENTED_SLACK_KB
    } else {
        HARDWARE_SLACK_KB
    };
    if hardware && !cfg!(debug_assertions) {
        assert!(
            worst <= baseline + bound,
            "PSS {worst} kB after cycle {}'s demotion, more than {bound} kB over the warm \
             {baseline} kB before any promotion:\n{}",
            worst_at + 1,
            growth(&baseline_maps, &maps[worst_at])
        );
    }
}

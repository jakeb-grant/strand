//! "The GPU leaves nothing running" (M4, docs/architecture.md,
//! "`strand-gpu`", "Budgets and tests"): `strand run` on headless sway
//! with a large animated `shader` panel. Before anything needs the GPU no
//! Vulkan library is mapped and no `strand-gpu` thread runs; each cycle
//! (the panel opened, its surface promoted and drawn by the GPU, the
//! panel closed, the device dropped once idle) ends with the thread gone
//! and the process taking no wakeups, and the PSS after the second cycle
//! is within 3 MiB of the PSS after the first.
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
panel Glow { anchor: center; width: 640; height: 400; open: on
  shader "aurora.wgsl" { width: 640; height: 400; u_speed: 1 }
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

#[test]
fn each_gpu_cycle_ends_with_the_thread_gone_and_no_growth() {
    if !have("sway") {
        assert!(
            std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
            "sway is not installed but STRAND_REQUIRE_SWAY is set"
        );
        eprintln!("\n*** SKIPPED: sway is not installed ***\n");
        return;
    }
    let required = std::env::var_os("STRAND_REQUIRE_GPU").is_some();
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
        let attached = log().matches("attached (").count();
        assert!(set(true), "strand set glow.on true");
        // The panel animates 0.26 Mpx a frame: promoted after 500 ms.
        let end = Instant::now() + STEP;
        loop {
            let text = log();
            if text.matches("attached (").count() > attached {
                break;
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
        }
        assert!(
            gpu_thread(pid),
            "cycle {cycle}: promoted without a GPU thread"
        );
        // GPU frames for a while.
        std::thread::sleep(Duration::from_millis(500));
        assert!(set(false), "strand set glow.on false");
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
    if std::env::var("STRAND_GPU_HARDWARE").as_deref() == Ok("1") && !cfg!(debug_assertions) {
        assert!(
            first <= baseline + HARDWARE_SLACK_KB,
            "PSS {first} kB after the drop, more than {HARDWARE_SLACK_KB} kB over the \
             {baseline} kB before the GPU:\n{}",
            growth(&baseline_maps, &maps[0])
        );
    }
}

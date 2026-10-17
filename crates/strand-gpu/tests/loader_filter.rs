//! (gpu-first review) The loader filter `strand_gpu::loader_disable`
//! writes keeps out of the Vulkan loader's enumeration every driver that
//! cannot drive a GPU here: with no render node (the container, CI), a
//! probe with the filter set finds no adapter and maps neither lavapipe
//! nor RADV nor the LLVM they link, however a software adapter would be
//! judged; without it the same probe finds lavapipe (so the filter, not
//! the machine, made the difference). With a GPU (`gpu.sh`) the filtered
//! probe still finds it. Each probe runs in a child process (this test binary
//! again), since a loaded driver stays mapped for the process.
//!
//! The children search the drivers as on a person's machine: the
//! image's own choice of lavapipe (`VK_DRIVER_FILES`) is removed, so the
//! loader reads every installed manifest (Mesa's hardware drivers find
//! no device without a render node). Where a hardware adapter answers
//! (`gpu.sh`), lavapipe is not the only driver and the test says so and
//! stops.

mod common;

use std::process::Command;

use strand_gpu::{GpuErrorKind, GpuOptions, GpuReply, LOADER_CHOICE_ENVS, LOADER_DISABLE_ENV};

/// Set in the child: probe and print what came of it.
const CHILD: &str = "STRAND_GPU_FILTER_CHILD";

/// The child's probe: a `Gpu` that accepts a software adapter, so only
/// the loader's enumeration decides; its first reply and whether
/// lavapipe or LLVM got mapped, as one line.
fn probe() -> String {
    let (tx, pings) = std::sync::mpsc::channel();
    let waker = Box::new(move || {
        let _ = tx.send(());
    });
    let mut h = common::Harness {
        gpu: strand_gpu::Gpu::spawn(waker, GpuOptions { software: true }),
        pings,
    };
    let first = match h.next() {
        GpuReply::Ready(info) => format!("ready software={}", info.software),
        GpuReply::Unavailable(e) if e.kind == GpuErrorKind::NoAdapter => "no-adapter".into(),
        other => format!("other {other:?}"),
    };
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let llvm = maps.contains("libLLVM");
    let lvp = maps.contains("libvulkan_lvp");
    format!("PROBE {first} llvm={llvm} lvp={lvp}")
}

fn run_child(filter: Option<&str>) -> String {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.args([
        "--exact",
        "the_loader_filter_keeps_lavapipe_and_llvm_out",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD, "1");
    for v in LOADER_CHOICE_ENVS {
        cmd.env_remove(v);
    }
    match filter {
        Some(f) => cmd.env(LOADER_DISABLE_ENV, f),
        None => cmd.env_remove(LOADER_DISABLE_ENV),
    };
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the child failed:\n{text}");
    // The harness prints the test's name first, on the same line.
    text.split("PROBE ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .unwrap_or_else(|| panic!("no PROBE line:\n{text}"))
        .to_string()
}

#[test]
fn the_loader_filter_keeps_lavapipe_and_llvm_out() {
    if std::env::var_os(CHILD).is_some() {
        println!("{}", probe());
        return;
    }
    let vendors = strand_gpu::render_vendors();
    let filter = strand_gpu::loader_disable(
        &GpuOptions { software: false },
        vendors.as_deref(),
        None,
        false,
    )
    .expect("a filter with software refused");
    let filter = filter.to_str().unwrap().to_string();
    let open = run_child(None);
    let filtered = run_child(Some(&filter));
    eprintln!("render nodes' vendors {vendors:x?}\n  unfiltered: {open}\n  filtered: {filtered}");
    assert!(
        filtered.starts_with("no-adapter") || filtered.starts_with("ready software=false"),
        "{filtered}"
    );
    // Filtered, lavapipe is never loaded, and LLVM only by RADV where an
    // AMD GPU is (its vendor unknown, RADV is not kept out).
    assert!(filtered.ends_with("lvp=false"), "{filtered}");
    if vendors.as_ref().is_some_and(|v| !v.contains(&0x1002)) {
        assert!(filtered.contains("llvm=false"), "{filtered}");
    }
    if !strand_gpu::device_possible(&GpuOptions { software: false }) {
        // No render node (the container, CI): unfiltered, lavapipe answers
        // and LLVM is mapped; filtered, no adapter.
        assert_eq!(open, "ready software=true llvm=true lvp=true");
        assert!(filtered.starts_with("no-adapter"), "{filtered}");
    } else if open.starts_with("ready software=false") {
        // A GPU (gpu.sh): the filter keeps it.
        assert!(filtered.starts_with("ready software=false"), "{filtered}");
    }
}

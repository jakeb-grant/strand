//! `strand report` (S-tools; design.md, "`strand report`"; m5-plan,
//! S-tools): the compositor, its globals with their versions, the
//! outputs and their scales, the process's PSS, each surface's frame
//! timing over its last 600 frames, the GPU's status, reason and lost
//! devices, and why a surface is on the GPU. No serials or ids: outputs
//! are named by connector, make and model (a description often carries
//! the serial), surfaces by namespace and output, and no global names,
//! surface ids or node ids are given.
//!
//! The data comes through the snapshot hop (docs/architecture.md, "M5
//! interfaces"): logic asks main (`ToMain::Snapshot` with
//! `SnapshotKind::Report`), main answers from its own state between
//! frames ([`main_part`]), and logic adds what it reads itself
//! ([`logic_part`]: the PSS and the compositor's name) before it answers
//! the socket (`run/request/report.rs`). `--json` prints the report as
//! one line; without it the CLI prints [`text`].

use serde_json::{Value as Json, json};
use strand_scene::{Backend, GpuStatus};
use strand_surface::State;

use crate::demo::host::Host;
use crate::ipc::Request;

/// Main's half: everything it holds (the registry, the outputs, the
/// surfaces, the renderer's frame timing and GPU state).
pub(crate) fn main_part(state: &State<Host>) -> Json {
    let r = &state.host().renderer;
    let monitors = state.monitors();
    let globals: Vec<Json> = state
        .globals()
        .into_iter()
        .map(|(interface, version)| json!({"interface": interface, "version": version}))
        .collect();
    let outputs: Vec<Json> = monitors
        .iter()
        .map(|m| {
            json!({
                "name": m.connector,
                "make": m.make,
                "model": m.model,
                "scale": m.scale.as_f64(),
                "size": m.logical_size.map(|(w, h)| json!([w, h])),
                "position": m.position.map(|(x, y)| json!([x, y])),
            })
        })
        .collect();
    let surfaces: Vec<Json> = state
        .surfaces()
        .into_iter()
        .map(|s| {
            let output = s
                .monitor
                .as_ref()
                .and_then(|id| monitors.iter().find(|m| m.id == *id))
                .and_then(|m| m.connector.clone());
            let backend = backend(r, s.id);
            let frames = r.frame_summary(s.id).map(|f| {
                json!({
                    "p50_us": f.p50.as_micros() as u64,
                    "p95_us": f.p95.as_micros() as u64,
                    "p99_us": f.p99.as_micros() as u64,
                    "drawn": f.drawn,
                    "unchanged": f.unchanged,
                })
            });
            json!({
                "namespace": s.namespace,
                "kind": s.kind.name(),
                "output": output,
                "size": [s.logical_size.0, s.logical_size.1],
                "scale": s.scale.as_f64(),
                "fractional": s.fractional,
                "backend": backend_name(backend),
                "promoted": promotion(backend),
                "frames": frames,
            })
        })
        .collect();
    let (status, reason, adapter) = match r.gpu_status() {
        GpuStatus::Unused => ("unused", None, None),
        GpuStatus::Starting => ("starting", None, None),
        GpuStatus::Up(a) => (
            "up",
            None,
            Some(json!({"name": a.name, "driver": a.driver, "software": a.software})),
        ),
        GpuStatus::Unavailable { reason } => ("unavailable", Some(reason), None),
    };
    json!({
        "compositor": {"globals": globals},
        "outputs": outputs,
        "surfaces": surfaces,
        "gpu": {
            "built": cfg!(feature = "gpu"),
            "status": status,
            "reason": reason,
            "adapter": adapter,
            "off": r.gpu_off(),
            "lost_devices": r.gpu_lost_devices(),
        },
    })
}

#[cfg(feature = "gpu")]
fn backend(r: &strand_render::Renderer, s: strand_scene::SurfaceId) -> Backend {
    r.backend(s)
}

#[cfg(not(feature = "gpu"))]
fn backend(_: &strand_render::Renderer, _: strand_scene::SurfaceId) -> Backend {
    Backend::Cpu
}

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Cpu => "cpu",
        Backend::GpuPresent => "gpu-present",
        Backend::GpuReadback => "gpu-readback",
    }
}

/// Why a surface is on the GPU: promotion has one rule (`promote.rs`).
fn promotion(b: Backend) -> Option<String> {
    #[cfg(feature = "gpu")]
    if b.is_gpu() {
        use strand_render::promote::{LARGE_DAMAGE, PROMOTE_AFTER};
        return Some(format!(
            "every frame damaged at least {LARGE_DAMAGE} px for over {} ms",
            PROMOTE_AFTER.as_millis()
        ));
    }
    let _ = b;
    None
}

/// Logic's half, merged into main's: the strand version, the
/// compositor's name (from the environment it gives its clients: its
/// IPC socket, else `XDG_CURRENT_DESKTOP`) and the PSS.
pub(crate) fn logic_part(mut data: Json) -> Json {
    let pss = std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .as_deref()
        .and_then(pss_kb);
    let name = compositor_name(
        strand_services::wm::detect().map(|b| b.kind().name()),
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    );
    if let Some(o) = data.as_object_mut() {
        o.insert("strand".into(), json!(env!("CARGO_PKG_VERSION")));
        o.insert("memory".into(), json!({"pss_kb": pss}));
    }
    if let Some(c) = data.get_mut("compositor").and_then(Json::as_object_mut) {
        c.insert("name".into(), json!(name));
    }
    data
}

/// The compositor's name: the one its IPC adapter knows, else the first
/// entry of `XDG_CURRENT_DESKTOP` (labwc sets `labwc:wlroots`), so a
/// compositor strand has no adapter for is still named.
fn compositor_name(detected: Option<&str>, desktop: Option<&str>) -> Option<String> {
    detected
        .or_else(|| desktop?.split(':').map(str::trim).find(|d| !d.is_empty()))
        .map(str::to_string)
}

/// The `Pss:` line of `/proc/self/smaps_rollup`, in kB.
fn pss_kb(rollup: &str) -> Option<u64> {
    rollup.lines().find_map(|l| {
        let rest = l.strip_prefix("Pss:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `strand report [--json]`.
pub fn report_cli(args: &[String]) -> Result<String, String> {
    let json = match args {
        [] => false,
        [a] if a == "--json" => true,
        _ => return Err("usage: strand report [--json]".into()),
    };
    let answer = crate::ipc::ask(&Request::Report)?;
    let report = answer.get("report").cloned().unwrap_or(Json::Null);
    Ok(if json {
        format!("{report}\n")
    } else {
        text(&report)
    })
}

/// The report as `strand report` prints it.
pub(crate) fn text(r: &Json) -> String {
    use std::fmt::Write;
    let s = |v: &Json| v.as_str().unwrap_or("?").to_string();
    let list = |v: &Json| v.as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    let _ = writeln!(out, "strand {}", s(&r["strand"]));
    let c = &r["compositor"];
    let globals = list(&c["globals"]);
    let _ = writeln!(
        out,
        "\ncompositor: {}\nglobals ({}):",
        c["name"].as_str().unwrap_or("unknown"),
        globals.len()
    );
    for g in &globals {
        let _ = writeln!(out, "  {} v{}", s(&g["interface"]), g["version"]);
    }
    let _ = writeln!(out, "\noutputs:");
    for o in list(&r["outputs"]) {
        let pair = |v: &Json, sep: &str| match v.as_array().map(Vec::as_slice) {
            Some([a, b]) => format!("{a}{sep}{b}"),
            _ => "?".into(),
        };
        let _ = writeln!(
            out,
            "  {}: {} {}, {} at {}, scale {}",
            o["name"].as_str().unwrap_or("(unnamed)"),
            s(&o["make"]),
            s(&o["model"]),
            pair(&o["size"], "x"),
            pair(&o["position"], ","),
            o["scale"]
        );
    }
    let pss = match r["memory"]["pss_kb"].as_u64() {
        Some(kb) => format!("{kb} kB"),
        None => "unknown (no /proc/self/smaps_rollup)".into(),
    };
    let _ = writeln!(out, "\nmemory: PSS {pss}");
    let g = &r["gpu"];
    let _ = write!(out, "\ngpu: {}", s(&g["status"]));
    if let Some(a) = g["adapter"].as_object() {
        let soft = if a["software"] == true {
            ", software"
        } else {
            ""
        };
        let _ = write!(out, " ({}, {}{soft})", s(&a["name"]), s(&a["driver"]));
    }
    if let Some(why) = g["reason"].as_str() {
        let _ = write!(out, ": {why}");
    }
    let _ = writeln!(out, "\n  lost devices: {}", g["lost_devices"]);
    let _ = writeln!(out, "\nsurfaces:");
    for v in list(&r["surfaces"]) {
        let size = match v["size"].as_array().map(Vec::as_slice) {
            Some([w, h]) => format!("{w}x{h}"),
            _ => "?".into(),
        };
        let _ = writeln!(
            out,
            "  {} ({}) on {}: {size} at scale {}, {}",
            s(&v["namespace"]),
            s(&v["kind"]),
            v["output"].as_str().unwrap_or("(no output)"),
            v["scale"],
            s(&v["backend"]),
        );
        if let Some(why) = v["promoted"].as_str() {
            let _ = writeln!(out, "    promoted: {why}");
        }
        let f = &v["frames"];
        if f["drawn"].as_u64().is_some_and(|n| n > 0) {
            let ms = |k: &str| f[k].as_u64().unwrap_or(0) as f64 / 1000.0;
            let _ = writeln!(
                out,
                "    frames: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms over {} drawn ({} unchanged)",
                ms("p50_us"),
                ms("p95_us"),
                ms("p99_us"),
                f["drawn"],
                f["unchanged"]
            );
        } else if f.is_object() {
            let _ = writeln!(out, "    frames: none drawn ({} unchanged)", f["unchanged"]);
        } else {
            let _ = writeln!(out, "    frames: none yet");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter's name first, then `XDG_CURRENT_DESKTOP`'s first
    /// entry; neither is null.
    #[test]
    fn the_compositor_is_named_without_an_adapter() {
        assert_eq!(
            compositor_name(Some("sway"), Some("GNOME")).as_deref(),
            Some("sway")
        );
        assert_eq!(
            compositor_name(None, Some("labwc:wlroots")).as_deref(),
            Some("labwc")
        );
        assert_eq!(compositor_name(None, Some("niri")).as_deref(), Some("niri"));
        assert_eq!(compositor_name(None, Some("")), None);
        assert_eq!(compositor_name(None, None), None);
    }

    #[test]
    fn pss_is_read_from_the_rollup() {
        let rollup = "55d0c0a00000-7ffd3e1f2000 ---p 00000000 00:00 0  [rollup]\n\
                      Rss:               45120 kB\n\
                      Pss:               31337 kB\n\
                      Pss_Dirty:         20000 kB\n";
        assert_eq!(pss_kb(rollup), Some(31337));
        assert_eq!(pss_kb("Rss: 1 kB\n"), None);
        // This process's own.
        if let Ok(t) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            assert!(pss_kb(&t).is_some_and(|kb| kb > 0), "{t}");
        }
    }

    /// Every section is in the text, from a report as main and logic
    /// make it.
    #[test]
    fn the_text_has_every_section() {
        let r = logic_part(json!({
            "compositor": {"globals": [{"interface": "wl_compositor", "version": 6}]},
            "outputs": [{"name": "DP-1", "make": "Acme", "model": "X", "scale": 1.25,
                         "size": [1536, 864], "position": [0, 0]}],
            "surfaces": [{"namespace": "strand-Top", "kind": "bar", "output": "DP-1",
                          "size": [1536, 32], "scale": 1.25, "fractional": true,
                          "backend": "gpu-present", "promoted": "because",
                          "frames": {"p50_us": 1500, "p95_us": 2000, "p99_us": 4000,
                                     "drawn": 10, "unchanged": 2}}],
            "gpu": {"built": true, "status": "up", "reason": null,
                    "adapter": {"name": "Intel", "driver": "Mesa", "software": false},
                    "off": false, "lost_devices": 1},
        }));
        let t = text(&r);
        for want in [
            "strand ",
            "compositor: ",
            "globals (1):\n  wl_compositor v6",
            "DP-1: Acme X, 1536x864 at 0,0, scale 1.25",
            "memory: PSS ",
            "gpu: up (Intel, Mesa)",
            "lost devices: 1",
            "strand-Top (bar) on DP-1: 1536x32 at scale 1.25, gpu-present",
            "promoted: because",
            "p50 1.50 ms, p95 2.00 ms, p99 4.00 ms over 10 drawn (2 unchanged)",
        ] {
            assert!(t.contains(want), "{want:?} in\n{t}");
        }
        // Paints that changed nothing, none drawn: no percentiles shown.
        let mut r = r;
        r["surfaces"][0]["frames"] =
            json!({"p50_us": 0, "p95_us": 0, "p99_us": 0, "drawn": 0, "unchanged": 3});
        let t = text(&r);
        assert!(t.contains("    frames: none drawn (3 unchanged)"), "{t}");
        assert!(!t.contains("p50 0.00"), "{t}");
    }
}

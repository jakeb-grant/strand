//! S-tools: `strand report [--json]` against `strand run` on a headless sway with two outputs, running the shell `strand
//! new` writes (the guide's hello bar, with the label the desk waits
//! on), so it also shows the new shell running on both outputs: every
//! section is there (compositor and globals with versions, outputs and
//! scales, PSS, per-surface frame timing, GPU status and lost devices),
//! the JSON round-trips, and no serials or ids are in it.

use std::time::{Duration, Instant};

use serde_json::Value as Json;

/// No key of the report names an id or serial.
fn no_ids(v: &Json, path: &str) {
    match v {
        Json::Object(m) => {
            for (k, v) in m {
                let k = k.to_lowercase();
                assert!(
                    !k.contains("id") && !k.contains("serial") && !k.contains("description"),
                    "{path}.{k}"
                );
                no_ids(v, &format!("{path}.{k}"));
            }
        }
        Json::Array(a) => a.iter().for_each(|v| no_ids(v, path)),
        _ => {}
    }
}

#[test]
fn every_section_is_reported_and_the_json_round_trips() {
    if !super::cli::tools() {
        return;
    }
    let bar = format!("{}export state label = \"hi\"\n", super::new::template());
    let desk = super::cli::Desk::start_with("report", &[("bar.strand", &bar)], &[], true);
    // A second output, at scale 2.
    let ipc = std::fs::read_dir(desk.root())
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with("sway-ipc."))
        .unwrap()
        .path();
    for args in [
        &["create_output"][..],
        &["output", "HEADLESS-2", "scale", "2"],
    ] {
        let ok = std::process::Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &ipc)
            .output()
            .unwrap();
        assert!(ok.status.success(), "swaymsg {args:?}");
    }
    // Both bars have painted frames to summarise, the second at scale 2.
    let deadline = Instant::now() + Duration::from_secs(15);
    let r = loop {
        let r = super::cli::json(&desk.ok(&["report", "--json"]));
        let painted = r["surfaces"].as_array().is_some_and(|s| {
            s.len() == 2
                && s.iter().all(|s| s["frames"]["drawn"].as_u64() > Some(0))
                && s.iter().any(|s| s["scale"] == 2.0)
        });
        if painted {
            break r;
        }
        assert!(Instant::now() < deadline, "not two painted bars: {r}");
        std::thread::sleep(Duration::from_millis(100));
    };
    // The JSON round-trips.
    let again: Json = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(again, r);
    assert_eq!(r["strand"], env!("CARGO_PKG_VERSION"));
    let c = &r["compositor"];
    assert_eq!(c["name"], "sway", "{r}");
    let globals = c["globals"].as_array().unwrap();
    for want in ["wl_compositor", "zwlr_layer_shell_v1", "wl_shm"] {
        assert!(
            globals
                .iter()
                .any(|g| g["interface"] == want && g["version"].as_u64() >= Some(1)),
            "{want} in {c}"
        );
    }
    let wl_outputs = globals.iter().filter(|g| g["interface"] == "wl_output");
    assert_eq!(wl_outputs.count(), 2, "{c}");
    let outputs = r["outputs"].as_array().unwrap();
    let named = |name: &str| {
        outputs
            .iter()
            .find(|o| o["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {r}"))
    };
    assert_eq!(outputs.len(), 2, "{r}");
    assert_eq!(named("HEADLESS-1")["scale"], 1.0);
    assert_eq!(named("HEADLESS-1")["size"], serde_json::json!([1280, 720]));
    assert_eq!(named("HEADLESS-2")["scale"], 2.0);
    assert!(r["memory"]["pss_kb"].as_u64() > Some(1000), "{r}");
    for s in r["surfaces"].as_array().unwrap() {
        assert_eq!(s["namespace"], "strand-Top");
        assert_eq!(s["kind"], "bar");
        let on = s["output"].as_str().unwrap();
        assert_eq!(s["scale"], named(on)["scale"], "{s}");
        assert_eq!(s["size"][1], 32, "{s}");
        assert_eq!(s["backend"], "cpu");
        assert_eq!(s["promoted"], Json::Null);
        let f = &s["frames"];
        let us = |k: &str| f[k].as_u64().unwrap();
        assert!(
            us("p50_us") <= us("p95_us") && us("p95_us") <= us("p99_us"),
            "{f}"
        );
        assert!(us("drawn") + us("unchanged") <= 600, "{f}");
    }
    let on: std::collections::BTreeSet<&str> = r["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["output"].as_str().unwrap())
        .collect();
    assert_eq!(
        on,
        ["HEADLESS-1", "HEADLESS-2"].into(),
        "a bar on each output"
    );
    let g = &r["gpu"];
    assert!(
        ["unused", "starting", "up", "unavailable"].contains(&g["status"].as_str().unwrap()),
        "{g}"
    );
    assert_eq!(g["lost_devices"], 0);
    if g["built"] == false {
        assert_eq!(g["reason"], "built without the GPU backend");
    }
    no_ids(&r, "report");
    // The text has every section.
    let text = desk.ok(&["report"]);
    for want in [
        "strand ",
        "compositor: ",
        "globals (",
        "  wl_compositor v",
        "outputs:\n  HEADLESS-",
        "  HEADLESS-1: ",
        "  HEADLESS-2: ",
        "scale 2",
        "memory: PSS ",
        "gpu: ",
        "lost devices: 0",
        "surfaces:\n  strand-Top (bar) on HEADLESS-",
        "strand-Top (bar) on HEADLESS-1: 1280x32 at scale 1.0, cpu",
        "frames: p50 ",
    ] {
        assert!(text.contains(want), "{want:?} in\n{text}");
    }
    // A stray argument is a usage error.
    let usage = std::process::Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(["report", "--yaml"])
        .output()
        .unwrap();
    assert!(!usage.status.success());
    let usage = String::from_utf8(usage.stderr).unwrap();
    assert!(usage.contains("usage: strand report [--json]"), "{usage}");
}

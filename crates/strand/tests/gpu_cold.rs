//! "GPU crates stay cold" (M4, docs/architecture.md, "`strand-gpu`",
//! "Budgets and tests"): the GPU backend is in every build, but wgpu,
//! vello_gpu and naga reach `strand` only through `strand-gpu` (naga
//! also through `strand-compiler`'s `shaders` check), and a build with
//! `--no-default-features` links none of them, nor libwayland-client
//! (wayland-backend's `client_system`). Checked per feature set from
//! cargo's own resolution (`cargo metadata`, `cargo tree`), not
//! `Cargo.lock`, which lists optional dependencies whatever is enabled.
//! That their code stays cold until something needs it (no Vulkan
//! library mapped before promotion) is `budgets.rs`'s and `gpu_idle.rs`'s.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

const GPU: &[&str] = &["wgpu", "wgpu-core", "wgpu-hal", "vello_gpu", "naga"];

fn cargo(args: &[&str]) -> String {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let out = Command::new(env!("CARGO"))
        .args(args)
        .args(["--offline", "--manifest-path", manifest])
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "cargo {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8")
}

/// The workspace's normal dependency graph with default features, by
/// package name.
fn default_graph() -> BTreeMap<String, BTreeSet<String>> {
    let json: serde_json::Value = serde_json::from_str(&cargo(&[
        "metadata",
        "--format-version",
        "1",
        "--filter-platform",
        "x86_64-unknown-linux-gnu",
    ]))
    .expect("cargo metadata's JSON");
    let names: BTreeMap<&str, &str> = json["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["id"].as_str()?, p["name"].as_str()?)))
        .collect();
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in json["resolve"]["nodes"].as_array().into_iter().flatten() {
        let Some(name) = node["id"].as_str().and_then(|id| names.get(id)) else {
            continue;
        };
        let deps = node["deps"].as_array().into_iter().flatten().filter(|d| {
            // Normal dependencies only (a null kind).
            d["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k["kind"].is_null())
        });
        let set = graph.entry(name.to_string()).or_default();
        for d in deps {
            if let Some(n) = d["pkg"].as_str().and_then(|id| names.get(id)) {
                set.insert(n.to_string());
            }
        }
    }
    graph
}

/// What `strand` reaches without passing through `skip`.
fn reach(graph: &BTreeMap<String, BTreeSet<String>>, skip: &[&str]) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec!["strand".to_string()];
    while let Some(n) = todo.pop() {
        if skip.contains(&n.as_str()) || !seen.insert(n.clone()) {
            continue;
        }
        todo.extend(graph.get(&n).into_iter().flatten().cloned());
    }
    seen
}

#[test]
fn gpu_crates_reach_strand_only_through_strand_gpu() {
    let graph = default_graph();
    let all = reach(&graph, &[]);
    assert!(all.contains("vello_cpu"), "strand renders with vello_cpu");
    for c in ["strand-gpu", "wgpu", "vello_gpu", "naga"] {
        assert!(all.contains(c), "the default build links {c}");
    }
    let without_gpu = reach(&graph, &["strand-gpu"]);
    for c in ["wgpu", "wgpu-core", "wgpu-hal", "vello_gpu"] {
        assert!(
            !without_gpu.contains(c),
            "{c} reaches strand other than through strand-gpu"
        );
    }
    let without_both = reach(&graph, &["strand-gpu", "strand-compiler"]);
    assert!(
        !without_both.contains("naga"),
        "naga reaches strand other than through strand-gpu and the compiler's `shaders`"
    );
}

/// `cargo tree`'s packages and their features for `strand` built with
/// `extra` flags.
fn tree(extra: &[&str]) -> Vec<(String, String)> {
    let mut args = vec![
        "tree",
        "-p",
        "strand",
        "-e",
        "normal",
        "--prefix",
        "none",
        "--format",
        "{p}|{f}",
        "--target",
        "x86_64-unknown-linux-gnu",
    ];
    args.extend_from_slice(extra);
    cargo(&args)
        .lines()
        .filter_map(|l| {
            let (p, f) = l.split_once('|')?;
            let name = p.split_whitespace().next()?.to_string();
            Some((name, f.trim_end_matches(" (*)").to_string()))
        })
        .collect()
}

#[test]
fn the_cpu_only_build_links_no_gpu_crate_nor_libwayland() {
    let cpu = tree(&["--no-default-features"]);
    assert!(cpu.iter().any(|(n, _)| n == "vello_cpu"));
    for (n, _) in &cpu {
        assert!(
            !GPU.contains(&n.as_str()) && n != "strand-gpu" && n != "raw-window-handle",
            "`--no-default-features` links {n}"
        );
    }
    for (n, f) in &cpu {
        if n == "wayland-backend" {
            assert!(
                !f.contains("client_system"),
                "`--no-default-features` links libwayland-client ({f})"
            );
        }
    }
    // The default build has all of them, libwayland's backend included.
    let gpu = tree(&[]);
    for c in ["strand-gpu", "wgpu", "vello_gpu", "naga"] {
        assert!(
            gpu.iter().any(|(n, _)| n == c),
            "the default build links {c}"
        );
    }
    assert!(
        gpu.iter()
            .any(|(n, f)| n == "wayland-backend" && f.contains("client_system")),
        "the default build presents through libwayland-client"
    );
}

//! "GPU crates stay cold" (M2): no GPU crate is linked into `strand` yet.
//! `Cargo.lock`'s dependency graph from the `strand` package (normal,
//! build and dev dependencies alike, so stricter than the binary) must
//! not reach `wgpu` or `vello_hybrid`.

use std::collections::{BTreeMap, BTreeSet};

const GPU: &[&str] = &["wgpu", "wgpu-core", "wgpu-hal", "vello_hybrid", "vello"];

#[test]
fn no_gpu_crate_reaches_strand() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
        .expect("the workspace's Cargo.lock");
    // name → the names it depends on (versions merged: stricter).
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for block in lock.split("[[package]]").skip(1) {
        let name = block
            .lines()
            .find_map(|l| l.strip_prefix("name = "))
            .map(|n| n.trim_matches('"').to_string())
            .expect("a package name");
        let mut list = BTreeSet::new();
        let mut inside = false;
        for l in block.lines() {
            let l = l.trim();
            if l.starts_with("dependencies = [") {
                inside = true;
                continue;
            }
            if inside {
                if l == "]" {
                    break;
                }
                let dep = l.trim_matches(|c| c == '"' || c == ',');
                let dep = dep.split(' ').next().unwrap_or(dep);
                list.insert(dep.to_string());
            }
        }
        deps.entry(name).or_default().extend(list);
    }
    assert!(
        deps.contains_key("strand"),
        "no strand package in Cargo.lock"
    );
    assert!(
        deps.contains_key("vello_cpu"),
        "the parse found no vello_cpu"
    );
    let mut seen = BTreeSet::new();
    let mut todo = vec![("strand".to_string(), vec!["strand".to_string()])];
    while let Some((name, path)) = todo.pop() {
        assert!(
            !GPU.contains(&name.as_str()),
            "a GPU crate is linked: {}",
            path.join(" → ")
        );
        if !seen.insert(name.clone()) {
            continue;
        }
        for d in deps.get(&name).into_iter().flatten() {
            let mut p = path.clone();
            p.push(d.clone());
            todo.push((d.clone(), p));
        }
    }
    assert!(seen.contains("vello_cpu"), "strand renders with vello_cpu");
}

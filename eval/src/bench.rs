//! `strand-eval bench` (docs/eval.md, "Cost"): per scenario, the p50,
//! p95 and max of each cost a surface's paint records, as a markdown
//! table ready for docs/benchmarks.md. The deterministic proxies (damage,
//! raster, items, records, layout, text) come first; the wall times
//! (flatten, text wait, raster) follow, marked advisory. Nothing here
//! gates: `[budget]` lines are the only cost gate, on deterministic
//! metrics, and wall times never gate (they vary with the machine).
//!
//! Each painted surface of each frame is one sample. Several runs of the
//! same scenario pool their samples, so a wall time's spread across runs
//! shows in its p95.

use std::collections::BTreeMap;

use crate::trace::{Cost, RunTrace};

/// A metric: its name and how a paint's cost gives it.
pub type Metric = (&'static str, fn(&Cost) -> u64);

/// The deterministic metrics, in the table's order.
pub const METRICS: [Metric; 8] = [
    ("damage_px", |c| c.damage_px),
    ("raster_px", |c| c.raster_px),
    ("items", |c| u64::from(c.items)),
    ("records", |c| u64::from(c.records)),
    ("layout_passes", |c| u64::from(c.layout_passes)),
    ("layout_nodes", |c| u64::from(c.layout_nodes)),
    ("text_pending", |c| u64::from(c.text_pending)),
    ("text_held", |c| u64::from(c.text_held)),
];

/// The wall times (µs), advisory.
pub const WALL: [Metric; 3] = [
    ("flatten_us", |c| c.flatten_us),
    ("text_wait_us", |c| c.text_wait_us),
    ("raster_us", |c| c.raster_us),
];

/// p50, p95 and max of `v` (nearest rank); `None` when empty.
pub fn percentiles(v: &mut [u64]) -> Option<[u64; 3]> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let rank = |p: f64| {
        let i = (p * v.len() as f64).ceil() as usize;
        v[i.clamp(1, v.len()) - 1]
    };
    Some([rank(0.50), rank(0.95), v[v.len() - 1]])
}

/// One scenario's samples: per metric, every painted surface's value.
#[derive(Debug, Default)]
pub struct Samples {
    pub paints: usize,
    pub runs: usize,
    pub values: BTreeMap<&'static str, Vec<u64>>,
}

/// Adds `trace`'s paints to `s`.
pub fn sample(s: &mut Samples, trace: &RunTrace) {
    s.runs += 1;
    for f in &trace.frames {
        for surface in &f.painted {
            let Some(c) = f.cost.get(surface) else {
                continue;
            };
            s.paints += 1;
            for (name, get) in METRICS.iter().chain(WALL.iter()) {
                s.values.entry(name).or_default().push(get(c));
            }
        }
    }
}

/// The markdown table of `scenarios` (id → samples).
pub fn table(scenarios: &BTreeMap<String, Samples>) -> String {
    let mut out =
        String::from("| scenario | metric | p50 | p95 | max |\n|---|---|---:|---:|---:|\n");
    for (id, s) in scenarios {
        let label = format!(
            "{id} ({} paints{})",
            s.paints,
            if s.runs > 1 {
                format!(", {} runs", s.runs)
            } else {
                String::new()
            }
        );
        let mut first = true;
        for (name, advisory) in METRICS
            .iter()
            .map(|(n, _)| (*n, false))
            .chain(WALL.iter().map(|(n, _)| (*n, true)))
        {
            let mut v = s.values.get(name).cloned().unwrap_or_default();
            let Some([p50, p95, max]) = percentiles(&mut v) else {
                continue;
            };
            let who = if first { label.as_str() } else { "" };
            first = false;
            let name = if advisory {
                format!("{name} (advisory)")
            } else {
                name.to_string()
            };
            out += &format!("| {who} | {name} | {p50} | {p95} | {max} |\n");
        }
        if first {
            out += &format!("| {label} | (no paints) | | | |\n");
        }
    }
    out
}

//! A/B (docs/eval.md, "A/B"): two runs of the same scenarios side by
//! side in one viewer page, nodes matched by key, and for each node the
//! first frame the two drew differently. `strand-eval ab A B` compares
//! two report directories; `scripts/eval/ab.sh` builds two revisions and
//! runs the current scenarios on both; `scripts/eval/mutants.sh` makes
//! one page per mutant (HEAD against the fix reverted).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::checks::Timeline;
use crate::report::Entry;
use crate::trace::{Row, RunTrace};

/// Beyond this a drawn edge differs (logical px).
pub const EDGE: f32 = 0.5;
/// Beyond this an opacity differs.
pub const OPACITY: f32 = 1.0 / 255.0;

/// A node is matched only by its place under its parent (no `id:` and
/// no origin): another node can take that place, so a match is a guess.
fn uncertain(t: &RunTrace, key: &str) -> bool {
    t.nodes
        .get(key)
        .is_none_or(|m| m.id.is_none() && m.origin.is_none())
}

fn differs(a: &Row, b: &Row) -> Option<(String, String, String)> {
    const AX: [&str; 4] = ["x", "y", "w", "h"];
    for (i, axis) in AX.iter().enumerate() {
        if (a.visual[i] - b.visual[i]).abs() > EDGE {
            return Some((
                (*axis).into(),
                format!("{:.2}", a.visual[i]),
                format!("{:.2}", b.visual[i]),
            ));
        }
    }
    if (a.op - b.op).abs() > OPACITY {
        return Some((
            "opacity".into(),
            format!("{:.3}", a.op),
            format!("{:.3}", b.op),
        ));
    }
    if a.hidden != b.hidden {
        return Some((
            "hidden".into(),
            a.hidden.clone().unwrap_or_else(|| "-".into()),
            b.hidden.clone().unwrap_or_else(|| "-".into()),
        ));
    }
    None
}

/// For each node either run drew, the first tick the two differ at.
pub fn diverge(a: &RunTrace, b: &RunTrace) -> Vec<Value> {
    let (ta, tb) = (Timeline::new(a), Timeline::new(b));
    let ticks: BTreeSet<u32> = ta.ks.iter().chain(tb.ks.iter()).copied().collect();
    let keys: BTreeSet<&String> = a.nodes.keys().chain(b.nodes.keys()).collect();
    let mut out = Vec::new();
    for key in keys {
        let mut found = None;
        for k in &ticks {
            let ra = ta.at(*k).and_then(|i| ta.row(i, key));
            let rb = tb.at(*k).and_then(|i| tb.row(i, key));
            let d = match (ra, rb) {
                (Some(x), Some(y)) => differs(x, y),
                (Some(_), None) => Some(("drawn".into(), "yes".into(), "no".into())),
                (None, Some(_)) => Some(("drawn".into(), "no".into(), "yes".into())),
                (None, None) => None,
            };
            if let Some(d) = d {
                found = Some((*k, d));
                break;
            }
        }
        if let Some((k, (what, va, vb))) = found {
            let meta = a.nodes.get(key).or_else(|| b.nodes.get(key));
            out.push(json!({
                "key": key,
                "label": meta.map_or(key.as_str(), |m| m.label.as_str()),
                "k": k,
                "what": what,
                "a": va,
                "b": vb,
                "uncertain": uncertain(a, key) || uncertain(b, key),
            }));
        }
    }
    out.sort_by_key(|v| v["k"].as_u64().unwrap_or(u64::MAX));
    out
}

/// The comparison a page shows: per scenario id in both runs, whether
/// the two drew the same and where each node first differs.
pub fn compare(a: &[Entry], b: &[Entry]) -> Value {
    let bs: BTreeMap<&str, &Entry> = b.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut scenarios = serde_json::Map::new();
    for ea in a {
        let Some(eb) = bs.get(ea.id.as_str()) else {
            continue;
        };
        let same = ea.trace.digest == eb.trace.digest;
        scenarios.insert(
            ea.id.clone(),
            json!({
                "same": same,
                "diverge": if same { Vec::new() } else { diverge(&ea.trace, &eb.trace) },
            }),
        );
    }
    json!({ "scenarios": scenarios })
}

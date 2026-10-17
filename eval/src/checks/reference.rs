//! `reference`: the owner's verdict on a recording
//! (`eval/refs/<id>.ref.json`). A run that reproduces one marked `bad`
//! (the same digest, or every node's path within `tol`) fails; one that
//! no longer matches a `good` one is CHANGED, a warning that never
//! gates.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Cx, Finding, Sev};
use crate::scenario::CheckId;
use crate::trace::RunTrace;

/// One point of a node's path: tick, visual x, y, w, h and opacity.
pub type PathPoint = [f32; 6];

/// A recorded verdict.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// `good` or `bad`.
    pub verdict: String,
    pub note: String,
    pub date: String,
    pub rev: String,
    pub digest: String,
    /// Each node's visual path through the run, quantised.
    pub path: BTreeMap<String, Vec<PathPoint>>,
}

impl Reference {
    pub fn load(path: &std::path::Path) -> Result<Option<Reference>, crate::Error> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t)
                .map(Some)
                .map_err(|e| crate::Error::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(crate::Error::Io(path.display().to_string(), e)),
        }
    }
}

/// Every node's visual path in `trace` (the shown frames).
pub fn paths(trace: &RunTrace) -> BTreeMap<String, Vec<PathPoint>> {
    let tl = super::Timeline::new(trace);
    let mut out: BTreeMap<String, Vec<PathPoint>> = BTreeMap::new();
    for (i, k) in tl.ks.iter().enumerate() {
        for (key, r) in &tl.screens[i] {
            if !tl.visible(i, r) {
                continue;
            }
            let p = [
                *k as f32,
                r.visual[0],
                r.visual[1],
                r.visual[2],
                r.visual[3],
                r.op,
            ];
            out.entry(key.to_string()).or_default().push(p);
        }
    }
    out
}

/// Whether two sets of paths agree within `tol` (px; opacity within
/// 1/255).
pub fn same_paths(
    a: &BTreeMap<String, Vec<PathPoint>>,
    b: &BTreeMap<String, Vec<PathPoint>>,
    tol: f64,
) -> bool {
    a.len() == b.len()
        && a.iter().all(|(k, pa)| {
            b.get(k).is_some_and(|pb| {
                pa.len() == pb.len()
                    && pa.iter().zip(pb).all(|(x, y)| {
                        x[0] == y[0]
                            && (1..5).all(|i| f64::from((x[i] - y[i]).abs()) <= tol)
                            && f64::from((x[5] - y[5]).abs()) <= 1.0 / 255.0
                    })
            })
        })
}

pub fn run(cx: &Cx, r: &Reference) -> Vec<Finding> {
    let matches = r.digest == cx.trace.digest || same_paths(&paths(cx.trace), &r.path, cx.tol);
    match (r.verdict.as_str(), matches) {
        ("bad", true) => vec![Finding::fail(
            CheckId::Reference,
            format!(
                "reproduces a recording marked bad ({}, {}): {}",
                r.rev, r.date, r.note
            ),
        )],
        ("good", false) => vec![
            Finding::fail(
                CheckId::Reference,
                format!(
                    "CHANGED: no longer the recording marked good ({}, {})",
                    r.rev, r.date
                ),
            )
            .sev(Sev::Warn),
        ],
        ("good" | "bad", _) => Vec::new(),
        (v, _) => vec![Finding::fail(
            CheckId::Reference,
            format!("the reference's verdict is `{v}`, not good or bad"),
        )],
    }
}

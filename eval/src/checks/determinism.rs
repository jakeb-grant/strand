//! `determinism`: the scenario run twice draws the same (equal digests);
//! a difference names the first tick it shows at.

use super::Finding;
use crate::scenario::CheckId;
use crate::trace::RunTrace;

pub fn run(a: &RunTrace, b: &RunTrace) -> Vec<Finding> {
    if a.digest == b.digest {
        return Vec::new();
    }
    let first = a
        .frames
        .iter()
        .zip(&b.frames)
        .find(|(x, y)| x.k != y.k || x.n != y.n || x.painted != y.painted)
        .map(|(x, _)| x.k)
        .or_else(|| {
            let n = a.frames.len().min(b.frames.len());
            a.frames.get(n).or(b.frames.get(n)).map(|f| f.k)
        });
    let f = Finding::fail(
        CheckId::Determinism,
        format!("two runs differ ({} and {})", a.digest, b.digest),
    );
    vec![match first {
        Some(k) => f.at(k),
        None => f,
    }]
}

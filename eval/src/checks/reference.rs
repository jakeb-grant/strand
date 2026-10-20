//! `reference`: the run against its reference recordings
//! (`eval/refs/`, docs/eval.md "Reference recordings"). A run that leaves
//! its approved recording beyond a quantum is CHANGED, naming the first
//! tick, node and field that differ; a run with no approved recording is
//! CHANGED where one is required (`check --gate`, the matrix) and a skip
//! elsewhere; a run that comes back to a recording marked bad (within
//! `tol` on geometry) fails.

use super::{Cx, Finding, Sev, Status};
use crate::recording::{self, Recording, Tol};
use crate::refs::{self, Refs};
use crate::scenario::CheckId;
use crate::trace::RunTrace;

/// What a run without an approved reference says.
pub const MISSING: &str = "no approved reference";

/// Why a `--reduced-motion` run is not compared (and `approve` refuses
/// it).
pub const REDUCED: &str = "a --reduced-motion run is not what the references record";

/// Per-node lines a CHANGED finding carries besides its first.
pub const PER_NODE: usize = 8;

/// The findings, and the status they make when it is not what the
/// findings say (CHANGED, a skip). `raw` is the run's trace as run (not
/// moved by its surfaces: the recording keeps their origins and poses).
pub fn run(cx: &Cx, raw: &RunTrace, r: &Refs) -> (Vec<Finding>, Option<Status>) {
    if let Some(e) = &r.error {
        return (
            vec![Finding::fail(
                CheckId::Reference,
                format!("{} could not be read: {e}", refs::shown(&r.file)),
            )],
            None,
        );
    }
    let rec = Recording::from_trace(raw);
    let mut out = Vec::new();
    let mut bad = false;
    for b in &r.bad {
        match b.recording() {
            Ok(x) => {
                if recording::diff(&x, &rec, Tol::px(cx.tol)).is_none() {
                    bad = true;
                    out.push(Finding::fail(
                        CheckId::Reference,
                        format!("reproduces a known-bad recording: {}", b.describe()),
                    ));
                }
            }
            Err(e) => out.push(Finding::fail(
                CheckId::Reference,
                format!("{}: {e}", refs::shown(&r.file)),
            )),
        }
    }
    let changed = |out: Vec<Finding>, bad: bool| {
        let st = (!bad).then_some(Status::Changed);
        (out, st)
    };
    let Some(good) = &r.good else {
        if r.required {
            out.push(Finding::fail(
                CheckId::Reference,
                format!("{MISSING} ({})", refs::shown(&r.file)),
            ));
            return changed(out, bad);
        }
        if out.is_empty() {
            return (
                out,
                Some(Status::Skip(format!(
                    "{MISSING} (strand-eval approve writes {})",
                    refs::shown(&r.file)
                ))),
            );
        }
        return (out, None);
    };
    let reference = match good.recording() {
        Ok(x) => x,
        Err(e) => {
            out.push(Finding::fail(
                CheckId::Reference,
                format!("{}: {e}", refs::shown(&r.file)),
            ));
            return (out, None);
        }
    };
    let Some(d) = recording::diff(&reference, &rec, Tol::QUANTUM) else {
        return (out, None);
    };
    // (eval-rules-8) The largest step the run takes that the reference
    // does not leads, when it is not the first difference itself: a
    // 3 px snap a reviewer must see, before the 0.14 px drift that
    // started it.
    let lead = d
        .largest
        .as_ref()
        .filter(|l| l.at.k != d.first.k || l.at.key != d.first.key || l.at.field != d.first.field);
    let mut msg = match lead {
        Some(l) => format!(
            "largest step {}; first {}; {} (reference {})",
            l.line(),
            d.first.line(),
            d.counts(),
            good.describe()
        ),
        None => format!(
            "{}; {} (reference {})",
            d.first.line(),
            d.counts(),
            good.describe()
        ),
    };
    if good.scenario != refs::source_hash(&cx.sc.source) {
        msg += "; the scenario's text changed since it was approved";
    }
    let finding = |c: &recording::Change, msg: String, sev: Sev| {
        let mut f = Finding::fail(CheckId::Reference, msg).sev(sev).at(c.k);
        f.axis = Some(c.field.clone());
        if let Some(k) = &c.key {
            f = f.node(k);
        }
        f
    };
    out.push(finding(lead.map_or(&d.first, |l| &l.at), msg, Sev::Fail));
    // Each other node's first difference, the largest first.
    for c in d
        .per_node
        .iter()
        .filter(|c| (&c.key, &c.label) != (&d.first.key, &d.first.label))
        .take(PER_NODE)
    {
        out.push(finding(c, c.line(), Sev::Info));
    }
    changed(out, bad)
}

//! The checks (docs/eval.md, "Checks"; docs/decisions.md eval-sandbox):
//! invariants over a run's trace, per segment, that hold for any correct
//! animation and that bugs 24 and 25 broke.
//!
//! A segment runs from a step that changes something to the next rest;
//! `s` is the tick before its first change, `e` its rest. A node is a
//! *mover* on an axis when its laid box differs there between `s` and `e`
//! (beyond `tol`), else a *bystander*; authored motion (springs of what
//! logic set, poses, keyframes, morphs, lean, jelly, drags) is exempt per
//! axis, derived motion (glides, layout, text arriving) is held to an
//! envelope. `intend` lines and `[allow]` entries exempt a node and its
//! subtree.
//!
//! Every check reads only the trace, so synthetic traces test them
//! (eval/tests/checks.rs).

use std::collections::{BTreeMap, HashMap, HashSet};

use strand_render::probe::{Axes, Motions};

use crate::scenario::Scenario;
use crate::select::Selector;
use crate::steps::Axis;
use crate::trace::{Frame, Row, RunTrace};

pub mod budget;
pub mod bystander;
pub mod coherence;
pub mod cpu_gpu;
pub mod determinism;
pub mod envelope;
pub mod expect;
pub mod ink;
pub mod reference;
pub mod regions;
pub mod settle;
pub mod teleport;
pub mod twin;
pub mod vanish;

pub use crate::scenario::CheckId;
pub use reference::Reference;

/// How bad a finding is.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Sev {
    /// Fails the check.
    Fail,
    /// Worth a look; never gates (a `good` reference that changed).
    Warn,
    /// Context (a pure layout jump).
    Info,
}

/// One thing a check found.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub check: String,
    pub sev: Sev,
    /// The tick it happened at.
    pub k: Option<u32>,
    pub key: Option<String>,
    pub axis: Option<String>,
    pub got: Option<f64>,
    pub bound: Option<[f64; 2]>,
    pub msg: String,
    /// The segment's name.
    pub segment: Option<String>,
    /// More ticks where the same node and axis broke it.
    pub more: u32,
}

impl Finding {
    pub fn fail(check: CheckId, msg: impl Into<String>) -> Finding {
        Finding {
            check: check.name().to_string(),
            sev: Sev::Fail,
            k: None,
            key: None,
            axis: None,
            got: None,
            bound: None,
            msg: msg.into(),
            segment: None,
            more: 0,
        }
    }

    pub fn sev(mut self, sev: Sev) -> Finding {
        self.sev = sev;
        self
    }

    pub fn at(mut self, k: u32) -> Finding {
        self.k = Some(k);
        self
    }

    pub fn node(mut self, key: &str) -> Finding {
        self.key = Some(key.to_string());
        self
    }

    pub fn axis(mut self, axis: &str) -> Finding {
        self.axis = Some(axis.to_string());
        self
    }

    pub fn got(mut self, got: f64, bound: [f64; 2]) -> Finding {
        self.got = Some(got);
        self.bound = Some(bound);
        self
    }

    pub fn in_segment(mut self, name: &str) -> Finding {
        self.segment = Some(name.to_string());
        self
    }
}

/// A check's outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase", tag = "status", content = "why")]
pub enum Status {
    Pass,
    Fail,
    /// Failed as `expect_fail` says it does.
    Xfail(String),
    /// Passed although `expect_fail` says it fails: the gate fails.
    Xpass(String),
    /// A `good` reference no longer matches (never gates).
    Changed,
    /// Not run, and why.
    Skip(String),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckResult {
    pub check: String,
    pub status: Status,
    pub findings: Vec<Finding>,
}

/// A scenario's checks.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub id: String,
    pub results: Vec<CheckResult>,
}

impl Report {
    /// Whether the gate passes: no check failed and none passed against
    /// its `expect_fail`.
    pub fn ok(&self) -> bool {
        self.results
            .iter()
            .all(|r| !matches!(r.status, Status::Fail | Status::Xpass(_)))
    }

    pub fn status(&self, check: CheckId) -> Option<&Status> {
        self.results
            .iter()
            .find(|r| r.check == check.name())
            .map(|r| &r.status)
    }

    /// The failing findings of `check`.
    pub fn failures(&self, check: CheckId) -> Vec<&Finding> {
        self.results
            .iter()
            .filter(|r| r.check == check.name())
            .flat_map(|r| r.findings.iter())
            .filter(|f| f.sev == Sev::Fail)
            .collect()
    }

    /// One line per check and per failing finding.
    pub fn lines(&self, trace: &RunTrace) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.results {
            let status = match &r.status {
                Status::Pass => "pass".to_string(),
                Status::Fail => "FAIL".to_string(),
                Status::Xfail(why) => format!("XFAIL ({why})"),
                Status::Xpass(why) => format!("XPASS: now passes, remove expect_fail ({why})"),
                Status::Changed => "CHANGED".to_string(),
                Status::Skip(why) => format!("skip ({why})"),
            };
            out.push(format!("{} {}: {status}", self.id, r.check));
            for f in r.findings.iter().filter(|f| f.sev != Sev::Info) {
                out.push(format!("  {}", describe(f, trace)));
            }
        }
        out
    }
}

/// A finding as one line: check, node label and origin, tick, axis,
/// value against its bound, what it means.
pub fn describe(f: &Finding, trace: &RunTrace) -> String {
    let mut s = f.check.clone();
    if let Some(seg) = &f.segment {
        s += &format!(" [{seg}]");
    }
    if let Some(k) = &f.key {
        let meta = trace.nodes.get(k);
        let label = meta.map_or(k.as_str(), |m| m.label.as_str());
        s += &format!(" {label}");
        if let Some(o) = meta.and_then(|m| m.origin.as_deref()) {
            s += &format!(" ({o})");
        }
    }
    if let Some(k) = f.k {
        s += &format!(" k{k}");
    }
    if let Some(a) = &f.axis {
        s += &format!(" {a}");
    }
    if let (Some(g), Some(b)) = (f.got, f.bound) {
        s += &format!(" {g:.2} not in [{:.2}, {:.2}]", b[0], b[1]);
    }
    if f.more > 0 {
        s += &format!(" (+{} ticks)", f.more);
    }
    s += &format!(": {}", f.msg);
    s
}

/// What is on screen at each painted tick: the rows of every surface
/// shown, a surface not painted at a tick keeping its last rows, and at
/// each tick a surface was detached at, without its rows.
#[derive(Debug)]
pub struct Timeline<'a> {
    pub ks: Vec<u32>,
    pub screens: Vec<HashMap<&'a str, &'a Row>>,
    pub frames: Vec<&'a Frame>,
    /// Where each surface was first placed, when the rows are moved
    /// with their surface ([`RunTrace::on_screen`]); else empty.
    first: BTreeMap<u32, [f32; 2]>,
}

impl<'a> Timeline<'a> {
    pub fn new(trace: &'a RunTrace) -> Timeline<'a> {
        let mut ks = Vec::new();
        let mut screens = Vec::new();
        let mut frames = Vec::new();
        let mut cur: HashMap<&'a str, &'a Row> = HashMap::new();
        // A surface detached shows nothing from then on, painted after
        // or not (a popup closed by its grace with nothing else to draw):
        // an entry at the tick it went, the frame before it standing in.
        let mut gone: Vec<(u32, u32)> = trace
            .surfaces
            .iter()
            .filter_map(|s| Some((s.detached?, s.id)))
            .collect();
        gone.sort_unstable();
        let mut gone = gone.into_iter().peekable();
        let detach = |d: u32,
                      sid: u32,
                      cur: &mut HashMap<&'a str, &'a Row>,
                      ks: &mut Vec<u32>,
                      screens: &mut Vec<HashMap<&'a str, &'a Row>>,
                      frames: &mut Vec<&'a Frame>| {
            if let Some(last) = frames.last().copied() {
                cur.retain(|_, r| r.surface != sid);
                ks.push(d);
                screens.push(cur.clone());
                frames.push(last);
            }
        };
        for f in &trace.frames {
            while let Some((d, sid)) = gone.next_if(|(d, _)| *d < f.k) {
                detach(d, sid, &mut cur, &mut ks, &mut screens, &mut frames);
            }
            cur.retain(|_, r| f.sizes.contains_key(&r.surface) && !f.painted.contains(&r.surface));
            for r in &f.n {
                cur.insert(r.key.as_str(), r);
            }
            ks.push(f.k);
            screens.push(cur.clone());
            frames.push(f);
        }
        for (d, sid) in gone {
            detach(d, sid, &mut cur, &mut ks, &mut screens, &mut frames);
        }
        Timeline {
            ks,
            screens,
            frames,
            first: if trace.on_screen {
                trace.first_origins()
            } else {
                BTreeMap::new()
            },
        }
    }

    /// The frame showing at tick `k` (the last painted at or before it).
    pub fn at(&self, k: u32) -> Option<usize> {
        match self.ks.partition_point(|x| *x <= k) {
            0 => None,
            i => Some(i - 1),
        }
    }

    /// The frames painted in `(s, e]`.
    pub fn within(&self, s: Option<u32>, e: u32) -> std::ops::Range<usize> {
        let lo = match s {
            Some(s) => self.ks.partition_point(|x| *x <= s),
            None => 0,
        };
        let hi = self.ks.partition_point(|x| *x <= e);
        lo..hi.max(lo)
    }

    pub fn row(&self, i: usize, key: &str) -> Option<&'a Row> {
        self.screens.get(i)?.get(key).copied()
    }

    /// The logical size of `surface` at frame `i`.
    pub fn size(&self, i: usize, surface: u32) -> Option<[f32; 2]> {
        self.frames.get(i)?.sizes.get(&surface).copied()
    }

    /// Whether `row` shows at frame `i` (docs/eval.md: present, not
    /// hidden, opacity at least 0.02, a visual area of at least one
    /// pixel, on its surface). An exiting ghost (`inert`) shows.
    pub fn visible(&self, i: usize, row: &Row) -> bool {
        let hidden = matches!(row.hidden.as_deref(), Some("opacity0" | "scale0"));
        let [x, y, w, h] = row.visual;
        // On its surface, wherever the surface has moved to.
        let [dx, dy] = self
            .frames
            .get(i)
            .map_or([0.0, 0.0], |f| RunTrace::moved(&self.first, f, row.surface));
        let (x, y) = (x - dx, y - dy);
        let on = self
            .size(i, row.surface)
            .is_none_or(|[sw, sh]| x < sw && y < sh && x + w > 0.0 && y + h > 0.0);
        !hidden && row.op >= 0.02 && w * h >= 1.0 && on
    }

    pub fn visible_key(&self, i: usize, key: &str) -> bool {
        self.row(i, key).is_some_and(|r| self.visible(i, r))
    }
}

/// An axis's value in a row.
pub fn value(r: &Row, a: Axis) -> f64 {
    f64::from(match a {
        Axis::X => r.visual[0],
        Axis::Y => r.visual[1],
        Axis::W => r.visual[2],
        Axis::H => r.visual[3],
        Axis::Opacity => r.op,
        Axis::Scale => r.scale[0].max(r.scale[1]),
    })
}

/// An axis's laid value (opacity and scale: their drawn values).
pub fn laid(r: &Row, a: Axis) -> f64 {
    f64::from(match a {
        Axis::X => r.laid[0],
        Axis::Y => r.laid[1],
        Axis::W => r.laid[2],
        Axis::H => r.laid[3],
        Axis::Opacity => r.op,
        Axis::Scale => r.scale[0].max(r.scale[1]),
    })
}

pub const GEOMETRY: [Axis; 4] = [Axis::X, Axis::Y, Axis::W, Axis::H];

/// Where layout and authoring put a node on a geometry axis over a
/// segment: whether it is a *bystander* there, and the way a mover's
/// derived motion may take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Way {
    /// Drawn at the same place at the segment's start and rest, laid at
    /// the same place at both, and laid elsewhere on no frame between for
    /// a reason ([`way`]).
    pub bystander: bool,
    /// The lowest and highest place it may be drawn at, before any
    /// overshoot: its start and rest, and the laid place of every frame
    /// counted, carried by the offset it rests at (written offsets,
    /// transforms).
    pub lo: f64,
    pub hi: f64,
}

/// What on a surface can lay a node out elsewhere for a while and back
/// with no change from logic or input: an exiting ghost holding its slot
/// (`EXIT`, an inert node), a size that springs past its target, a
/// keyframe or time-driven prop that places, a drag lifting an item out
/// of its list, a text drawn by its estimate or held.
pub const LAYOUT_CAUSES: Motions = Motions(
    Motions::EXIT.0
        | Motions::SIZE_SPRING.0
        | Motions::KEYFRAMES.0
        | Motions::TIME.0
        | Motions::LIFT.0,
);

/// `key`'s [`Way`] on `a` over the frames `(s, e_k]` (`b` its row at
/// `s`, `e` at the rest).
///
/// A node laid out at the same place at `s` and at `e` can be laid
/// elsewhere between and come back (an *excursion*). One with a reason
/// makes it a mover held to the layouts drawn: something on its surface
/// at one of its frames can lay out anew with no change from logic
/// ([`LAYOUT_CAUSES`]: an exiting ghost holding its slot, a text drawn by
/// its estimate before it is shaped), or logic's diff or input reached
/// the renderer at the frame it came back on (a second step of the
/// segment, a container query's answer, the pointer leaving what it
/// hovered). An excursion with no reason is a layout drawn wrong: it
/// neither makes the node a mover nor widens its way, so `bystander` or
/// `envelope` holds the node to where it belongs. The same holds for a
/// mover's excursions past its start and rest. A node drawn elsewhere at
/// rest though laid at the same place (a written `x:` offset that
/// changed) is a mover too. Only the layouts of frames drawn count: a
/// layout pass a frame threw away (the estimate's, before the shaped
/// text's in the same frame) never does.
pub fn way(cx: &Cx, key: &str, s: u32, e_k: u32, b: &Row, e: &Row, a: Axis) -> Way {
    let (vs, ve) = (value(b, a), value(e, a));
    let carry = ve - laid(e, a);
    let (mut lo, mut hi) = (vs.min(ve), vs.max(ve));
    let (lb, le) = (laid(b, a), laid(e, a));
    let (span_lo, span_hi) = (lb.min(le) - cx.tol, lb.max(le) + cx.tol);
    let mut laid_moved = (lb - le).abs() > cx.tol;
    // The excursion open: its laid places, and whether it has a reason.
    let mut run: Vec<f64> = Vec::new();
    let mut why = false;
    let take = |run: &mut Vec<f64>, lo: &mut f64, hi: &mut f64, moved: &mut bool| {
        for at in run.drain(..) {
            *lo = lo.min(at + carry);
            *hi = hi.max(at + carry);
            *moved = true;
        }
    };
    for i in cx.tl.within(Some(s), e_k) {
        let Some(r) = cx.tl.row(i, key) else { continue };
        let l = laid(r, a);
        if l < span_lo || l > span_hi {
            run.push(l);
            why |= cx.lays_out(i, r.surface);
            continue;
        }
        if !run.is_empty() {
            if why || cx.tl.frames.get(i).is_some_and(|f| f.driven) {
                take(&mut run, &mut lo, &mut hi, &mut laid_moved);
            }
            run.clear();
            why = false;
        }
        if (l - lb).abs() > cx.tol {
            laid_moved = true;
        }
        lo = lo.min(l + carry);
        hi = hi.max(l + carry);
    }
    if why {
        take(&mut run, &mut lo, &mut hi, &mut laid_moved);
    }
    Way {
        bystander: !laid_moved && (vs - ve).abs() <= cx.tol,
        lo,
        hi,
    }
}

pub fn authored(r: &Row, a: Axis) -> bool {
    Axes(r.auth).intersects(a.bits())
}

pub fn motions(r: &Row) -> Motions {
    Motions(r.emot)
}

/// What a check reads: the scenario, its trace and the exemptions.
#[derive(Debug)]
pub struct Cx<'a> {
    pub sc: &'a Scenario,
    pub trace: &'a RunTrace,
    pub tl: Timeline<'a>,
    /// An edge's give: `edge_px`, and at least one physical pixel.
    pub tol: f64,
    /// `[allow]` per check: the keys named.
    allow: HashMap<CheckId, HashSet<String>>,
    /// Allow entries that named nothing.
    pub unmatched_allows: Vec<(CheckId, String)>,
    parents: HashMap<&'a str, &'a str>,
    /// Per frame of the timeline, the surfaces with something on them
    /// that lays out anew by itself ([`LAYOUT_CAUSES`]).
    causes: Vec<HashSet<u32>>,
}

impl<'a> Cx<'a> {
    pub fn new(sc: &'a Scenario, trace: &'a RunTrace) -> Cx<'a> {
        // The coarsest physical pixel any frame was drawn at (a `scale`
        // step can change it mid-run).
        let scale = if trace.frames.is_empty() && trace.scenario.scale <= 0.0 {
            sc.screen.scale
        } else {
            trace.min_scale()
        };
        let tol = sc.tolerance.edge_px.max(1.0 / scale.max(0.25));
        let mut allow: HashMap<CheckId, HashSet<String>> = HashMap::new();
        let mut unmatched = Vec::new();
        for (check, list) in &sc.allow {
            for a in list {
                let keys = select_seen(trace, &a.node);
                if keys.is_empty() {
                    unmatched.push((*check, a.node.to_string()));
                }
                allow.entry(*check).or_default().extend(keys);
            }
        }
        let parents = trace
            .nodes
            .iter()
            .filter_map(|(k, m)| Some((k.as_str(), m.parent.as_deref()?)))
            .collect();
        let tl = Timeline::new(trace);
        let causes = tl
            .screens
            .iter()
            .map(|rows| {
                rows.values()
                    .filter(|r| {
                        Motions(r.mot).intersects(LAYOUT_CAUSES)
                            || r.hidden.as_deref() == Some("inert")
                            || matches!(r.text.as_deref(), Some("E" | "I"))
                    })
                    .map(|r| r.surface)
                    .collect()
            })
            .collect();
        Cx {
            sc,
            trace,
            tl,
            tol,
            allow,
            unmatched_allows: unmatched,
            parents,
            causes,
        }
    }

    /// Something on `surface` at frame `i` lays out anew by itself.
    pub fn lays_out(&self, i: usize, surface: u32) -> bool {
        self.causes.get(i).is_some_and(|c| c.contains(&surface))
    }

    /// `key` and its ancestors.
    pub fn lineage(&self, key: &'a str) -> Vec<&'a str> {
        let mut out = vec![key];
        let mut at = key;
        while let Some(p) = self.parents.get(at) {
            if out.contains(p) {
                break;
            }
            out.push(p);
            at = p;
        }
        out
    }

    /// `check` leaves `key`'s `axes` alone in segment `seg`: an `[allow]`
    /// entry, or an `intend` of the segment, names it or an ancestor.
    pub fn exempt(&self, check: CheckId, seg: usize, key: &'a str, axes: Axes) -> bool {
        let line = self.lineage(key);
        if let Some(set) = self.allow.get(&check)
            && line.iter().any(|k| set.contains(*k))
        {
            return true;
        }
        self.trace.segments.get(seg).is_some_and(|s| {
            s.intends.iter().any(|i| {
                Axes(i.axes).intersects(axes) && line.iter().any(|k| i.keys.iter().any(|x| x == k))
            })
        })
    }

    /// The keys present and visible at frame `i`.
    pub fn visible_keys(&self, i: usize) -> Vec<&'a str> {
        let mut v: Vec<&'a str> = self
            .tl
            .screens
            .get(i)
            .map(|s| {
                s.iter()
                    .filter(|(_, r)| self.tl.visible(i, r))
                    .map(|(k, _)| *k)
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// The segments to check motion in: those with a frame before them.
    pub fn segments(&self) -> impl Iterator<Item = (usize, &'a crate::trace::Segment, u32)> + '_ {
        self.trace
            .segments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, s, s.s?)))
            .filter(|(_, s, start)| s.e > *start)
    }
}

/// The keys of nodes a run saw that `sel` names (by id, origin or text).
pub fn select_seen(trace: &RunTrace, sel: &Selector) -> Vec<String> {
    trace
        .nodes
        .iter()
        .filter(|(_, m)| match sel {
            Selector::Id(id) => m.id.as_deref() == Some(id),
            Selector::Text(t) => m.text.as_deref() == Some(t),
            Selector::Origin {
                file, line, col, ..
            } => m.origin.as_deref().is_some_and(|o| {
                let mut it = o.rsplitn(3, ':');
                let (c, l, f) = (it.next(), it.next(), it.next());
                f.is_some_and(|f| f == file || f.ends_with(&format!("/{file}")))
                    && l == Some(&line.to_string())
                    && col.is_none_or(|col| c == Some(&col.to_string()))
            }),
            Selector::Path(_) => false,
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// What makes two findings the same: node, axis, segment and the kind
/// of finding (its message up to the first colon).
type FindingId = (Option<String>, Option<String>, Option<String>, String);

/// Collapses findings of one node, axis and segment into the first, with
/// a count of the rest.
pub fn first_per_node(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen: BTreeMap<FindingId, usize> = BTreeMap::new();
    let mut out: Vec<Finding> = Vec::new();
    for f in findings {
        let id = (
            f.key.clone(),
            f.axis.clone(),
            f.segment.clone(),
            f.msg.split(':').next().unwrap_or("").to_string(),
        );
        match seen.get(&id) {
            Some(i) => out[*i].more += 1,
            None => {
                seen.insert(id, out.len());
                out.push(f);
            }
        }
    }
    out
}

/// Runs the trace checks `sc` asks for. `twin` is the reduced-motion
/// re-run, `again` a second run for determinism, `reference` the
/// scenario's recorded verdict.
pub fn evaluate(
    sc: &Scenario,
    trace: &RunTrace,
    twin: Option<&RunTrace>,
    again: Option<&RunTrace>,
    reference: Option<&Reference>,
) -> Report {
    // The checks read what the eye sees: rows moved with their surface.
    let moved = trace.on_screen();
    let trace = moved.as_ref().unwrap_or(trace);
    let twin_moved = twin.and_then(RunTrace::on_screen);
    let twin = twin_moved.as_ref().or(twin);
    let cx = Cx::new(sc, trace);
    let mut results = Vec::new();
    for check in CheckId::ALL {
        let skip = |why: &str| Some(Status::Skip(why.to_string()));
        let (findings, skipped) = if !sc.runs(check) && check != CheckId::Determinism {
            (Vec::new(), skip("not asked for"))
        } else {
            match check {
                CheckId::Settle => (settle::run(&cx), None),
                CheckId::Bystander => (bystander::run(&cx), None),
                CheckId::Envelope => (envelope::run(&cx), None),
                CheckId::Teleport if trace.scenario.reduced_motion => {
                    (Vec::new(), skip("motion is reduced: everything snaps"))
                }
                CheckId::Teleport => (teleport::run(&cx), None),
                CheckId::Vanish => (vanish::run(&cx), None),
                CheckId::Coherence => (coherence::run(&cx), None),
                CheckId::Ink => (ink::run(&cx), None),
                CheckId::Twin if trace.scenario.reduced_motion => {
                    (Vec::new(), skip("the scenario reduces motion itself"))
                }
                CheckId::Twin => match twin {
                    Some(t) => (twin::run(&cx, t), None),
                    None => (Vec::new(), skip("no twin run")),
                },
                CheckId::Determinism => match again {
                    Some(a) => (determinism::run(trace, a), None),
                    None => (Vec::new(), skip("one run (curated scenarios run twice)")),
                },
                CheckId::Budget if sc.budget.is_empty() => (Vec::new(), skip("no [budget]")),
                CheckId::Budget => (budget::run(&cx), None),
                CheckId::Reference => match reference {
                    Some(r) => (reference::run(&cx, r), None),
                    None => (Vec::new(), skip("no reference recorded")),
                },
                CheckId::Expect if trace.expects.is_empty() => {
                    (Vec::new(), skip("no expect steps"))
                }
                CheckId::Expect => (expect::run(&cx), None),
                CheckId::Regions => (regions::run(&cx), None),
                // `check_scenario` runs the GPU leg and replaces this.
                CheckId::CpuGpu => (Vec::new(), skip("no GPU run to compare with")),
            }
        };
        let mut findings = findings;
        for (c, sel) in &cx.unmatched_allows {
            if *c == check {
                findings.push(Finding::fail(
                    check,
                    format!("[allow] {} names no node the run saw", sel),
                ));
            }
        }
        let status = status(sc, check, &findings, skipped);
        results.push(CheckResult {
            check: check.name().to_string(),
            status,
            findings,
        });
    }
    Report {
        id: sc.id.clone(),
        results,
    }
}

/// A check's status from its findings: skipped, or failed, passed or
/// changed, turned XFAIL or XPASS by the scenario's `expect_fail`.
pub fn status(
    sc: &Scenario,
    check: CheckId,
    findings: &[Finding],
    skipped: Option<Status>,
) -> Status {
    let failed = findings.iter().any(|f| f.sev == Sev::Fail);
    let changed = findings.iter().any(|f| f.sev == Sev::Warn);
    match (skipped, sc.expect_fail.get(&check)) {
        (Some(s), _) => s,
        (None, Some(why)) if failed => Status::Xfail(why.clone()),
        (None, Some(why)) => Status::Xpass(why.clone()),
        (None, None) if failed => Status::Fail,
        (None, None) if changed => Status::Changed,
        (None, None) => Status::Pass,
    }
}

impl Report {
    /// `check`'s result replaced by `findings` (or a skip), as
    /// [`evaluate`] would have judged them: for the checks that need more
    /// than the trace (`cpu-gpu`'s second run).
    pub fn set(
        &mut self,
        sc: &Scenario,
        check: CheckId,
        findings: Vec<Finding>,
        skipped: Option<Status>,
    ) {
        let status = status(sc, check, &findings, skipped);
        let r = CheckResult {
            check: check.name().to_string(),
            status,
            findings,
        };
        match self.results.iter_mut().find(|x| x.check == r.check) {
            Some(x) => *x = r,
            None => self.results.push(r),
        }
    }
}

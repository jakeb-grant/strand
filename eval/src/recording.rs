//! A run's recording (docs/eval.md, "Reference recordings"): what each
//! painted tick showed, quantised, in a compact text that diffs line by
//! line. Per tick: the surfaces painted; per surface shown its size,
//! where it lies on the output, the pose the compositor applies and its
//! input region; per node on screen its drawn and visual boxes, effective
//! opacity, scale, rotation, hidden reason, text state, place in its
//! surface's paint order, and for a text its glyph ink and the width it
//! was cut to. A surface not
//! painted at a tick keeps what it showed (as `checks::Timeline` reads a
//! trace).
//!
//! The quanta: boxes, origins and offsets 1/64 logical px, opacity 1/255,
//! scale 1/1000, rotation 1/100 degree. [`diff`] holds two recordings to
//! one quantum per value ([`Tol::QUANTUM`]), so a last-bit difference
//! that rounds the other way never counts, and anything else does.
//!
//! The text (one tick's changes per line group; [`encode`], [`decode`]):
//!
//! ```text
//! S1 0 - "bar Top"                    surface 1: attached at k0, never detached
//! N0 "<key>" "box#after"              node 0: its key and label
//! k0 p1                               tick 0, surface 1 painted
//! s1 z=420,60                         surface 1: its size (o= origin, P= pose, i= input)
//! n0 x=82 y=10 w=20 h=12 txt=S        node 0: its drawn box, its text shaped
//! k1                                  tick 1, the same surfaces painted
//! n0 x=83                             only what changed since its last line
//! n3 -                                node 3 left the screen
//! ```
//!
//! A node's first line names everything not at its default (`srf=` the
//! first surface listed, `v=d op=255 sc=1,1 rot=0 hid=- txt=- ord=0
//! ink=- fit=-`); `v=d` is a visual box equal to the drawn one, else
//! `v=x,y,w,h`. `ord` is the node's place in its surface's paint order
//! (0 first), `ink=x,y,w,h` a text's glyphs as drawn, `fit` the width a
//! cut text was shaped for.

use std::collections::{BTreeMap, BTreeSet};

use crate::trace::{Frame, NodeMeta, PoseRec, Regions, Row, RunTrace, SurfaceInfo};

/// Quanta per unit: logical px, opacity, scale, degrees.
pub const PX: f64 = 64.0;
pub const OP: f64 = 255.0;
pub const SCALE: f64 = 1000.0;
pub const DEG: f64 = 100.0;

/// A quantised value past this is clamped to it, so no difference of two
/// values overflows and none reaches the sentinels below.
const BOUND: i64 = 1 << 52;
/// What a value that is not a number quantises to: each never within any
/// tolerance of a finite value or of the others, so a NaN or an infinity
/// in a run always differs from a reference that has a number there.
const NAN: i64 = -(1 << 60);
const INF: i64 = 1 << 58;
const NEG_INF: i64 = -(1 << 58);

fn qz(v: f64, per: f64) -> i64 {
    let x = (v * per).round();
    if x.is_nan() {
        NAN
    } else if x == f64::INFINITY {
        if v.is_finite() { BOUND } else { INF }
    } else if x == f64::NEG_INFINITY {
        if v.is_finite() { -BOUND } else { NEG_INF }
    } else {
        (x as i64).clamp(-BOUND, BOUND)
    }
}

/// A quantised value as decimal text (`per` 64: exact in six digits); a
/// value that was not a number as `nan`, `inf` or `-inf`.
fn dec(v: i64, per: f64) -> String {
    match v {
        NAN => return "nan".into(),
        INF => return "inf".into(),
        NEG_INF => return "-inf".into(),
        _ => {}
    }
    let per_i = per as i64;
    if v % per_i == 0 {
        return (v / per_i).to_string();
    }
    let s = format!("{:.6}", v as f64 / per);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn undec(s: &str, per: f64) -> Option<i64> {
    let v: f64 = s.parse().ok()?;
    Some(qz(v, per))
}

fn list(v: &[i64], per: f64) -> String {
    v.iter().map(|x| dec(*x, per)).collect::<Vec<_>>().join(",")
}

fn unlist<const N: usize>(s: &str, per: f64) -> Option<[i64; N]> {
    let v: Vec<i64> = s.split(',').map(|x| undec(x, per)).collect::<Option<_>>()?;
    v.try_into().ok()
}

/// A node on screen at a tick, quantised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeState {
    pub surface: u32,
    pub drawn: [i64; 4],
    pub visual: [i64; 4],
    /// Effective opacity, 1/255.
    pub op: i64,
    pub scale: [i64; 2],
    pub rot: i64,
    pub hidden: Option<String>,
    pub text: Option<String>,
    /// Where it is in its surface's paint order at the tick: 0 is drawn
    /// first (the trace lists a surface's rows in the order they paint).
    pub z: u32,
    /// A text's glyphs as drawn (`Row::ink`), 1/64 px.
    pub ink: Option<[i64; 4]>,
    /// A text cut to its box: the width it was shaped for (`Row::fit`),
    /// 1/64 px.
    pub fit: Option<i64>,
}

/// A surface shown at a tick, quantised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceState {
    pub size: [i64; 2],
    pub origin: [i64; 2],
    /// The pose the compositor applies: opacity (1/255), scale (1/1000),
    /// x and y (1/64 px).
    pub pose: Option<[i64; 4]>,
    /// Where it takes input (`None`: the surface manager's).
    pub input: Option<Vec<[i64; 4]>>,
}

/// One painted tick: everything on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    pub k: u32,
    pub painted: Vec<u32>,
    pub surfaces: BTreeMap<u32, SurfaceState>,
    pub nodes: BTreeMap<String, NodeState>,
}

/// A surface of the run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceMeta {
    pub id: u32,
    pub label: String,
    pub attached: u32,
    pub detached: Option<u32>,
}

/// A run as its recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recording {
    pub surfaces: Vec<SurfaceMeta>,
    /// Each node's label, by key (the nodes the ticks show).
    pub labels: BTreeMap<String, String>,
    pub ticks: Vec<Tick>,
}

fn node_state(r: &Row, z: u32) -> NodeState {
    let rect = |v: [f32; 4]| v.map(|x| qz(f64::from(x), PX));
    NodeState {
        z,
        ink: r.ink.map(rect),
        fit: r.fit.map(|x| qz(f64::from(x), PX)),
        surface: r.surface,
        drawn: rect(r.drawn),
        visual: rect(r.visual),
        op: qz(f64::from(r.op), OP),
        scale: r.scale.map(|x| qz(f64::from(x), SCALE)),
        rot: qz(f64::from(r.rotate), DEG),
        hidden: r.hidden.clone(),
        text: r.text.clone(),
    }
}

fn pose(p: &PoseRec) -> [i64; 4] {
    [
        qz(f64::from(p.opacity), OP),
        qz(f64::from(p.scale), SCALE),
        qz(f64::from(p.x), PX),
        qz(f64::from(p.y), PX),
    ]
}

fn input(r: Option<&Regions>) -> Option<Vec<[i64; 4]>> {
    r.and_then(|r| r.input.as_ref())
        .map(|v| v.iter().map(|x| x.map(|c| qz(f64::from(c), PX))).collect())
}

impl Recording {
    /// The recording of `trace` (as run: not moved by its surfaces).
    pub fn from_trace(trace: &RunTrace) -> Recording {
        let mut nodes: BTreeMap<String, NodeState> = BTreeMap::new();
        let mut surfaces: BTreeMap<u32, SurfaceState> = BTreeMap::new();
        let mut ticks = Vec::with_capacity(trace.frames.len());
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for f in &trace.frames {
            nodes
                .retain(|_, n| f.sizes.contains_key(&n.surface) && !f.painted.contains(&n.surface));
            let mut z: BTreeMap<u32, u32> = BTreeMap::new();
            for r in &f.n {
                let at = z.entry(r.surface).or_default();
                nodes.insert(r.key.clone(), node_state(r, *at));
                *at += 1;
                seen.insert(r.key.as_str());
            }
            let mut next = BTreeMap::new();
            for (sid, size) in &f.sizes {
                let was = surfaces.get(sid);
                let painted = f.painted.contains(sid);
                let o = f.origins.get(sid).copied().unwrap_or([0.0, 0.0]);
                next.insert(
                    *sid,
                    SurfaceState {
                        size: size.map(|x| qz(f64::from(x), PX)),
                        origin: o.map(|x| qz(f64::from(x), PX)),
                        pose: if painted {
                            f.poses.get(sid).map(pose)
                        } else {
                            was.and_then(|w| w.pose)
                        },
                        input: if painted {
                            input(f.regions.get(sid))
                        } else {
                            was.and_then(|w| w.input.clone())
                        },
                    },
                );
            }
            surfaces = next;
            let mut painted = f.painted.clone();
            painted.sort_unstable();
            ticks.push(Tick {
                k: f.k,
                painted,
                surfaces: surfaces.clone(),
                nodes: nodes.clone(),
            });
        }
        let labels = seen
            .iter()
            .map(|k| {
                let label = trace
                    .nodes
                    .get(*k)
                    .map_or_else(|| (*k).to_string(), |m| m.label.clone());
                ((*k).to_string(), label)
            })
            .collect();
        Recording {
            surfaces: trace
                .surfaces
                .iter()
                .map(|s| SurfaceMeta {
                    id: s.id,
                    label: s.label.clone(),
                    attached: s.attached,
                    detached: s.detached,
                })
                .collect(),
            labels,
            ticks,
        }
    }

    /// A node's label (its key when the recording does not know it).
    pub fn label<'a>(&'a self, key: &'a str) -> &'a str {
        self.labels.get(key).map_or(key, |l| l.as_str())
    }

    /// The recording as a trace the viewer draws (wireframes: there are
    /// no images), on `run`'s scenario, segments and events: each tick a
    /// frame whose painted surfaces carry their nodes. A recording keeps
    /// no laid box, so a row's laid box is its drawn one.
    pub fn to_trace(&self, run: &RunTrace) -> RunTrace {
        let times: BTreeMap<u32, u64> = run.frames.iter().map(|f| (f.k, f.t_ns)).collect();
        let hz = u64::from(run.scenario.hz.max(1));
        let roots: BTreeMap<u32, &SurfaceInfo> = run.surfaces.iter().map(|s| (s.id, s)).collect();
        let mut t = RunTrace {
            frames: Vec::new(),
            digest: "reference".into(),
            ..run.clone()
        };
        t.surfaces = self
            .surfaces
            .iter()
            .map(|s| SurfaceInfo {
                id: s.id,
                label: s.label.clone(),
                kind: roots.get(&s.id).map(|r| r.kind.clone()).unwrap_or_default(),
                root: roots.get(&s.id).map(|r| r.root.clone()).unwrap_or_default(),
                attached: s.attached,
                detached: s.detached,
            })
            .collect();
        for (k, label) in &self.labels {
            t.nodes.entry(k.clone()).or_insert_with(|| NodeMeta {
                label: label.clone(),
                ..NodeMeta::default()
            });
        }
        let px = |v: i64| (v as f64 / PX) as f32;
        for tick in &self.ticks {
            let mut f = Frame {
                k: tick.k,
                t_ns: times
                    .get(&tick.k)
                    .copied()
                    .unwrap_or(1_000_000_000 + u64::from(tick.k) * 1_000_000_000 / hz),
                painted: tick.painted.clone(),
                scale: run.scenario.scale,
                ..Frame::default()
            };
            for (sid, s) in &tick.surfaces {
                f.sizes.insert(*sid, s.size.map(px));
                f.origins.insert(*sid, s.origin.map(px));
                if !tick.painted.contains(sid) {
                    continue;
                }
                if let Some(p) = s.pose {
                    f.poses.insert(
                        *sid,
                        PoseRec {
                            opacity: (p[0] as f64 / OP) as f32,
                            scale: (p[1] as f64 / SCALE) as f32,
                            x: px(p[2]),
                            y: px(p[3]),
                        },
                    );
                }
                f.regions.insert(
                    *sid,
                    Regions {
                        input: s
                            .input
                            .as_ref()
                            .map(|v| v.iter().map(|r| r.map(px)).collect()),
                        ..Regions::default()
                    },
                );
            }
            // In paint order, as a trace lists them.
            let mut shown: Vec<(&String, &NodeState)> = tick
                .nodes
                .iter()
                .filter(|(_, n)| tick.painted.contains(&n.surface))
                .collect();
            shown.sort_by_key(|(k, n)| (n.surface, n.z, *k));
            for (key, n) in shown {
                let op = (n.op as f64 / OP) as f32;
                f.n.push(Row {
                    key: key.clone(),
                    surface: n.surface,
                    drawn: n.drawn.map(px),
                    visual: n.visual.map(px),
                    laid: n.drawn.map(px),
                    op,
                    own_op: op,
                    scale: n.scale.map(|x| (x as f64 / SCALE) as f32),
                    rotate: (n.rot as f64 / DEG) as f32,
                    hidden: n.hidden.clone(),
                    text: n.text.clone(),
                    ink: n.ink.map(|r| r.map(px)),
                    fit: n.fit.map(px),
                    ..Row::default()
                });
            }
            t.frames.push(f);
        }
        t
    }
}

/// How far two recordings may differ and still be the same, in quanta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tol {
    /// Boxes, origins, pose offsets and input regions (1/64 px).
    pub px: i64,
    pub op: i64,
    pub scale: i64,
    pub rot: i64,
}

impl Tol {
    /// One quantum of each: what an approved reference holds a run to.
    pub const QUANTUM: Tol = Tol {
        px: 1,
        op: 1,
        scale: 1,
        rot: 1,
    };

    /// `px` logical px on geometry, a quantum elsewhere: what a run must
    /// come within to reproduce a recording marked bad.
    pub fn px(px: f64) -> Tol {
        Tol {
            px: qz(px, PX).max(1),
            ..Tol::QUANTUM
        }
    }
}

/// One difference: where, what, and the two values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub k: u32,
    /// The node, when it is a node's.
    pub key: Option<String>,
    /// `box#after`, `surface 1 (popup #card)`, `frame`.
    pub label: String,
    /// `drawn.x`, `opacity`, `painted`, `shown`…
    pub field: String,
    pub run: String,
    pub reference: String,
}

impl Change {
    /// `k57 box#after drawn.x 101.5 (reference 100)`.
    pub fn line(&self) -> String {
        format!(
            "k{} {} {} {} (reference {})",
            self.k, self.label, self.field, self.run, self.reference
        )
    }
}

/// Where a run left its reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    /// The first difference (by tick, then surfaces, then node key).
    pub first: Change,
    /// The first difference of each node or surface that differs, by
    /// tick.
    pub per_node: Vec<Change>,
    /// Ticks that differ, of how many in either.
    pub ticks: usize,
    pub of: usize,
    /// Values that differ, in all.
    pub total: usize,
}

impl Diff {
    /// `differs at 12 of 140 ticks, 3 nodes, 40 values`.
    pub fn counts(&self) -> String {
        format!(
            "differs at {} of {} ticks, on {} node{} or surface{}, {} value{} in all",
            self.ticks,
            self.of,
            self.per_node.len(),
            if self.per_node.len() == 1 { "" } else { "s" },
            if self.per_node.len() == 1 { "" } else { "s" },
            self.total,
            if self.total == 1 { "" } else { "s" },
        )
    }
}

fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref()
        .map_or_else(|| "-".to_string(), |x| x.to_string())
}

/// The differences of one node between two states: `(field, run,
/// reference)`.
fn node_diff(a: &NodeState, r: &NodeState, tol: Tol, out: &mut Vec<(String, String, String)>) {
    if a.surface != r.surface {
        out.push((
            "surface".into(),
            a.surface.to_string(),
            r.surface.to_string(),
        ));
    }
    const AX: [&str; 4] = ["x", "y", "w", "h"];
    for (name, x, y) in [
        ("drawn", &a.drawn, &r.drawn),
        ("visual", &a.visual, &r.visual),
    ] {
        for i in 0..4 {
            if (x[i] - y[i]).abs() > tol.px {
                out.push((format!("{name}.{}", AX[i]), dec(x[i], PX), dec(y[i], PX)));
            }
        }
    }
    if (a.op - r.op).abs() > tol.op {
        out.push((
            "opacity".into(),
            format!("{}/255", a.op),
            format!("{}/255", r.op),
        ));
    }
    for (i, axis) in ["x", "y"].iter().enumerate() {
        if (a.scale[i] - r.scale[i]).abs() > tol.scale {
            out.push((
                format!("scale.{axis}"),
                dec(a.scale[i], SCALE),
                dec(r.scale[i], SCALE),
            ));
        }
    }
    if (a.rot - r.rot).abs() > tol.rot {
        out.push(("rotate".into(), dec(a.rot, DEG), dec(r.rot, DEG)));
    }
    if a.hidden != r.hidden {
        out.push(("hidden".into(), opt(&a.hidden), opt(&r.hidden)));
    }
    if a.text != r.text {
        out.push(("text".into(), opt(&a.text), opt(&r.text)));
    }
    if a.z != r.z {
        out.push(("order".into(), a.z.to_string(), r.z.to_string()));
    }
    let ink_same = match (&a.ink, &r.ink) {
        (None, None) => true,
        (Some(x), Some(y)) => within(x, y, tol.px),
        _ => false,
    };
    if !ink_same {
        out.push(("ink".into(), ink_text(&a.ink), ink_text(&r.ink)));
    }
    let fit_same = match (a.fit, r.fit) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= tol.px,
        _ => false,
    };
    if !fit_same {
        out.push((
            "fit".into(),
            a.fit.map_or_else(|| "-".into(), |v| dec(v, PX)),
            r.fit.map_or_else(|| "-".into(), |v| dec(v, PX)),
        ));
    }
}

fn ink_text(r: &Option<[i64; 4]>) -> String {
    r.as_ref().map_or_else(|| "-".into(), |r| list(r, PX))
}

fn within(a: &[i64], b: &[i64], t: i64) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= t)
}

fn surface_diff(
    a: &SurfaceState,
    r: &SurfaceState,
    tol: Tol,
    out: &mut Vec<(String, String, String)>,
) {
    if !within(&a.size, &r.size, tol.px) {
        out.push(("size".into(), list(&a.size, PX), list(&r.size, PX)));
    }
    if !within(&a.origin, &r.origin, tol.px) {
        out.push(("origin".into(), list(&a.origin, PX), list(&r.origin, PX)));
    }
    let pose_same = match (&a.pose, &r.pose) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            (x[0] - y[0]).abs() <= tol.op
                && (x[1] - y[1]).abs() <= tol.scale
                && within(&x[2..], &y[2..], tol.px)
        }
        _ => false,
    };
    if !pose_same {
        out.push(("pose".into(), pose_text(&a.pose), pose_text(&r.pose)));
    }
    let input_same = match (&a.input, &r.input) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| within(p, q, tol.px))
        }
        _ => false,
    };
    if !input_same {
        out.push(("input".into(), input_text(&a.input), input_text(&r.input)));
    }
}

fn pose_text(p: &Option<[i64; 4]>) -> String {
    match p {
        None => "-".into(),
        Some(p) => format!(
            "{},{},{},{}",
            p[0],
            dec(p[1], SCALE),
            dec(p[2], PX),
            dec(p[3], PX)
        ),
    }
}

fn input_text(i: &Option<Vec<[i64; 4]>>) -> String {
    match i {
        None => "-".into(),
        Some(v) if v.is_empty() => "none".into(),
        Some(v) => v.iter().map(|r| list(r, PX)).collect::<Vec<_>>().join("/"),
    }
}

fn surface_label(rec: &Recording, other: &Recording, id: u32) -> String {
    let l = rec
        .surfaces
        .iter()
        .chain(&other.surfaces)
        .find(|s| s.id == id)
        .map(|s| s.label.as_str())
        .unwrap_or("");
    if l.is_empty() {
        format!("surface {id}")
    } else {
        format!("surface {id} ({l})")
    }
}

/// Where `run` differs from `reference` beyond `tol`, or `None` when it
/// does not.
pub fn diff(reference: &Recording, run: &Recording, tol: Tol) -> Option<Diff> {
    let mut changes: Vec<Change> = Vec::new();
    let mut ticks_differing = 0;
    let push = |changes: &mut Vec<Change>,
                k: u32,
                key: Option<&str>,
                label: String,
                d: Vec<(String, String, String)>| {
        for (field, a, r) in d {
            changes.push(Change {
                k,
                key: key.map(String::from),
                label: label.clone(),
                field,
                run: a,
                reference: r,
            });
        }
    };
    // The surfaces themselves: when each came and went.
    let ids: BTreeSet<u32> = reference
        .surfaces
        .iter()
        .chain(&run.surfaces)
        .map(|s| s.id)
        .collect();
    for id in ids {
        let a = run.surfaces.iter().find(|s| s.id == id);
        let r = reference.surfaces.iter().find(|s| s.id == id);
        let label = surface_label(run, reference, id);
        let when = |s: Option<&SurfaceMeta>| {
            s.map_or_else(
                || "never".to_string(),
                |s| {
                    format!(
                        "k{}..{}",
                        s.attached,
                        opt(&s.detached.map(|d| format!("k{d}")))
                    )
                },
            )
        };
        let same = match (a, r) {
            (Some(a), Some(r)) => a.attached == r.attached && a.detached == r.detached,
            _ => false,
        };
        if !same {
            let k = a.or(r).map_or(0, |s| s.attached);
            push(
                &mut changes,
                k,
                None,
                label,
                vec![("attached".into(), when(a), when(r))],
            );
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut of = 0;
    while i < run.ticks.len() || j < reference.ticks.len() {
        of += 1;
        let ka = run.ticks.get(i).map(|t| t.k);
        let kr = reference.ticks.get(j).map(|t| t.k);
        let before = changes.len();
        let (a, r, k) = match (ka, kr) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
                (&run.ticks[i - 1], &reference.ticks[j - 1], x)
            }
            (Some(x), y) if y.is_none_or(|y| x < y) => {
                i += 1;
                push(
                    &mut changes,
                    x,
                    None,
                    "frame".into(),
                    vec![(
                        "painted".into(),
                        format!("{:?}", run.ticks[i - 1].painted),
                        "nothing".into(),
                    )],
                );
                ticks_differing += 1;
                continue;
            }
            (_, Some(y)) => {
                j += 1;
                push(
                    &mut changes,
                    y,
                    None,
                    "frame".into(),
                    vec![(
                        "painted".into(),
                        "nothing".into(),
                        format!("{:?}", reference.ticks[j - 1].painted),
                    )],
                );
                ticks_differing += 1;
                continue;
            }
            _ => break,
        };
        if a.painted != r.painted {
            push(
                &mut changes,
                k,
                None,
                "frame".into(),
                vec![(
                    "painted".into(),
                    format!("{:?}", a.painted),
                    format!("{:?}", r.painted),
                )],
            );
        }
        let sids: BTreeSet<u32> = a
            .surfaces
            .keys()
            .chain(r.surfaces.keys())
            .copied()
            .collect();
        for sid in sids {
            let label = surface_label(run, reference, sid);
            let mut d = Vec::new();
            match (a.surfaces.get(&sid), r.surfaces.get(&sid)) {
                (Some(x), Some(y)) => surface_diff(x, y, tol, &mut d),
                (x, _) => d.push((
                    "shown".into(),
                    if x.is_some() { "yes" } else { "no" }.into(),
                    if x.is_some() { "no" } else { "yes" }.into(),
                )),
            }
            push(&mut changes, k, None, label, d);
        }
        let keys: BTreeSet<&String> = a.nodes.keys().chain(r.nodes.keys()).collect();
        for key in keys {
            let mut d = Vec::new();
            match (a.nodes.get(key), r.nodes.get(key)) {
                (Some(x), Some(y)) => node_diff(x, y, tol, &mut d),
                (x, _) => d.push((
                    "on screen".into(),
                    if x.is_some() { "yes" } else { "no" }.into(),
                    if x.is_some() { "no" } else { "yes" }.into(),
                )),
            }
            let label = if run.labels.contains_key(key.as_str()) {
                run.label(key)
            } else {
                reference.label(key)
            }
            .to_string();
            push(&mut changes, k, Some(key), label, d);
        }
        if changes.len() > before {
            ticks_differing += 1;
        }
    }
    let first = changes.first()?.clone();
    let mut seen: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    let per_node = changes
        .iter()
        .filter(|c| seen.insert((c.key.clone(), c.label.clone())))
        .cloned()
        .collect();
    Some(Diff {
        first,
        per_node,
        ticks: ticks_differing,
        of,
        total: changes.len(),
    })
}

// --- the text

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The strings of `s`, JSON strings separated by spaces.
fn json_strs(s: &str) -> Option<Vec<String>> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<String>()
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

/// A node's fields as the text writes them, in order.
fn node_fields(n: &NodeState) -> [(&'static str, String); 14] {
    [
        ("srf", n.surface.to_string()),
        ("x", dec(n.drawn[0], PX)),
        ("y", dec(n.drawn[1], PX)),
        ("w", dec(n.drawn[2], PX)),
        ("h", dec(n.drawn[3], PX)),
        (
            "v",
            if n.visual == n.drawn {
                "d".into()
            } else {
                list(&n.visual, PX)
            },
        ),
        ("op", n.op.to_string()),
        ("sc", list(&n.scale, SCALE)),
        ("rot", dec(n.rot, DEG)),
        ("hid", opt(&n.hidden)),
        ("txt", opt(&n.text)),
        ("ord", n.z.to_string()),
        ("ink", ink_text(&n.ink)),
        ("fit", n.fit.map_or_else(|| "-".into(), |v| dec(v, PX))),
    ]
}

/// A node's first line leaves out what is at these (and `srf` at the
/// first surface listed).
const NODE_DEFAULTS: [(&str, &str); 9] = [
    ("v", "d"),
    ("op", "255"),
    ("sc", "1,1"),
    ("rot", "0"),
    ("hid", "-"),
    ("txt", "-"),
    ("ord", "0"),
    ("ink", "-"),
    ("fit", "-"),
];

fn surface_fields(s: &SurfaceState) -> [(&'static str, String); 4] {
    [
        ("z", list(&s.size, PX)),
        ("o", list(&s.origin, PX)),
        ("P", pose_text(&s.pose)),
        ("i", input_text(&s.input)),
    ]
}

const SURFACE_DEFAULTS: [(&str, &str); 3] = [("o", "0,0"), ("P", "-"), ("i", "-")];

/// The surface a node's first line leaves out: the first listed.
fn first_surface(rec: &Recording) -> u32 {
    rec.surfaces.first().map_or(0, |s| s.id)
}

/// The recording as text (see the module's docs).
pub fn encode(rec: &Recording) -> String {
    let mut out = String::new();
    for s in &rec.surfaces {
        out += &format!(
            "S{} {} {} {}\n",
            s.id,
            s.attached,
            opt(&s.detached),
            json_str(&s.label)
        );
    }
    // Nodes numbered in the order they first show, then by key.
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    for t in &rec.ticks {
        for k in t.nodes.keys() {
            if !index.contains_key(k.as_str()) {
                index.insert(k, order.len());
                order.push(k);
            }
        }
    }
    for (i, k) in order.iter().enumerate() {
        out += &format!("N{i} {} {}\n", json_str(k), json_str(rec.label(k)));
    }
    let srf0 = first_surface(rec).to_string();
    let mut last_s: BTreeMap<u32, [(&str, String); 4]> = BTreeMap::new();
    let mut last_n: BTreeMap<&str, [(&str, String); 14]> = BTreeMap::new();
    let mut painted: Option<&Vec<u32>> = None;
    for t in &rec.ticks {
        if painted == Some(&t.painted) {
            out += &format!("k{}\n", t.k);
        } else {
            out += &format!(
                "k{} p{}\n",
                t.k,
                t.painted
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        painted = Some(&t.painted);
        for (sid, s) in &t.surfaces {
            let f = surface_fields(s);
            let was = last_s.get(sid);
            let parts: Vec<String> = f
                .iter()
                .enumerate()
                .filter(|(i, (name, v))| match was {
                    Some(w) => w[*i].1 != *v,
                    None => !SURFACE_DEFAULTS.contains(&(*name, v.as_str())),
                })
                .map(|(_, (name, v))| format!("{name}={v}"))
                .collect();
            if was.is_none() || !parts.is_empty() {
                out += &format!("s{sid} {}\n", parts.join(" "));
            }
            last_s.insert(*sid, f);
        }
        let gone: Vec<u32> = last_s
            .keys()
            .filter(|s| !t.surfaces.contains_key(s))
            .copied()
            .collect();
        for s in gone {
            out += &format!("s{s} -\n");
            last_s.remove(&s);
        }
        let mut lines: Vec<(usize, String)> = Vec::new();
        for (key, n) in &t.nodes {
            let f = node_fields(n);
            let was = last_n.get(key.as_str());
            let parts: Vec<String> = f
                .iter()
                .enumerate()
                .filter(|(i, (name, v))| match was {
                    Some(w) => w[*i].1 != *v,
                    None if *name == "srf" => *v != srf0,
                    None => !NODE_DEFAULTS.contains(&(*name, v.as_str())),
                })
                .map(|(_, (name, v))| format!("{name}={v}"))
                .collect();
            if was.is_none() || !parts.is_empty() {
                lines.push((index[key.as_str()], parts.join(" ")));
            }
            last_n.insert(key.as_str(), f);
        }
        let gone: Vec<&str> = last_n
            .keys()
            .filter(|k| !t.nodes.contains_key(**k))
            .copied()
            .collect();
        for k in gone {
            lines.push((index[k], "-".into()));
            last_n.remove(k);
        }
        lines.sort();
        for (i, l) in lines {
            out += &format!("n{i} {l}\n");
        }
    }
    out
}

fn bad(line: usize, what: &str) -> String {
    format!("line {line}: {what}")
}

fn set_node(n: &mut NodeState, name: &str, v: &str, vis_d: &mut bool) -> Option<()> {
    match name {
        "srf" => n.surface = v.parse().ok()?,
        "x" => n.drawn[0] = undec(v, PX)?,
        "y" => n.drawn[1] = undec(v, PX)?,
        "w" => n.drawn[2] = undec(v, PX)?,
        "h" => n.drawn[3] = undec(v, PX)?,
        "v" if v == "d" => *vis_d = true,
        "v" => {
            *vis_d = false;
            n.visual = unlist(v, PX)?;
        }
        "op" => n.op = v.parse().ok()?,
        "sc" => n.scale = unlist(v, SCALE)?,
        "rot" => n.rot = undec(v, DEG)?,
        "hid" => n.hidden = (v != "-").then(|| v.to_string()),
        "txt" => n.text = (v != "-").then(|| v.to_string()),
        "ord" => n.z = v.parse().ok()?,
        "ink" if v == "-" => n.ink = None,
        "ink" => n.ink = Some(unlist(v, PX)?),
        "fit" if v == "-" => n.fit = None,
        "fit" => n.fit = Some(undec(v, PX)?),
        _ => return None,
    }
    Some(())
}

fn set_surface(s: &mut SurfaceState, name: &str, v: &str) -> Option<()> {
    match name {
        "z" => s.size = unlist(v, PX)?,
        "o" => s.origin = unlist(v, PX)?,
        "P" if v == "-" => s.pose = None,
        "P" => {
            let p: Vec<&str> = v.split(',').collect();
            let [o, sc, x, y] = p.as_slice() else {
                return None;
            };
            s.pose = Some([
                o.parse().ok()?,
                undec(sc, SCALE)?,
                undec(x, PX)?,
                undec(y, PX)?,
            ]);
        }
        "i" if v == "-" => s.input = None,
        "i" if v == "none" => s.input = Some(Vec::new()),
        "i" => {
            s.input = Some(
                v.split('/')
                    .map(|r| unlist::<4>(r, PX))
                    .collect::<Option<Vec<_>>>()?,
            )
        }
        _ => return None,
    }
    Some(())
}

/// The text back as a recording; `first` is the line number of its first
/// line (for messages).
pub fn decode(text: &str, first: usize) -> Result<Recording, String> {
    let mut rec = Recording::default();
    let mut keys: Vec<String> = Vec::new();
    // The state carried from line to line, and whether each node's
    // visual box is its drawn one.
    let mut nodes: BTreeMap<String, (NodeState, bool)> = BTreeMap::new();
    let mut surfaces: BTreeMap<u32, SurfaceState> = BTreeMap::new();
    let mut cur: Option<Tick> = None;
    let finish = |cur: &mut Option<Tick>,
                  rec: &mut Recording,
                  nodes: &BTreeMap<String, (NodeState, bool)>,
                  surfaces: &BTreeMap<u32, SurfaceState>| {
        if let Some(mut t) = cur.take() {
            t.surfaces = surfaces.clone();
            t.nodes = nodes
                .iter()
                .map(|(k, (n, vd))| {
                    let mut n = n.clone();
                    if *vd {
                        n.visual = n.drawn;
                    }
                    (k.clone(), n)
                })
                .collect();
            rec.ticks.push(t);
        }
    };
    for (i, line) in text.lines().enumerate() {
        let ln = first + i;
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let num = |s: &str| -> Result<u32, String> {
            s.get(1..)
                .and_then(|x| x.parse().ok())
                .ok_or_else(|| bad(ln, &format!("`{head}`: a number after `{tag}`")))
        };
        match tag {
            'S' => {
                let id = num(head)?;
                let mut it = rest.splitn(3, ' ');
                let attached = it
                    .next()
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| bad(ln, "a surface's attached tick"))?;
                let detached = match it.next() {
                    Some("-") => None,
                    Some(d) => Some(
                        d.parse()
                            .map_err(|_| bad(ln, "a surface's detached tick"))?,
                    ),
                    None => return Err(bad(ln, "a surface's detached tick")),
                };
                let label = json_strs(it.next().unwrap_or(""))
                    .and_then(|v| v.into_iter().next())
                    .ok_or_else(|| bad(ln, "a surface's label"))?;
                rec.surfaces.push(SurfaceMeta {
                    id,
                    label,
                    attached,
                    detached,
                });
            }
            'N' => {
                let n = num(head)? as usize;
                if n != keys.len() {
                    return Err(bad(ln, &format!("node N{n} out of order")));
                }
                let v = json_strs(rest).ok_or_else(|| bad(ln, "a node's key and label"))?;
                let [key, label] = v.as_slice() else {
                    return Err(bad(ln, "a node's key and label"));
                };
                rec.labels.insert(key.clone(), label.clone());
                keys.push(key.clone());
            }
            'k' => {
                finish(&mut cur, &mut rec, &nodes, &surfaces);
                let k = num(head)?;
                // No `p`: the tick before's.
                let painted = match rest.strip_prefix('p') {
                    None if rest.is_empty() && !rec.ticks.is_empty() => rec
                        .ticks
                        .last()
                        .map(|t| t.painted.clone())
                        .unwrap_or_default(),
                    None => return Err(bad(ln, "a tick's painted surfaces (`p0,1`)")),
                    Some("") => Vec::new(),
                    Some(p) => p
                        .split(',')
                        .map(|x| x.parse().map_err(|_| bad(ln, "a painted surface")))
                        .collect::<Result<Vec<u32>, _>>()?,
                };
                cur = Some(Tick {
                    k,
                    painted,
                    ..Tick::default()
                });
            }
            's' => {
                if cur.is_none() {
                    return Err(bad(ln, "a surface line before any tick"));
                }
                let sid = num(head)?;
                if rest == "-" {
                    surfaces.remove(&sid);
                    continue;
                }
                let s = surfaces.entry(sid).or_default();
                for part in rest.split(' ').filter(|p| !p.is_empty()) {
                    let (name, v) = part
                        .split_once('=')
                        .ok_or_else(|| bad(ln, &format!("`{part}`: name=value")))?;
                    set_surface(s, name, v).ok_or_else(|| bad(ln, &format!("`{part}`")))?;
                }
            }
            'n' => {
                if cur.is_none() {
                    return Err(bad(ln, "a node line before any tick"));
                }
                let n = num(head)? as usize;
                let key = keys
                    .get(n)
                    .ok_or_else(|| bad(ln, &format!("no node N{n}")))?
                    .clone();
                if rest == "-" {
                    nodes.remove(&key);
                    continue;
                }
                let srf0 = first_surface(&rec);
                let (state, vd) = nodes.entry(key).or_insert_with(|| {
                    (
                        NodeState {
                            surface: srf0,
                            op: 255,
                            scale: [1000, 1000],
                            ..NodeState::default()
                        },
                        true,
                    )
                });
                for part in rest.split(' ').filter(|p| !p.is_empty()) {
                    let (name, v) = part
                        .split_once('=')
                        .ok_or_else(|| bad(ln, &format!("`{part}`: name=value")))?;
                    set_node(state, name, v, vd).ok_or_else(|| bad(ln, &format!("`{part}`")))?;
                }
            }
            _ => return Err(bad(ln, &format!("`{line}`: not a recording line"))),
        }
    }
    finish(&mut cur, &mut rec, &nodes, &surfaces);
    Ok(rec)
}

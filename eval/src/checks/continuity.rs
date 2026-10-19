//! (eval-rules-2) `continuity`: a motion goes on from what is on screen.
//!
//! design.md "Layout, animation and input": "Retargeting mid-flight
//! keeps velocity, so interrupted animations never jolt", and a spring is
//! `spring(stiffness, damping)`, a unit-mass damped oscillator
//! (docs/architecture.md, "Motion": a change starts at the next sample,
//! one refresh of its surface before it (at most `START_LEAD`, never
//! before the previous sample), from wherever the value is there,
//! keeping its velocity; a timed curve carries it as `v0·t·(1 − t/d)²`).
//! The trace holds every sample a painted frame took of a prop's spring,
//! a laid-out size's and a glide's (`trace::SpringRec`): the value drawn
//! and what the motion was asked to do (its target and curve, a change
//! starting and a FLIP's jump), never its velocity or where a way began.
//! From those alone this check works each spring's way out, sample by
//! sample, and holds the value drawn to it:
//!
//! - **velocity**: a change in flight starts from the place and the
//!   velocity the way had there; one that starts from rest, or stalls or
//!   turns back on an axis whose target did not, is a jolt, and so is a
//!   way in flight dropped for one at rest (a jump, or a dead stop);
//! - **start**: a change starts one refresh of its surface before the
//!   first frame that shows it, so that frame moves it as far as each
//!   frame after does (decisions.md first-frame-lead);
//! - **way**: between changes, each frame draws the value where its curve
//!   is at the frame's time, settled within its tolerance;
//! - **snap**: a spring that comes to rest snaps to its target, and on
//!   an opacity or a colour that jump is at most a step of 1/255
//!   (decisions.md border-eps);
//! - **end**: a surface that wants no more frames shows each spring it
//!   sampled last exactly at its target (a settled motion snaps to it);
//! - **first frame**: a node drawn for the first time has no value on
//!   screen to spring from, so on the frame it enters only what its enter
//!   pose sets springs (design.md "Poses"; decisions.md
//!   m4-effects-finish);
//! - **fold** (eval-rules-3): a size that springs from zero, or to zero
//!   and then unmounts, carries its slot with it: "its padding and the
//!   parent's gap beside it fold with it, so its slot reaches zero as the
//!   spring settles and unmounting it moves nothing" (strand-render
//!   anim `Forced::collapse`; design.md's toast, whose "siblings slide
//!   up to fill the gap"). So on the frame such a node first draws, its
//!   siblings are laid out no further from where the frame before laid
//!   them than its slot then takes (its size, and a gap folded to at most
//!   its size), and on the frame after it unmounts no further than its
//!   last frame's slot, and their parent, sized by what it holds, changes
//!   across the frame a slot unmounts by no more than that slot
//!   (eval-coverage-3: a last row's gap left behind and dropped when it
//!   unmounts). A gap that opens at once and a glide that hides the step
//!   (set E's enter-from-zero mutant) is not a fold;
//! - **roll**: a `roll: true` text a change interrupts rolls on from the
//!   text on screen, the nearer of the two it was rolling between
//!   (design.md "Rolling numbers"; `effects/roll.rs`), not from the one
//!   the eye had all but lost.
//!
//! A spring met in flight with no way known (its sample before was not
//! traced), and a way its surface paints a frame without sampling
//! (`reduced_motion` snaps and samples nothing), are picked up at their
//! next rest. Each way is held within four times its settling tolerance
//! plus 1e-4 of its span: the renderer works in f32, and a settle a
//! frame apart from the check's arithmetic moves a value by at most one
//! tolerance.

use std::collections::HashMap;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{CurveRec, Frame, SpringRec};

/// `START_LEAD` (strand-scene's motion.rs): the most a change starts
/// before its first frame, one refresh at 60 Hz.
const START_LEAD: u64 = 16_667_000;

/// `MAX_MOTION`: a spring counts as settled after 10 s.
const MAX_MOTION: u64 = 10_000_000_000;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = springs(cx);
    out.extend(short(cx));
    out.extend(rolls(cx));
    out.extend(fold(cx));
    first_per_node(out)
}

/// A spring's way between two changes: from where and how fast it left,
/// to what and along what, from when (ns).
#[derive(Clone, Debug, PartialEq)]
pub struct Way {
    pub start: u64,
    pub from: Vec<f64>,
    pub vel: Vec<f64>,
    pub target: Vec<f64>,
    pub curve: CurveRec,
    /// Its settling tolerance per channel (decisions.md border-eps).
    pub eps: Vec<f64>,
}

impl Way {
    /// At rest at `v`.
    pub fn rest(v: &[f32], eps: &[f32]) -> Way {
        let v: Vec<f64> = v.iter().map(|x| f64::from(*x)).collect();
        // One tolerance per channel; a trace that gives one for all (or
        // fewer) repeats its last.
        let last = eps.last().copied().unwrap_or(1e-3);
        let eps = (0..v.len())
            .map(|i| f64::from(eps.get(i).copied().unwrap_or(last)))
            .collect();
        Way {
            start: 0,
            vel: vec![0.0; v.len()],
            from: v.clone(),
            target: v,
            curve: CurveRec::Instant,
            eps,
        }
    }

    /// Place and velocity at `t` (ns).
    pub fn at(&self, t: u64) -> (Vec<f64>, Vec<f64>) {
        let n = self.target.len();
        let secs = t.saturating_sub(self.start) as f64 / 1e9;
        let mut pos = self.target.clone();
        let mut vel = vec![0.0; n];
        match self.curve {
            CurveRec::Instant => {}
            CurveRec::Spring { k, z } => {
                for i in 0..n {
                    let x0 = self.from[i] - self.target[i];
                    let (x, v) = spring(f64::from(k), f64::from(z), x0, self.vel[i], secs);
                    pos[i] = self.target[i] + x;
                    vel[i] = v;
                }
            }
            CurveRec::Timed { d, ease } => {
                // As strand-scene's `Segment::at`: the curve's own way,
                // its velocity by a small finite difference, and the
                // velocity carried in fading out as `v0·t·(1 − t/d)²`.
                let d = (d as f64 / 1e9).max(1e-6);
                let p = (secs / d).min(1.0) as f32;
                let easing = ease.easing();
                let e = strand_scene::motion::ease(easing, p);
                let h = 1e-3f32;
                let e2 = strand_scene::motion::ease(easing, (p + h).min(1.0));
                let de = if p < 1.0 {
                    f64::from((e2 - e) / h / d as f32)
                } else {
                    0.0
                };
                let u = (secs / d).min(1.0);
                let carry = secs.min(d) * (1.0 - u) * (1.0 - u);
                let dcarry = (1.0 - u) * (1.0 - 3.0 * u);
                for i in 0..n {
                    let span = self.target[i] - self.from[i];
                    pos[i] = self.from[i] + span * f64::from(e) + self.vel[i] * carry;
                    vel[i] = span * de + if p < 1.0 { self.vel[i] * dcarry } else { 0.0 };
                }
            }
        }
        (pos, vel)
    }

    /// At rest at its target by `t`, by the rule the product's springs
    /// rest by (strand-scene motion.rs `Segment::rest`, decisions.md
    /// roll-rate; integration-r4): each channel's amplitude,
    /// `√(x² + (v/ω₀)²)`, within its own `eps`, `ω₀ = √k`. A damped spring's
    /// amplitude never grows, so a way at rest stays at rest, and it
    /// ends at the moment the product's spring does, whatever the rate.
    pub fn settled(&self, t: u64) -> bool {
        let dt = t.saturating_sub(self.start);
        match self.curve {
            CurveRec::Instant => true,
            CurveRec::Timed { d, .. } => dt >= d,
            CurveRec::Spring { .. } if dt >= MAX_MOTION => true,
            CurveRec::Spring { k, .. } => {
                let (p, v) = self.at(t);
                let w0 = f64::from(k).max(1e-6).sqrt();
                (0..p.len()).all(|i| {
                    let x = p[i] - self.target[i];
                    let v = v[i] / w0;
                    (x * x + v * v).sqrt() <= self.eps.get(i).copied().unwrap_or(1e-3)
                })
            }
        }
    }

    /// The value drawn at `t`: its target once settled there (the way
    /// then ends at rest).
    pub fn sample(&mut self, t: u64) -> Vec<f64> {
        if self.settled(t) {
            *self = Way {
                start: self.start,
                from: self.target.clone(),
                vel: vec![0.0; self.target.len()],
                target: self.target.clone(),
                curve: CurveRec::Instant,
                eps: self.eps.clone(),
            };
            return self.target.clone();
        }
        self.at(t).0
    }

    /// The way a change to `target` along `curve` starting at `start`
    /// takes from this one, jumped by `shift` (a FLIP): from where this
    /// one is then, as fast as it moves then.
    pub fn retarget(&self, start: u64, target: &[f32], curve: CurveRec, shift: &[f32]) -> Way {
        let (mut pos, vel) = self.at(start.max(self.start));
        for (p, s) in pos.iter_mut().zip(shift) {
            *p += f64::from(*s);
        }
        let target: Vec<f64> = target.iter().map(|x| f64::from(*x)).collect();
        if curve == CurveRec::Instant {
            return Way {
                start,
                from: target.clone(),
                vel: vec![0.0; target.len()],
                target,
                curve,
                eps: self.eps.clone(),
            };
        }
        Way {
            start,
            from: pos,
            vel,
            target,
            curve,
            eps: self.eps.clone(),
        }
    }

    fn moving(&self) -> bool {
        self.curve != CurveRec::Instant
    }
}

/// A unit-mass damped oscillator `spring(k, z)` released `t` s ago at
/// displacement `x0` with velocity `v0`: displacement and velocity.
/// Written here from the oscillator, not taken from strand-scene, so a
/// mistake in its closed form is a mismatch.
pub fn spring(k: f64, z: f64, x0: f64, v0: f64, t: f64) -> (f64, f64) {
    if t <= 0.0 {
        return (x0, v0);
    }
    let w = k.max(1e-6).sqrt();
    let z = z.max(0.0);
    if (z - 1.0).abs() < 1e-6 {
        // x(t) = (x0 + (v0 + w x0) t) e^{-wt}
        let c = v0 + w * x0;
        let e = (-w * t).exp();
        ((x0 + c * t) * e, (c - w * (x0 + c * t)) * e)
    } else if z < 1.0 {
        let wd = w * (1.0 - z * z).sqrt();
        let a = z * w;
        let b = (v0 + a * x0) / wd;
        let e = (-a * t).exp();
        let (s, c) = (wd * t).sin_cos();
        let x = e * (x0 * c + b * s);
        // d/dt of e^{-at}(x0 cos + b sin)
        let v = e * (-a * (x0 * c + b * s) + (-x0 * wd * s + b * wd * c));
        (x, v)
    } else {
        let r = (z * z - 1.0).sqrt();
        let r1 = -w * (z - r);
        let r2 = -w * (z + r);
        let c2 = (v0 - r1 * x0) / (r2 - r1);
        let c1 = x0 - c2;
        let (e1, e2) = ((r1 * t).exp(), (r2 * t).exp());
        (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
    }
}

/// How far a sampled value may sit from its way: four times its settling
/// tolerance (a settle the way reaches a frame apart from the renderer's
/// f32 arithmetic moves it by at most one), plus 1e-4 of the distance.
fn tolerance(w: &Way, i: usize) -> f64 {
    4.0 * w.eps.get(i).copied().unwrap_or(1e-3) + 1e-4 * (w.target[i] - w.from[i]).abs() + 1e-5
}

/// The most a channel of `v` is off `want`, against its tolerance:
/// `(channel, got, want)` past it.
fn off(w: &Way, v: &[f32], want: &[f64]) -> Option<(usize, f64, f64)> {
    (0..v.len().min(want.len()))
        .map(|i| (i, f64::from(v[i]), want[i]))
        .filter(|(i, got, want)| (got - want).abs() > tolerance(w, *i))
        .max_by(|a, b| (a.1 - a.2).abs().total_cmp(&(b.1 - b.2).abs()))
}

/// When the change `st` asked for starts, sampled at `t` on a surface
/// whose frame period is `period` (ns, zero: not known): one refresh
/// before (at most `START_LEAD`), never before its last sample.
pub fn start_of(t: u64, last: Option<u64>, period: u64) -> u64 {
    let lead = if period == 0 {
        START_LEAD
    } else {
        period.min(START_LEAD)
    };
    let lead = match last {
        Some(l) => t.saturating_sub(l).min(lead),
        None => 0,
    };
    t.saturating_sub(lead)
}

/// What the check knows of one spring: its way, and the change that
/// started it (for the finding's words).
struct Known {
    way: Way,
    /// The tick a change started at, the way before it, the start's own
    /// sample time and the motion's last sample before it.
    began: Option<(u32, Way, u64, Option<u64>)>,
    /// When it was last sampled (ns) and at which tick.
    last: (u64, u32),
    /// Its surface.
    s: u32,
}

fn springs(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    // samples, held to a way, starts followed, met with no way, lost.
    // first-frame records, rests met with a way known.
    // snaps drawn.
    let mut stats = [0u64; 8];
    let mut known: HashMap<(&str, &str), Option<Known>> = HashMap::new();
    for f in &cx.trace.frames {
        let sampled: std::collections::HashSet<(&str, &str)> =
            f.sp.iter()
                .map(|r| (r.key.as_str(), r.of.as_str()))
                .collect();
        // A way in flight its surface painted without sampling (motion
        // reduced snaps it, or the value came from elsewhere) is no
        // longer one the check can follow: picked up at its next rest.
        for (id, slot) in known.iter_mut() {
            if slot
                .as_ref()
                .is_some_and(|kn| kn.way.moving() && f.painted.contains(&kn.s))
                && !sampled.contains(id)
            {
                *slot = None;
            }
        }
        for r in &f.sp {
            let id = (r.key.as_str(), r.of.as_str());
            if cx.allowed(CheckId::Continuity, &r.key) {
                continue;
            }
            let t = r.at.unwrap_or(f.t_ns);
            if r.enter == Some(false) && r.start.is_some() {
                stats[5] += 1;
            }
            if let Some(v) = first_frame(f, r) {
                out.push(v);
            }
            let slot = known.entry(id).or_insert(None);
            if let Some(rest) = &r.rest {
                if let Some(kn) = slot.as_ref() {
                    stats[6] += 1;
                    if let Some(v) = dropped(f, r, kn, rest) {
                        out.push(v);
                    }
                }
                *slot = Some(Known {
                    way: Way::rest(rest, &r.eps),
                    began: None,
                    last: (t, f.k),
                    s: r.s,
                });
            }
            stats[0] += 1;
            let Some(kn) = slot.as_mut() else {
                stats[3] += 1;
                continue;
            };
            if r.to.len() != kn.way.target.len() || r.v.len() != r.to.len() {
                *slot = None;
                continue;
            }
            if let Some(st) = &r.start {
                let period = f.period.get(&r.s).copied().unwrap_or(0);
                let start = start_of(t, st.last, period);
                let before = kn.way.clone();
                kn.way = before.retarget(start, &r.to, r.c, &st.shift);
                kn.began = Some((f.k, before, t, st.last));
                stats[2] += 1;
            } else if r
                .to
                .iter()
                .zip(&kn.way.target)
                .any(|(a, b)| (f64::from(*a) - b).abs() > 1e-4 + 1e-6 * b.abs())
            {
                // A target changed with no change started: a way this
                // check cannot follow (it never should). Picked up at
                // its next rest.
                stats[4] += 1;
                *slot = None;
                continue;
            }
            stats[1] += 1;
            kn.last = (t, f.k);
            if let Some(v) = snap(f, r, &kn.way, t) {
                stats[7] += 1;
                out.push(v);
            }
            let want = kn.way.sample(t);
            let Some((ch, got, wanted)) = off(&kn.way, &r.v, &want) else {
                continue;
            };
            out.push(finding(f, r, kn, t, ch, got, wanted));
            // Its way is lost: picked up at its next rest.
            *slot = None;
        }
    }
    super::stats(
        "continuity.springs",
        cx,
        &[
            ("samples", stats[0]),
            ("held", stats[1]),
            ("changes", stats[2]),
            ("unknown", stats[3]),
            ("lost", stats[4]),
            ("first", stats[5]),
            ("rests", stats[6]),
            ("snaps", stats[7]),
        ],
    );
    out
}

/// `fold`: the frame a node sized from zero first draws, and the frame
/// after one sized to zero unmounts, lay its siblings out no further from
/// where the frame before laid them (from their parent) than its slot
/// then takes: its laid size and a gap folded to at most that, both
/// counted for every sibling folding there. Judged only where nothing
/// else in the parent changes at that frame: every other sibling there
/// at both frames keeps its laid size, and none comes or goes but those
/// folding. A sibling that shows nothing at both frames (no area, or no
/// opacity) is not judged: where it is laid opens no gap one sees; nor is
/// one whose place among its siblings changed (a reorder moved it, and
/// FLIP glides it) (integration-r5).
fn fold(cx: &Cx) -> Vec<Finding> {
    use strand_render::probe::Motions;
    let tl = &cx.tl;
    let mut out = Vec::new();
    let (mut held, mut held_parents) = (0u64, 0u64);
    // (timeline frame before, frame after, axis): the nodes folding
    // there, each with its slot's size at the frame it draws in.
    let mut at: std::collections::BTreeMap<(usize, usize, usize), Folding> =
        std::collections::BTreeMap::new();
    for f in &cx.trace.frames {
        for r in &f.sp {
            let axis = match r.of.as_str() {
                "w" => 0,
                "h" => 1,
                _ => continue,
            };
            let from_zero = r.start.is_some()
                && r.rest
                    .as_ref()
                    .is_some_and(|v| v.first().is_some_and(|x| *x < COLLAPSED));
            if !from_zero || cx.allowed(CheckId::Continuity, &r.key) {
                continue;
            }
            let Some(i) = tl.at(f.k).filter(|i| *i > 0) else {
                continue;
            };
            // Its first frame: not drawn the frame before.
            let Some(row) = tl.row(i, &r.key) else {
                continue;
            };
            if tl.row(i - 1, &r.key).is_some() {
                continue;
            }
            at.entry((i - 1, i, axis))
                .or_default()
                .push((r.key.as_str(), f64::from(row.laid[2 + axis])));
        }
    }
    for (key, meta) in &cx.trace.nodes {
        let Some(pose) = meta.exit else { continue };
        if cx.allowed(CheckId::Continuity, key) {
            continue;
        }
        for (axis, p) in [(0, pose.w), (1, pose.h)] {
            if !p.is_some_and(|p| p < COLLAPSED) {
                continue;
            }
            // The last frame it is drawn in, exiting, then gone.
            for i in 0..tl.ks.len().saturating_sub(1) {
                let Some(r) = tl.row(i, key) else { continue };
                if tl.row(i + 1, key).is_some() || !Motions(r.mot).contains(Motions::EXIT) {
                    continue;
                }
                // Unmounted, not its surface gone.
                if !tl.frames[i + 1].painted.contains(&r.surface)
                    || !tl.screens[i + 1].values().any(|q| q.surface == r.surface)
                {
                    continue;
                }
                at.entry((i, i + 1, axis))
                    .or_default()
                    .push((key.as_str(), f64::from(r.laid[2 + axis])));
            }
        }
    }
    for ((a, b, axis), folding) in at {
        // By parent.
        let mut parents: std::collections::BTreeMap<&str, Vec<(&str, f64)>> =
            std::collections::BTreeMap::new();
        for (k, size) in folding {
            if let Some(p) = cx.parent(k) {
                parents.entry(p).or_default().push((k, size));
            }
        }
        for (p, folding) in parents {
            let (Some(pa), Some(pb)) = (tl.row(a, p), tl.row(b, p)) else {
                continue;
            };
            let kids = |i: usize| -> Vec<(&str, &crate::trace::Row)> {
                tl.screens[i]
                    .iter()
                    .filter(|(k, _)| cx.parent(k) == Some(p))
                    .map(|(k, r)| (*k, *r))
                    .collect()
            };
            let (ka, kb) = (kids(a), kids(b));
            let folds = |k: &str| folding.iter().any(|(f, _)| *f == k);
            let came_or_went = ka
                .iter()
                .filter(|(k, _)| !kb.iter().any(|(q, _)| q == k))
                .chain(kb.iter().filter(|(k, _)| !ka.iter().any(|(q, _)| q == k)))
                .any(|(k, _)| !folds(k));
            let resized = ka.iter().any(|(k, ra)| {
                !folds(k)
                    && kb.iter().any(|(q, rb)| {
                        q == k
                            && (0..2)
                                .any(|j| (ra.laid[2 + j] - rb.laid[2 + j]).abs() > cx.tol as f32)
                    })
            });
            if came_or_went || resized {
                continue;
            }
            let slot: f64 = folding.iter().map(|(_, s)| 2.0 * s.max(0.0)).sum();
            let bound = slot + cx.tol;
            // (eval-coverage-3) The parent itself: a slot that folds with
            // the gap beside it leaves nothing for its mounting or
            // unmounting to take, so a parent sized by what it holds is
            // laid no larger or smaller across that frame than the slot
            // then takes ("unmounting it moves nothing": the last row of
            // a content-sized card, whose gap its neighbour above gives
            // up). One whose own size moves by itself (its size
            // springs, it exits, keyframes or time drive it) is not
            // judged; one of fixed size never changes. Only unmounting is
            // judged: what a parent sized by its content is laid at the
            // frame a slot mounts from zero is the content-sized growth
            // rule's (it takes its size at rest at once and its slot
            // springs into it: docs/decisions.md, a question left to the
            // owner), not the fold's.
            let own = Motions::SIZE_SPRING | Motions::EXIT | Motions::KEYFRAMES | Motions::TIME;
            let went = folding.iter().all(|(k, _)| tl.row(b, k).is_none());
            if went
                && !Motions(pa.mot).intersects(own)
                && !Motions(pb.mot).intersects(own)
                && !cx.allowed(CheckId::Continuity, p)
            {
                held_parents += 1;
                let grew = f64::from(pb.laid[2 + axis] - pa.laid[2 + axis]);
                if grew.abs() > bound {
                    let who = folding[0].0;
                    let label = cx.trace.nodes.get(who).map_or(who, |m| m.label.as_str());
                    out.push(
                        Finding::fail(
                            CheckId::Continuity,
                            format!(
                                "fold: {label} unmounts at size 0 here, taking a slot of at most \
                                 {slot:.1} px, yet its parent is laid {grew:+.1} px {} than \
                                 the frame before: the gap beside it did not fold with it",
                                ["wider", "taller"][axis]
                            ),
                        )
                        .at(tl.ks[b])
                        .node(p)
                        .axis(["w", "h"][axis])
                        .got(grew.abs(), [0.0, bound]),
                    );
                }
            }
            // (integration-r5) Where each sibling that stays stands among
            // the others on the axis: one whose place in that order
            // changed was moved by the change, not by the fold (design.md
            // "Reordering uses FLIP": a sorted list inserting a row and
            // moving another), and its glide is its own.
            let order = |kids: &[(&str, &crate::trace::Row)], k: &str| -> Option<usize> {
                let at = kids.iter().find(|(q, _)| *q == k)?.1.laid[axis];
                Some(
                    kids.iter()
                        .filter(|(q, r)| !folds(q) && *q != k && r.laid[axis] < at)
                        .count(),
                )
            };
            for (k, ra) in &ka {
                if folds(k) || cx.allowed(CheckId::Continuity, k) {
                    continue;
                }
                let Some((_, rb)) = kb.iter().find(|(q, _)| q == k) else {
                    continue;
                };
                if order(&ka, k) != order(&kb, k) {
                    continue;
                }
                // (integration-r5) A sibling that shows nothing at both
                // frames (no area, or no opacity: one leaving itself,
                // folded to nothing and faded out) opens no gap the eye
                // sees, wherever it is laid.
                let blank = |r: &crate::trace::Row| {
                    r.visual[2] < COLLAPSED || r.visual[3] < COLLAPSED || r.op <= 0.0
                };
                if blank(ra) && blank(rb) {
                    continue;
                }
                held += 1;
                let shift = f64::from(rb.laid[axis] - pb.laid[axis])
                    - f64::from(ra.laid[axis] - pa.laid[axis]);
                // A FLIP glide started here on this axis over a step of
                // its layout within its parent: the glide hides the step
                // (a slot that folds lays its siblings out anew each
                // frame, with nothing to glide over). Its jump less its
                // parent's move (a FLIP's jump is where it was drawn
                // less where it is laid): what carries the whole parent
                // (a centred panel shrinking once the fold is done) is
                // not a step within it (set D's launcher-open-narrow).
                let moved = f64::from(pb.laid[axis] - pa.laid[axis]);
                let glided = tl.frames[b]
                    .sp
                    .iter()
                    .filter(|g| g.key == *k && g.of == "glide")
                    .filter_map(|g| g.start.as_ref())
                    .map(|st| st.shift.get(axis).map_or(0.0, |v| f64::from(*v)) + moved)
                    .fold(0.0f64, |m, v| m.max(v.abs()));
                if shift.abs() <= bound && glided <= cx.tol {
                    continue;
                }
                let (who, how) = (
                    folding[0].0,
                    if tl.row(b, folding[0].0).is_some() {
                        "enters from"
                    } else {
                        "unmounts at"
                    },
                );
                let label = cx.trace.nodes.get(who).map_or(who, |m| m.label.as_str());
                let (msg, got, lim) = if shift.abs() > bound {
                    (
                        format!(
                            "fold: {label} {how} size 0 here, taking a slot of at most \
                             {slot:.1} px, yet this sibling is laid {shift:.1} px from where \
                             the frame before laid it: its gap opened (or closed) at once \
                             instead of folding with it"
                        ),
                        shift.abs(),
                        bound,
                    )
                } else {
                    (
                        format!(
                            "fold: {label} {how} size 0 here, yet this sibling started a glide \
                             over a {glided:.1} px step of its layout: its gap opened (or \
                             closed) at once and the glide hides it, where a folding slot lays \
                             its siblings out anew each frame"
                        ),
                        glided,
                        cx.tol,
                    )
                };
                out.push(
                    Finding::fail(CheckId::Continuity, msg)
                        .at(tl.ks[b])
                        .node(k)
                        .axis(["x", "y"][axis])
                        .got(got, [0.0, lim]),
                );
            }
        }
    }
    super::stats(
        "continuity.fold",
        cx,
        &[("siblings", held), ("parents", held_parents)],
    );
    out
}

/// The nodes folding at a frame, each with its slot's size there.
type Folding<'a> = Vec<(&'a str, f64)>;

/// A size this small is zero: its slot collapses (strand-render anim
/// `COLLAPSED`).
const COLLAPSED: f32 = 0.5;

/// The most a spring's settle may move a colour channel or an opacity:
/// one step of 1/255, what the raster draws apart.
const SNAP: f64 = 1.0 / 255.0;

/// `snap`: a spring comes to rest by snapping to its target from where
/// its way is then, by at most its settling tolerance per channel
/// (architecture.md "Motion"). On an opacity and on a colour's channels
/// (premultiplied OKLab plus alpha: a fill's, a text's, a track's, and
/// the colour of a border, a stroke or a glow beside its width) that
/// snap is drawn, and a fade or a colour change ends in a jump the eye
/// sees when it is over a step of 1/255: a tolerance set for a
/// channel of other units (a border's width in pixels) is too coarse
/// for its colour (decisions.md border-eps).
fn snap(f: &Frame, r: &SpringRec, w: &Way, t: u64) -> Option<Finding> {
    if !matches!(w.curve, CurveRec::Spring { .. })
        || t.saturating_sub(w.start) >= MAX_MOTION
        || !w.settled(t)
    {
        return None;
    }
    let chans: &[usize] = match r.of.as_str() {
        "opacity" => &[0],
        "bg" | "color" | "track" | "fill" => &[0, 1, 2, 3],
        "border" | "stroke" | "glow" => &[1, 2, 3, 4],
        _ => return None,
    };
    let (p, _) = w.at(t);
    let ch = chans
        .iter()
        .copied()
        .filter(|i| *i < p.len() && *i < w.target.len())
        .max_by(|a, b| {
            (p[*a] - w.target[*a])
                .abs()
                .total_cmp(&(p[*b] - w.target[*b]).abs())
        })
        .filter(|i| (p[*i] - w.target[*i]).abs() > SNAP)?;
    let chan = if p.len() > 1 {
        format!("{}[{ch}]", r.of)
    } else {
        r.of.clone()
    };
    Some(
        Finding::fail(
            CheckId::Continuity,
            format!(
                "snap: its spring settles at k{} by jumping from {} to its target {}, more than \
                 a step of 1/255: a fade or a colour change ends where the eye no longer sees it \
                 move, not by a jump",
                f.k,
                num(p[ch]),
                num(w.target[ch])
            ),
        )
        .at(f.k)
        .node(&r.key)
        .axis(&chan)
        .got(p[ch], [w.target[ch], w.target[ch]]),
    )
}

/// `end`: a surface that stops wanting frames shows what it
/// drew last until something changes, so each spring it sampled last is
/// exactly its target there (a motion that settles snaps to its target,
/// and a surface wants frames while one has not: architecture.md
/// "Motion", "Once settled it is exactly its target"), unless its node
/// is leaving (`vanish` holds an exit to its pose). Exactly: within f32
/// rounding, not a settling tolerance, as a motion stopped one frame
/// short of settling is within that.
fn short(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut ends = 0u64;
    for f in &cx.trace.frames {
        let mut last: HashMap<(&str, &str), &SpringRec> = HashMap::new();
        for r in &f.sp {
            last.insert((r.key.as_str(), r.of.as_str()), r);
        }
        let mut last: Vec<&SpringRec> = last.into_values().collect();
        last.sort_by(|a, b| (&a.key, &a.of).cmp(&(&b.key, &b.of)));
        for r in last {
            if !f.painted.contains(&r.s)
                || f.wants.contains(&r.s)
                || r.v.len() != r.to.len()
                || cx.allowed(CheckId::Continuity, &r.key)
                || f.n.iter().any(|n| {
                    n.key == r.key
                        && strand_render::probe::Motions(n.mot)
                            .contains(strand_render::probe::Motions::EXIT)
                })
            {
                continue;
            }
            ends += 1;
            let Some(ch) = (0..r.v.len())
                .max_by(|a, b| {
                    (r.v[*a] - r.to[*a])
                        .abs()
                        .total_cmp(&(r.v[*b] - r.to[*b]).abs())
                })
                .filter(|i| {
                    f64::from((r.v[*i] - r.to[*i]).abs())
                        > 1e-5 * f64::from(r.to[*i].abs().max(1.0))
                })
            else {
                continue;
            };
            let chan = if r.v.len() > 1 {
                format!("{}[{ch}]", r.of)
            } else {
                r.of.clone()
            };
            let (got, want) = (f64::from(r.v[ch]), f64::from(r.to[ch]));
            out.push(
                Finding::fail(
                    CheckId::Continuity,
                    format!(
                        "end: its surface wants no frame after k{} and so shows {} until \
                         something changes, short of its target {}: a motion ends on its target",
                        f.k,
                        num(got),
                        num(want)
                    ),
                )
                .at(f.k)
                .node(&r.key)
                .axis(&chan)
                .got(got, [want, want]),
            );
        }
    }
    super::stats("continuity.end", cx, &[("ends", ends)]);
    out
}

/// A motion found at rest where the way the check follows still moved
/// at its last sample: the way was dropped (a jump, or a stall that
/// throws its velocity away), not finished.
fn dropped(f: &Frame, r: &SpringRec, kn: &Known, rest: &[f32]) -> Option<Finding> {
    let w = &kn.way;
    if !w.moving() || rest.len() != w.target.len() {
        return None;
    }
    let (t, k) = kn.last;
    if w.settled(t) {
        return None;
    }
    let (p, v) = w.at(t);
    // A frame's worth of its velocity past its tolerance, or its place.
    let frame = 1.0 / 60.0;
    let ch = (0..rest.len()).find(|i| {
        (p[*i] - f64::from(rest[*i])).abs() > tolerance(w, *i)
            || v[*i].abs() * frame > tolerance(w, *i)
    })?;
    let chan = if rest.len() > 1 {
        format!("{}[{ch}]", r.of)
    } else {
        r.of.clone()
    };
    let msg = if (p[ch] - f64::from(rest[ch])).abs() > tolerance(w, ch) {
        format!(
            "velocity: at rest at {} by k{}, where its way since k{k} was at {} moving at \
             {:.1}/s: the way in flight was dropped, not carried on",
            num(f64::from(rest[ch])),
            f.k,
            num(p[ch]),
            v[ch]
        )
    } else {
        format!(
            "velocity: at rest at {} by k{}, where its way since k{k} was there moving at \
             {:.1}/s: the way in flight stopped dead",
            num(f64::from(rest[ch])),
            f.k,
            v[ch]
        )
    };
    Some(
        Finding::fail(CheckId::Continuity, msg)
            .at(f.k)
            .node(&r.key)
            .axis(&chan)
            .got(f64::from(rest[ch]), [p[ch], p[ch]]),
    )
}

/// `first frame`: a prop's spring that starts on the frame its node
/// enters, from a value its enter pose does not set.
fn first_frame(f: &Frame, r: &SpringRec) -> Option<Finding> {
    let (Some(false), Some(_), Some(rest)) = (r.enter, &r.start, &r.rest) else {
        return None;
    };
    let tol = |i: usize| f64::from(r.eps.get(i).or(r.eps.last()).copied().unwrap_or(1e-3));
    let ch = (0..rest.len().min(r.to.len()))
        .find(|i| (f64::from(rest[*i]) - f64::from(r.to[*i])).abs() > tol(*i))?;
    Some(
        Finding::fail(
            CheckId::Continuity,
            format!(
                "first frame: its `{}` springs from {} to {} on the frame it enters, though its \
                 enter pose does not set it: a node drawn for the first time has no value on \
                 screen to spring from",
                r.of,
                num(f64::from(rest[ch])),
                num(f64::from(r.to[ch]))
            ),
        )
        .at(f.k)
        .node(&r.key)
        .axis(&r.of),
    )
}

fn num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// The finding for a sample off its way: what the way says, and when a
/// change started the way, what the drawn values fit instead (a start
/// with no velocity, a start one `START_LEAD` early).
fn finding(
    f: &Frame,
    r: &SpringRec,
    kn: &Known,
    t: u64,
    ch: usize,
    got: f64,
    wanted: f64,
) -> Finding {
    let chan = if r.v.len() > 1 {
        format!("{}[{ch}]", r.of)
    } else {
        r.of.clone()
    };
    let msg = match &kn.began {
        Some((k0, before, t0, last)) => {
            let way = &kn.way;
            let fits = |w: &Way| {
                let mut w = w.clone();
                let v = w.sample(t);
                (v[ch] - got).abs() <= tolerance(&w, ch)
            };
            // Started from rest where the way it left moved.
            let still = Way {
                vel: vec![0.0; way.vel.len()],
                ..way.clone()
            };
            let (_, v_then) = before.at(way.start.max(before.start));
            let early = before.retarget(
                start_of(*t0, *last, 0),
                &r.to,
                r.c,
                &r.start
                    .as_ref()
                    .map(|s| s.shift.clone())
                    .unwrap_or_default(),
            );
            if before.moving() && v_then[ch].abs() > 1e-3 && fits(&still) {
                format!(
                    "velocity: retargeted at k{k0} while moving at {:.1}/s, it started again \
                     from rest: drawn {}, where a way that kept its velocity draws {}",
                    v_then[ch],
                    num(got),
                    num(wanted)
                )
            } else if early.start != way.start && fits(&early) {
                let period = f.period.get(&r.s).copied().unwrap_or(0);
                format!(
                    "start: the change at k{k0} started {:.1} ms before its first frame, where \
                     its surface refreshes every {:.1} ms: drawn {}, where a change started one \
                     refresh before draws {}",
                    START_LEAD as f64 / 1e6,
                    period as f64 / 1e6,
                    num(got),
                    num(wanted)
                )
            } else {
                format!(
                    "way: since the change at k{k0} it is drawn {}, where its {} draws {}",
                    num(got),
                    curve_name(r.c),
                    num(wanted)
                )
            }
        }
        None => format!(
            "way: drawn {}, where its {} draws {}",
            num(got),
            curve_name(r.c),
            num(wanted)
        ),
    };
    Finding::fail(CheckId::Continuity, msg)
        .at(f.k)
        .node(&r.key)
        .axis(&chan)
        .got(got, [wanted, wanted])
}

fn curve_name(c: CurveRec) -> String {
    match c {
        CurveRec::Instant => "snap".into(),
        CurveRec::Spring { k, z } => format!("spring({k}, {z})"),
        CurveRec::Timed { d, .. } => format!("{} ms curve", d / 1_000_000),
    }
}

/// `roll`: a roll a change interrupts goes on from the text on screen.
fn rolls(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut held = 0u64;
    // Each rolling text's roll in the frame before.
    let mut before: HashMap<&str, f32> = HashMap::new();
    for f in &cx.trace.frames {
        let mut now: HashMap<&str, f32> = HashMap::new();
        for r in &f.rolls {
            now.insert(r.key.as_str(), r.p);
            let Some((Some(a), Some(b), pw)) = &r.was else {
                continue;
            };
            let Some(from) = &r.from else { continue };
            if cx.allowed(CheckId::Continuity, &r.key) {
                continue;
            }
            // The nearer of the two at the change; when the frame before
            // showed the other side of half way, either is on screen.
            let near = if *pw < 0.5 { a } else { b };
            let far = if *pw < 0.5 { b } else { a };
            let seen = before.get(r.key.as_str()).copied();
            let either = seen.is_some_and(|p| (p < 0.5) != (*pw < 0.5));
            held += 1;
            if from == near || (either && from == far) || a == b {
                continue;
            }
            out.push(
                Finding::fail(
                    CheckId::Continuity,
                    format!(
                        "roll: interrupted at {:.2} of the way from {a:?} to {b:?}, it rolled on \
                         from {from:?}, not from {near:?}, the text on screen",
                        pw
                    ),
                )
                .at(f.k)
                .node(&r.key)
                .axis("text"),
            );
        }
        // A surface not painted keeps its rolls.
        for (k, p) in now {
            before.insert(k, p);
        }
    }
    super::stats("continuity.roll", cx, &[("interrupted", held)]);
    out
}

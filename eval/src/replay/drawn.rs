//! (replay-owner) The replay's verdict: the boxes the live shell drew
//! at the report (`drawn.json`, each mapped surface's nodes as its last
//! frame drew them) held, node by node, to the replay's traced frame of
//! the same surface at the same time, and that frame's pixels to the
//! bundle's PNG (docs/eval.md, "Replaying a glitch report", "The
//! verdict").
//!
//! A live surface is the replay's whose root is the same node (origin,
//! `id:` and `for` row key), each replay surface taken once, in order.
//! Its frame is the replay's last that painted it at or before the tick
//! nearest the time the live frame was drawn (`drawn_ns`), never one
//! after the report. A node is matched by its origin, its `id:` and its
//! `for` row key, the n-th live one of those to the n-th the replay
//! drew, in paint order. Its box is the drawn one through every
//! transform, axis aligned, in surface logical px; its opacity its own.
//! A node at rest is held to [`PX`] on every edge and [`OPACITY`]; one
//! moving at the report (by its live travel over the next frame period,
//! or the replay's over its frame's ticks) to [`STEP_FRAMES`] frames of
//! that travel more, and past that it is named on its own line, not as
//! a drift. VERIFIED: every node of both is in the other within its
//! bound, every live surface has its replayed one and the other way
//! round, and no pixel past the `cpu-gpu` tolerance differs from the
//! bundle's PNG outside the boxes of what moves. DRIFTED: anything
//! else, each difference named.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};

use super::bundle::Bundle;
use super::fidelity::{diff_pixels_masked, same_origin};
use super::plan::Plan;
use crate::checks::cpu_gpu::GPU_TOLERANCE;
use crate::harness::{NodeRef, RunOutput};
use crate::trace::Frame;

/// How far apart an edge may be, logical px.
pub const PX: f64 = 1.0;
/// How far apart an opacity may be.
pub const OPACITY: f64 = 0.02;
/// How many frames of its travel a node moving at the report may be
/// off besides: half a frame for the replay's tick nearest the live
/// frame's time, half for a motion's start quantised to the replay's
/// ticks, and up to one for the live frame's presentation lead (its
/// springs are sampled at the time it is shown, not drawn).
pub const STEP_FRAMES: f64 = 2.0;
/// Below this a node is at rest: a 64th of a px, an 8-bit step of
/// opacity.
const STILL_PX: f64 = 1.0 / 64.0;
const STILL_OPACITY: f64 = 1.0 / 255.0;

/// One node as drawn: its name and box, and how far it moves in a frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Drawn {
    pub origin: String,
    pub id: Option<String>,
    pub key: Option<String>,
    /// `[x, y, w, h]`, surface logical px.
    pub rect: [f64; 4],
    pub opacity: f64,
    /// Its travel over one frame, px (the farthest edge), and its
    /// opacity's change; 0 at rest.
    pub travel: f64,
    pub fade: f64,
}

impl Drawn {
    /// `@origin[key] #id`, as `replay.txt` names a node.
    pub fn name(&self) -> String {
        NodeRef {
            origin: self.origin.clone(),
            id: self.id.clone(),
            key: self.key.clone(),
        }
        .to_string()
    }

    fn same(&self, o: &Drawn) -> bool {
        same_origin(&self.origin, &o.origin) && self.id == o.id && self.key == o.key
    }

    /// The largest distance between their edges, px.
    pub fn px_off(&self, o: &Drawn) -> f64 {
        edges_off(self.rect, o.rect)
    }
}

fn edges_off(a: [f64; 4], b: [f64; 4]) -> f64 {
    let [x, y, w, h] = a;
    let [p, q, r, s] = b;
    [
        (x - p).abs(),
        (y - q).abs(),
        ((x + w) - (p + r)).abs(),
        ((y + h) - (q + s)).abs(),
    ]
    .into_iter()
    .fold(0.0, f64::max)
}

/// A node whose live and replayed drawings part.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    /// The surface root's origin.
    pub surface: String,
    pub live: Drawn,
    pub replay: Drawn,
    pub px: f64,
    pub opacity: f64,
    /// How far it moves in a frame (the larger of the two sides'), px
    /// and opacity: 0 for a node at rest.
    pub travel: f64,
    pub fade: f64,
}

impl Drift {
    fn moving(&self) -> bool {
        self.travel > STILL_PX || self.fade > STILL_OPACITY
    }
}

/// Pixels of a compared surface's frame that differ from the bundle's
/// PNG past the `cpu-gpu` tolerance, outside the boxes of what moves.
#[derive(Clone, Debug, PartialEq)]
pub struct Pixels {
    /// The surface root's origin.
    pub surface: String,
    pub png: String,
    pub over: u64,
    /// Their bounding box, buffer px: `[x, y, w, h]`.
    pub bbox: Option<[u32; 4]>,
    /// Why they could not be compared (sizes differ, a PNG that does
    /// not decode).
    pub why: Option<String>,
}

/// The verdict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Surfaces compared, and their nodes.
    pub surfaces: usize,
    pub nodes: usize,
    /// Nodes past their bound (at rest: [`PX`] and [`OPACITY`]).
    pub drifts: Vec<Drift>,
    /// Nodes moving at the report, past [`STEP_FRAMES`] of their travel:
    /// their phase on the replay's clock parts from the live shell's
    /// (a loop begun before the window, a motion's start off by more
    /// than its quantum). Named on their own lines; not a drift.
    pub moving: Vec<Drift>,
    /// Nodes moving at the report held within their bound.
    pub in_motion: usize,
    /// Nodes the live shell drew that the replay did not, and the other
    /// way, with their surface's origin.
    pub live_only: Vec<(String, Drawn)>,
    pub replay_only: Vec<(String, Drawn)>,
    /// Live surfaces with no replayed frame to hold them to.
    pub missing: Vec<String>,
    /// Surfaces the replay showed at the report that the live shell did
    /// not (their root's origin).
    pub replay_only_surfaces: Vec<String>,
    /// Compared surfaces whose pixels differ from the bundle's PNG.
    pub pixels: Vec<Pixels>,
    /// Surfaces whose pixels were held to the bundle's PNG.
    pub pixels_held: usize,
    /// Files the bundle copied that differ now (a replay on the desktop
    /// reads them as they are): named when the pixels differ.
    pub changed_files: Vec<String>,
    /// Why nothing could be compared (a bundle with no `drawn.json`).
    pub unchecked: Option<String>,
    /// The replay's own boxes at the report: each surface shown then
    /// (its root's origin) and its nodes, in paint order.
    pub replayed: Vec<(String, Vec<Drawn>)>,
    /// The replay's tick each compared surface was held at.
    pub ticks: Vec<Tick>,
}

/// (replay-clip) A compared surface's frame: its root's origin, the
/// replay's tick it was held at, the replayed surface (the trace's
/// surface id, so a clip places the comparison on one surface when two
/// share a root's origin), and how many of its nodes were moving at the
/// report and held within their bound (their boxes left out of the
/// pixel comparison).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tick {
    pub origin: String,
    pub k: u32,
    pub sid: u32,
    pub in_motion: usize,
}

impl Verdict {
    pub fn verified(&self) -> bool {
        self.unchecked.is_none()
            && self.drifts.is_empty()
            && self.live_only.is_empty()
            && self.replay_only.is_empty()
            && self.missing.is_empty()
            && self.replay_only_surfaces.is_empty()
            && self.pixels.is_empty()
    }

    /// `VERIFIED`, `DRIFTED` or `NOT CHECKED`.
    pub fn word(&self) -> &'static str {
        if self.unchecked.is_some() {
            "NOT CHECKED"
        } else if self.verified() {
            "VERIFIED"
        } else {
            "DRIFTED"
        }
    }

    /// What moved at the report, as the verdict's line ends.
    fn motion(&self) -> String {
        let mut m = String::new();
        if self.in_motion > 0 {
            m += &format!(
                "; {} moving at the report, each within {STEP_FRAMES} frames of its travel",
                self.in_motion
            );
        }
        if !self.moving.is_empty() {
            m += &format!(
                "; {} moving out of step with the live shell (named below)",
                self.moving.len()
            );
        }
        m
    }

    /// The verdict in one plain line.
    pub fn line(&self) -> String {
        if let Some(why) = &self.unchecked {
            return format!("NOT CHECKED: {why}");
        }
        if self.verified() {
            return format!(
                "VERIFIED: the replay drew all {} nodes on {} surface{} where the live shell \
                 did (within {PX} px and {OPACITY} opacity){}{}",
                self.nodes,
                self.surfaces,
                if self.surfaces == 1 { "" } else { "s" },
                match self.pixels_held {
                    0 => String::new(),
                    1 => ", and the pixels of 1 as the live frame's".into(),
                    n => format!(", and the pixels of {n} as the live frames'"),
                },
                self.motion()
            );
        }
        let mut parts = Vec::new();
        if !self.drifts.is_empty() {
            let worst = self.drifts.iter().map(|d| d.px).fold(0.0, f64::max);
            parts.push(format!(
                "{} of {} nodes drawn elsewhere (up to {worst:.1} px off)",
                self.drifts.len(),
                self.nodes
            ));
        }
        if !self.live_only.is_empty() {
            parts.push(format!("{} drawn live only", self.live_only.len()));
        }
        if !self.replay_only.is_empty() {
            parts.push(format!(
                "{} drawn in the replay only",
                self.replay_only.len()
            ));
        }
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        if !self.missing.is_empty() {
            parts.push(format!(
                "{} surface{} the replay never drew",
                self.missing.len(),
                plural(self.missing.len())
            ));
        }
        if !self.replay_only_surfaces.is_empty() {
            parts.push(format!(
                "{} surface{} shown in the replay only",
                self.replay_only_surfaces.len(),
                plural(self.replay_only_surfaces.len())
            ));
        }
        if !self.pixels.is_empty() {
            parts.push(format!(
                "pixels differ on {}",
                self.pixels
                    .iter()
                    .map(|p| match &p.why {
                        Some(_) => format!("@{} (not compared)", p.surface),
                        None => format!("@{} ({} px)", p.surface, p.over),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let first = self.lines().into_iter().next().unwrap_or_default();
        format!(
            "DRIFTED: {}{}; first: {first}",
            parts.join(", "),
            self.motion()
        )
    }

    /// One line per difference.
    pub fn lines(&self) -> Vec<String> {
        let rect = |r: [f64; 4]| format!("[{:.1}, {:.1}, {:.1}, {:.1}]", r[0], r[1], r[2], r[3]);
        let drift = |d: &Drift| {
            format!(
                "{} (surface @{}): live {} opacity {:.2}, replay {} opacity {:.2}: {:.1} px off{}{}",
                d.live.name(),
                d.surface,
                rect(d.live.rect),
                d.live.opacity,
                rect(d.replay.rect),
                d.replay.opacity,
                d.px,
                if d.opacity > OPACITY {
                    format!(", opacity {:.2} off", d.opacity)
                } else {
                    String::new()
                },
                if d.moving() {
                    format!(
                        ", moving at the report ({:.1} px and {:.2} opacity a frame)",
                        d.travel, d.fade
                    )
                } else {
                    String::new()
                }
            )
        };
        let mut out = Vec::new();
        for s in &self.missing {
            out.push(format!(
                "surface @{s}: the replay drew no frame of it at the report"
            ));
        }
        for s in &self.replay_only_surfaces {
            out.push(format!(
                "surface @{s}: shown in the replay at the report, not live"
            ));
        }
        for d in &self.drifts {
            out.push(drift(d));
        }
        for (s, n) in &self.live_only {
            out.push(format!(
                "{} (surface @{s}): drawn live at {}, not in the replay",
                n.name(),
                rect(n.rect)
            ));
        }
        for (s, n) in &self.replay_only {
            out.push(format!(
                "{} (surface @{s}): drawn in the replay at {}, not live",
                n.name(),
                rect(n.rect)
            ));
        }
        for p in &self.pixels {
            out.push(match &p.why {
                Some(w) => format!("pixels of @{} ({}): not compared: {w}", p.surface, p.png),
                None => format!(
                    "pixels of @{} ({}): {} differ past {GPU_TOLERANCE} from the live frame{}{}",
                    p.surface,
                    p.png,
                    p.over,
                    p.bbox.map_or_else(String::new, |b| format!(
                        ", in {}x{} at {},{}",
                        b[2], b[3], b[0], b[1]
                    )),
                    if self.changed_files.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " (files changed since the report: {})",
                            self.changed_files.join(", ")
                        )
                    }
                ),
            });
        }
        for d in &self.moving {
            out.push(format!("moving out of step: {}", drift(d)));
        }
        out
    }

    /// `fidelity.json`'s `verdict` part.
    pub fn json(&self) -> Json {
        let node = |n: &Drawn| {
            json!({"origin": n.origin, "id": n.id, "key": n.key, "box": n.rect,
                   "opacity": n.opacity, "travel": n.travel, "fade": n.fade})
        };
        let drift = |d: &Drift| {
            json!({
                "surface": d.surface,
                "node": d.live.name(),
                "live": node(&d.live),
                "replay": node(&d.replay),
                "px": d.px,
                "opacity": d.opacity,
                "travel": d.travel,
                "fade": d.fade,
            })
        };
        json!({
            "verdict": self.word(),
            "line": self.line(),
            "surfaces": self.surfaces,
            "nodes": self.nodes,
            "tolerance": {"px": PX, "opacity": OPACITY, "moving_frames": STEP_FRAMES},
            "unchecked": self.unchecked,
            "ticks": self.ticks.iter().map(|t| json!({"surface": t.origin, "k": t.k, "sid": t.sid, "in_motion": t.in_motion})).collect::<Vec<_>>(),
            "drifted": self.drifts.iter().map(drift).collect::<Vec<_>>(),
            "in_motion": self.in_motion,
            "pixels_held": self.pixels_held,
            "moving_out_of_step": self.moving.iter().map(drift).collect::<Vec<_>>(),
            "live_only": self.live_only.iter().map(|(s, n)| json!({"surface": s, "node": n.name(), "live": node(n)})).collect::<Vec<_>>(),
            "replay_only": self.replay_only.iter().map(|(s, n)| json!({"surface": s, "node": n.name(), "replay": node(n)})).collect::<Vec<_>>(),
            "missing": self.missing,
            "replay_only_surfaces": self.replay_only_surfaces,
            "pixels": self.pixels.iter().map(|p| json!({
                "surface": p.surface, "png": p.png, "over": p.over, "bbox": p.bbox, "why": p.why,
            })).collect::<Vec<_>>(),
            "changed_files": self.changed_files,
        })
    }
}

/// An origin as the bundle wrote it, with the live config directory
/// taken off (a test bundle may name it).
pub(crate) fn local(b: &Bundle, o: &str) -> String {
    b.live_config_dir()
        .and_then(|d| {
            o.strip_prefix(&format!("{}/", d.display()))
                .map(str::to_string)
        })
        .unwrap_or_else(|| o.to_string())
}

/// The tick about to run when the replay reached the report (its `mark
/// report` step): the frames before it are the ones the report saw.
pub fn report_tick(run: &RunOutput, plan: &Plan) -> u32 {
    run.trace
        .events
        .iter()
        .rev()
        .find(|e| e.text.trim() == "mark report")
        .map(|e| e.k)
        .unwrap_or_else(|| {
            let len = plan.end - plan.start;
            ((len + plan.period / 2) / plan.period.max(1)) as u32
        })
}

/// Whether trace surface `t` is shown at the report: attached before
/// its tick and not gone by then.
fn shown_at(t: &crate::trace::SurfaceInfo, report: u32) -> bool {
    t.attached < report && t.detached.is_none_or(|d| d >= report)
}

/// The replay's frame a live surface's frame is held to: the last one
/// that painted `sid` at or before the tick nearest `drawn_ns` (when the
/// live frame was drawn, ns since the rings' epoch; `None`: the report),
/// never one at or after the report's tick; the first traced one when
/// none was painted by then (the warm-up's last paint is it).
///
/// (replayfix) A live frame drawn more than [`STEP_FRAMES`] frame
/// periods before the report is its surface at rest (a surface paints
/// every frame while anything on it is in flight, a colour's spring
/// too): it is held to the replay's last paint at or before
/// [`STEP_FRAMES`] ticks past that nearest tick, the replay at rest
/// too. A motion's end is as uncertain on the replay's clock as its
/// start (quantised to the ticks, behind the live frame's presentation
/// lead), so the nearest tick may hold the replay a frame or two short
/// of rest: a spring's last step of colour, which no box shows.
pub fn frame_for<'a>(
    run: &'a RunOutput,
    plan: &Plan,
    sid: u32,
    drawn_ns: Option<u64>,
) -> Option<&'a Frame> {
    let report = report_tick(run, plan);
    let cap = report.saturating_sub(1);
    let period = plan.period.max(1);
    let steps = STEP_FRAMES as u64;
    let target = drawn_ns
        .filter(|t| *t >= plan.start)
        .map(|t| {
            let nearest = (t - plan.start + period / 2) / period;
            let at_rest = plan.end.saturating_sub(t) > steps * period;
            (nearest + if at_rest { steps } else { 0 }).min(u64::from(cap)) as u32
        })
        .unwrap_or(cap);
    let paints = |fr: &&Frame| fr.painted.contains(&sid) && fr.k < report;
    let frames = &run.trace.frames;
    frames
        .iter()
        .rev()
        .filter(paints)
        .find(|fr| fr.k <= target)
        .or_else(|| frames.iter().find(paints))
}

/// One painted frame's rows of a surface: key, visual box, own opacity.
type Rows = Vec<(String, [f64; 4], f64)>;

/// The nodes `fr` drew on surface `sid`, named by `run.log.refs`, each
/// with the replay's own travel over its ticks: how far it moved from
/// the frame before (the last that painted it) and to the frame after
/// (the next tick's, when it painted it), the larger.
fn nodes_of(run: &RunOutput, fr: &Frame, sid: u32) -> Vec<Drawn> {
    let rows = |f: &Frame| -> Rows {
        f.n.iter()
            .filter(|r| r.surface == sid)
            .map(|r| (r.key.clone(), r.visual.map(f64::from), f64::from(r.own_op)))
            .collect()
    };
    let frames = &run.trace.frames;
    let before = frames
        .iter()
        .rev()
        .find(|f| f.k < fr.k && f.painted.contains(&sid))
        .map(rows);
    let after = frames
        .iter()
        .find(|f| f.k == fr.k + 1 && f.painted.contains(&sid))
        .map(rows);
    let step = |other: &Option<Rows>, key: &str, rect: [f64; 4], op: f64| -> (f64, f64) {
        other
            .as_ref()
            .and_then(|o| o.iter().find(|(k, ..)| k == key))
            .map_or((0.0, 0.0), |(_, r, o)| {
                (edges_off(rect, *r), (op - o).abs())
            })
    };
    fr.n.iter()
        .filter(|r| r.surface == sid)
        .filter_map(|r| {
            let n = run.log.refs.get(&r.key)?;
            let rect = r.visual.map(f64::from);
            let op = f64::from(r.own_op);
            let (tb, fb) = step(&before, &r.key, rect, op);
            let (ta, fa) = step(&after, &r.key, rect, op);
            Some(Drawn {
                origin: n.origin.clone(),
                id: n.id.clone(),
                key: n.key.clone(),
                rect,
                opacity: op,
                travel: tb.max(ta),
                fade: fb.max(fa),
            })
        })
        .collect()
}

/// A replay surface's root as a node (origin, `id:`, row key).
fn replay_root(run: &RunOutput, root: &str) -> Option<NodeRef> {
    run.log.refs.get(root).cloned().or_else(|| {
        run.trace
            .nodes
            .get(root)
            .and_then(|m| m.origin.clone())
            .or_else(|| root.split('|').nth(1).map(str::to_string))
            .map(|origin| NodeRef {
                origin,
                id: None,
                key: None,
            })
    })
}

/// A live surface's root as a node, from `nodes.json` (its origin under
/// the config directory).
fn live_root(b: &Bundle, root: &str) -> Option<NodeRef> {
    let n = b.nodes.get(root)?;
    Some(NodeRef {
        origin: local(b, n["origin"].as_str()?),
        id: n["id"].as_str().map(str::to_string),
        key: n["key"].as_str().map(str::to_string),
    })
}

fn same_ref(a: &NodeRef, b: &NodeRef) -> bool {
    same_origin(&a.origin, &b.origin) && a.id == b.id && a.key == b.key
}

/// Which replay surface each live surface on the replayed output is
/// (`bundle.json`'s, by slot): the one shown at the report whose root is
/// the same node (origin, `id:`, row key), each taken once, in order.
/// `None`: no such surface, or a root `nodes.json` does not name.
pub fn match_surfaces(b: &Bundle, plan: &Plan, run: &RunOutput) -> BTreeMap<u16, Option<u32>> {
    let report = report_tick(run, plan);
    let shown: Vec<(u32, Option<NodeRef>)> = run
        .trace
        .surfaces
        .iter()
        .filter(|t| shown_at(t, report))
        .map(|t| (t.id, replay_root(run, &t.root)))
        .collect();
    let mut used = vec![false; shown.len()];
    let mut out = BTreeMap::new();
    for rec in &b.surfaces {
        if rec.output.as_deref().is_some_and(|o| o != plan.output.name) {
            continue;
        }
        let hit = live_root(b, &rec.root).and_then(|want| {
            shown
                .iter()
                .enumerate()
                .find(|(i, (_, r))| !used[*i] && r.as_ref().is_some_and(|r| same_ref(r, &want)))
                .map(|(i, (id, _))| (i, *id))
        });
        if let Some((i, _)) = hit {
            used[i] = true;
        }
        out.insert(rec.slot, hit.map(|(_, id)| id));
    }
    out
}

/// When the live surface in `slot` drew its last frame (`drawn.json`'s
/// `drawn_ns`), if the bundle says.
pub fn live_drawn_ns(b: &Bundle, slot: u16) -> Option<u64> {
    b.drawn["surfaces"]
        .as_array()?
        .iter()
        .find(|s| s["slot"].as_u64() == Some(u64::from(slot)))?["drawn_ns"]
        .as_u64()
}

/// (replayfix) The image of `sid` as the replay's frame `fr` left it:
/// its own, or, for a paint that damaged nothing (the trace keeps no
/// image of it), the last one kept before it, which the buffer still
/// holds.
pub(crate) fn image_of<'a>(run: &'a RunOutput, fr: &Frame, sid: u32) -> Option<&'a str> {
    run.trace
        .frames
        .iter()
        .rev()
        .filter(|f| f.k <= fr.k)
        .find_map(|f| f.png.get(&sid))
        .map(String::as_str)
}

/// The pixels of the replay's frame `fr` of `sid` against the bundle's
/// PNG `png`, outside `mask` (logical px rects, scaled by `scale`):
/// how many differ and their box, or why they could not be compared.
/// `None` when the replay kept no image of the frame.
fn pixels_of(
    b: &Bundle,
    run: &RunOutput,
    fr: &Frame,
    sid: u32,
    png: &str,
    scale: f64,
    mask: &[[f64; 4]],
) -> Option<(u64, Option<[u32; 4]>, Option<String>)> {
    let name = image_of(run, fr, sid)?;
    let (w, h, px) = run
        .images
        .get(name)
        .and_then(|i| crate::report::png::decode(&i.png).ok())?;
    let Some((lw, lh, lpx)) = std::fs::read(b.dir.join(png))
        .ok()
        .and_then(|bytes| crate::report::png::decode(&bytes).ok())
    else {
        return Some((0, None, Some(format!("{png} does not decode"))));
    };
    if (w, h) != (lw, lh) {
        return Some((
            0,
            None,
            Some(format!(
                "the replay's frame is {w}x{h}, the live one {lw}x{lh}"
            )),
        ));
    }
    let boxes: Vec<[u32; 4]> = mask
        .iter()
        .map(|r| {
            let x0 = (r[0] * scale).floor().max(0.0) as u32;
            let y0 = (r[1] * scale).floor().max(0.0) as u32;
            let x1 = ((r[0] + r[2]) * scale).ceil().max(0.0) as u32;
            let y1 = ((r[1] + r[3]) * scale).ceil().max(0.0) as u32;
            [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
        })
        .collect();
    let (over, bbox) = diff_pixels_masked(w, &lpx, &px, &boxes);
    Some((over, bbox, None))
}

/// A rect grown by `by` px on every side.
fn grown(r: [f64; 4], by: f64) -> [f64; 4] {
    [r[0] - by, r[1] - by, r[2] + 2.0 * by, r[3] + 2.0 * by]
}

/// Holds the replay's frame at the report to `drawn.json`.
pub fn compare(b: &Bundle, plan: &Plan, run: &RunOutput) -> Verdict {
    let mut v = Verdict::default();
    let report = report_tick(run, plan);
    for t in run.trace.surfaces.iter().filter(|t| shown_at(t, report)) {
        let origin = replay_root(run, &t.root).map_or_else(|| t.root.clone(), |r| r.origin);
        if let Some(fr) = frame_for(run, plan, t.id, None) {
            v.replayed.push((origin, nodes_of(run, fr, t.id)));
        }
    }
    let Some(surfaces) = b.drawn["surfaces"].as_array() else {
        v.unchecked = Some(
            "the report has no drawn.json (it was saved by a strand from before the live \
             boxes were recorded)"
                .into(),
        );
        return v;
    };
    let matched = match_surfaces(b, plan, run);
    for s in surfaces {
        let Some(slot) = s["slot"].as_u64().and_then(|n| u16::try_from(n).ok()) else {
            continue;
        };
        let rec = b.surfaces.iter().find(|r| r.slot == slot);
        // Only the output replayed (the sandbox has one screen).
        if rec.is_some_and(|r| r.output.as_deref().is_some_and(|o| o != plan.output.name)) {
            continue;
        }
        let root = s["root"].as_str().unwrap_or("");
        let Some(want) = live_root(b, root) else {
            v.missing
                .push(format!("{root} (its root is not in nodes.json)"));
            continue;
        };
        let origin = want.origin.clone();
        let live: Vec<Drawn> = s["nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| {
                        let r = n["box"].as_array()?;
                        let f = |i: usize| r.get(i).and_then(Json::as_f64);
                        Some(Drawn {
                            origin: local(b, n["origin"].as_str()?),
                            id: n["id"].as_str().map(str::to_string),
                            key: n["key"].as_str().map(str::to_string),
                            rect: [f(0)?, f(1)?, f(2)?, f(3)?],
                            opacity: n["opacity"].as_f64().unwrap_or(1.0),
                            travel: n["travel"].as_f64().unwrap_or(0.0),
                            fade: n["fade"].as_f64().unwrap_or(0.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sid = matched.get(&slot).copied().flatten();
        let drawn_ns = s["drawn_ns"].as_u64();
        let Some((sid, fr)) = sid.and_then(|id| Some((id, frame_for(run, plan, id, drawn_ns)?)))
        else {
            v.missing.push(origin);
            continue;
        };
        let at = v.ticks.len();
        v.ticks.push(Tick {
            origin: origin.clone(),
            k: fr.k,
            sid,
            in_motion: 0,
        });
        let replay = nodes_of(run, fr, sid);
        v.surfaces += 1;
        v.nodes += live.len();
        let mut mask = Vec::new();
        let mut used = vec![false; replay.len()];
        for l in live {
            let m = replay
                .iter()
                .enumerate()
                .find(|(i, r)| !used[*i] && l.same(r))
                .map(|(i, _)| i);
            let Some(i) = m else {
                v.live_only.push((origin.clone(), l));
                continue;
            };
            used[i] = true;
            let r = &replay[i];
            let d = Drift {
                surface: origin.clone(),
                px: l.px_off(r),
                opacity: (l.opacity - r.opacity).abs(),
                travel: l.travel.max(r.travel),
                fade: l.fade.max(r.fade),
                replay: r.clone(),
                live: l,
            };
            if d.moving() {
                // What moves is not held pixel for pixel: its boxes on
                // both sides, grown by its bound, are left out.
                let by = STEP_FRAMES * d.travel + 1.0;
                mask.push(grown(d.live.rect, by));
                mask.push(grown(d.replay.rect, by));
            }
            let px_bound = PX + STEP_FRAMES * d.travel;
            let op_bound = OPACITY + STEP_FRAMES * d.fade;
            if d.px <= px_bound && d.opacity <= op_bound {
                if d.moving() {
                    v.in_motion += 1;
                    v.ticks[at].in_motion += 1;
                }
            } else if d.moving() {
                v.moving.push(d);
            } else {
                v.drifts.push(d);
            }
        }
        for (i, r) in replay.into_iter().enumerate() {
            if !used[i] {
                v.replay_only.push((origin.clone(), r));
            }
        }
        // The frame's pixels against the bundle's PNG of it.
        if let Some(rec) = rec
            && let Some(png) = &rec.frame
            && let Some((over, bbox, why)) =
                pixels_of(b, run, fr, sid, png, rec.scale.max(0.01), &mask)
        {
            if why.is_none() {
                v.pixels_held += 1;
            }
            if over == 0 && why.is_none() {
                continue;
            }
            v.pixels.push(Pixels {
                surface: origin.clone(),
                png: png.clone(),
                over,
                bbox,
                why,
            });
        }
    }
    // What the replay showed at the report that no live surface is.
    let taken: Vec<u32> = matched.values().filter_map(|s| *s).collect();
    for t in run.trace.surfaces.iter().filter(|t| shown_at(t, report)) {
        if taken.contains(&t.id) || frame_for(run, plan, t.id, None).is_none() {
            continue;
        }
        let origin = replay_root(run, &t.root).map_or_else(|| t.root.clone(), |r| r.origin);
        v.replay_only_surfaces.push(origin);
    }
    if v.surfaces == 0 && v.missing.is_empty() && v.replay_only_surfaces.is_empty() {
        v.unchecked = Some(format!(
            "drawn.json has no surface on {}, the output replayed",
            plan.output.name
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::ReplayLog;

    /// (replayfix) A paint that damaged nothing keeps no image: its
    /// pixels are the last image kept of that surface before it (the
    /// busy session's request panel, painted again at tick 2 unchanged,
    /// had its pixels silently not held).
    #[test]
    fn a_paint_that_damaged_nothing_shows_the_image_before_it() {
        let frame = |k: u32, painted: &[u32], png: &[(u32, &str)]| Frame {
            k,
            painted: painted.to_vec(),
            png: png.iter().map(|(s, p)| (*s, p.to_string())).collect(),
            ..Frame::default()
        };
        let mut run = RunOutput {
            trace: Default::default(),
            images: BTreeMap::new(),
            notes: Vec::new(),
            log: ReplayLog::default(),
        };
        run.trace.frames = vec![
            frame(0, &[1, 2], &[(1, "frames/a.png"), (2, "frames/b.png")]),
            frame(2, &[1], &[]),
            frame(3, &[2], &[(2, "frames/c.png")]),
            frame(5, &[1, 2], &[(1, "frames/d.png")]),
        ];
        let at = |k: usize| run.trace.frames[k].clone();
        assert_eq!(image_of(&run, &at(1), 1), Some("frames/a.png"));
        assert_eq!(image_of(&run, &at(2), 2), Some("frames/c.png"));
        assert_eq!(image_of(&run, &at(3), 2), Some("frames/c.png"));
        assert_eq!(image_of(&run, &at(3), 1), Some("frames/d.png"));
        assert_eq!(image_of(&run, &at(0), 3), None);
    }
}

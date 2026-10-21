//! (replay-owner) The replay's verdict: the boxes the live shell drew
//! at the report (`drawn.json`, each mapped surface's nodes as its last
//! frame drew them) held, node by node, to the replay's traced frame of
//! the same surface at the report (docs/eval.md, "Replaying a glitch
//! report", "The verdict").
//!
//! A node is matched by its surface (its root's origin), its origin, its
//! `id:` and its `for` row key, the n-th live one of those to the n-th
//! the replay drew, in paint order. Its box is the drawn one through
//! every transform, axis aligned, in surface logical px; its opacity its
//! own. VERIFIED: every node of both is in the other, each within
//! [`PX`] on every edge and [`OPACITY`]. DRIFTED: anything else, each
//! difference named.

use serde_json::{Value as Json, json};

use super::bundle::Bundle;
use super::fidelity::same_origin;
use super::plan::Plan;
use crate::clock::T0;
use crate::harness::{NodeRef, RunOutput};

/// How far apart an edge may be, logical px.
pub const PX: f64 = 1.0;
/// How far apart an opacity may be.
pub const OPACITY: f64 = 0.02;

/// One node as drawn: its name and box.
#[derive(Clone, Debug, PartialEq)]
pub struct Drawn {
    pub origin: String,
    pub id: Option<String>,
    pub key: Option<String>,
    /// `[x, y, w, h]`, surface logical px.
    pub rect: [f64; 4],
    pub opacity: f64,
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
        let [x, y, w, h] = self.rect;
        let [a, b, c, d] = o.rect;
        [
            (x - a).abs(),
            (y - b).abs(),
            ((x + w) - (a + c)).abs(),
            ((y + h) - (b + d)).abs(),
        ]
        .into_iter()
        .fold(0.0, f64::max)
    }
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
}

/// The verdict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Surfaces compared, and their nodes.
    pub surfaces: usize,
    pub nodes: usize,
    pub drifts: Vec<Drift>,
    /// Nodes the live shell drew that the replay did not, and the other
    /// way, with their surface's origin.
    pub live_only: Vec<(String, Drawn)>,
    pub replay_only: Vec<(String, Drawn)>,
    /// Live surfaces with no replayed frame to hold them to.
    pub missing: Vec<String>,
    /// Why nothing could be compared (a bundle with no `drawn.json`).
    pub unchecked: Option<String>,
    /// The replay's own boxes at the report: each surface shown then
    /// (its root's origin) and its nodes, in paint order.
    pub replayed: Vec<(String, Vec<Drawn>)>,
}

impl Verdict {
    pub fn verified(&self) -> bool {
        self.unchecked.is_none()
            && self.drifts.is_empty()
            && self.live_only.is_empty()
            && self.replay_only.is_empty()
            && self.missing.is_empty()
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

    /// The verdict in one plain line.
    pub fn line(&self) -> String {
        if let Some(why) = &self.unchecked {
            return format!("NOT CHECKED: {why}");
        }
        if self.verified() {
            return format!(
                "VERIFIED: the replay drew all {} nodes on {} surface{} where the live shell \
                 did (within {PX} px and {OPACITY} opacity)",
                self.nodes,
                self.surfaces,
                if self.surfaces == 1 { "" } else { "s" }
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
        if !self.missing.is_empty() {
            parts.push(format!(
                "{} surface{} the replay never drew",
                self.missing.len(),
                if self.missing.len() == 1 { "" } else { "s" }
            ));
        }
        let first = self.lines().into_iter().next().unwrap_or_default();
        format!("DRIFTED: {}; first: {first}", parts.join(", "))
    }

    /// One line per difference.
    pub fn lines(&self) -> Vec<String> {
        let rect = |r: [f64; 4]| format!("[{:.1}, {:.1}, {:.1}, {:.1}]", r[0], r[1], r[2], r[3]);
        let mut out = Vec::new();
        for s in &self.missing {
            out.push(format!(
                "surface @{s}: the replay drew no frame of it at the report"
            ));
        }
        for d in &self.drifts {
            out.push(format!(
                "{} (surface @{}): live {} opacity {:.2}, replay {} opacity {:.2}: {:.1} px off{}",
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
                }
            ));
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
        out
    }

    /// `fidelity.json`'s `verdict` part.
    pub fn json(&self) -> Json {
        let node = |n: &Drawn| json!({"origin": n.origin, "id": n.id, "key": n.key, "box": n.rect, "opacity": n.opacity});
        json!({
            "verdict": self.word(),
            "line": self.line(),
            "surfaces": self.surfaces,
            "nodes": self.nodes,
            "tolerance": {"px": PX, "opacity": OPACITY},
            "unchecked": self.unchecked,
            "drifted": self.drifts.iter().map(|d| json!({
                "surface": d.surface,
                "node": d.live.name(),
                "live": node(&d.live),
                "replay": node(&d.replay),
                "px": d.px,
                "opacity": d.opacity,
            })).collect::<Vec<_>>(),
            "live_only": self.live_only.iter().map(|(s, n)| json!({"surface": s, "node": n.name(), "live": node(n)})).collect::<Vec<_>>(),
            "replay_only": self.replay_only.iter().map(|(s, n)| json!({"surface": s, "node": n.name(), "replay": node(n)})).collect::<Vec<_>>(),
            "missing": self.missing,
        })
    }
}

/// An origin as the bundle wrote it, with the live config directory
/// taken off (a test bundle may name it).
fn local(b: &Bundle, o: &str) -> String {
    b.live_config_dir()
        .and_then(|d| {
            o.strip_prefix(&format!("{}/", d.display()))
                .map(str::to_string)
        })
        .unwrap_or_else(|| o.to_string())
}

/// The replay's nodes of surface `sid` on its last traced frame at or
/// before the report (with a period's give), named by `run.log.refs`.
fn replayed(run: &RunOutput, plan: &Plan, sid: u32) -> Option<Vec<Drawn>> {
    let len = plan.end - plan.start;
    let fr = run.trace.frames.iter().rev().find(|fr| {
        fr.t_ns <= (T0.as_nanos() as u64) + len + plan.period && fr.painted.contains(&sid)
    })?;
    Some(
        fr.n.iter()
            .filter(|r| r.surface == sid)
            .filter_map(|r| {
                let n = run.log.refs.get(&r.key)?;
                Some(Drawn {
                    origin: n.origin.clone(),
                    id: n.id.clone(),
                    key: n.key.clone(),
                    rect: r.visual.map(f64::from),
                    opacity: f64::from(r.own_op),
                })
            })
            .collect(),
    )
}

/// Holds the replay's frame at the report to `drawn.json`.
pub fn compare(b: &Bundle, plan: &Plan, run: &RunOutput) -> Verdict {
    let mut v = Verdict::default();
    for t in run.trace.surfaces.iter().filter(|t| t.detached.is_none()) {
        let origin = run
            .trace
            .nodes
            .get(&t.root)
            .and_then(|m| m.origin.clone())
            .unwrap_or_else(|| t.root.clone());
        if let Some(nodes) = replayed(run, plan, t.id) {
            v.replayed.push((origin, nodes));
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
    let trace = &run.trace;
    let root_origin = |root: &str| {
        trace
            .nodes
            .get(root)
            .and_then(|m| m.origin.clone())
            .or_else(|| root.split('|').nth(1).map(str::to_string))
    };
    for s in surfaces {
        let slot = s["slot"].as_u64();
        let rec = b.surfaces.iter().find(|r| Some(u64::from(r.slot)) == slot);
        // Only the output replayed (the sandbox has one screen).
        if rec.is_some_and(|r| r.output.as_deref().is_some_and(|o| o != plan.output.name)) {
            continue;
        }
        let root = s["root"].as_str().unwrap_or("");
        let Some(origin) = b.nodes.get(root).and_then(|n| n["origin"].as_str()) else {
            v.missing
                .push(format!("{root} (its root is not in nodes.json)"));
            continue;
        };
        let origin = local(b, origin);
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
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sid = trace
            .surfaces
            .iter()
            .filter(|t| t.detached.is_none())
            .find(|t| root_origin(&t.root).is_some_and(|o| same_origin(&o, &origin)))
            .map(|t| t.id);
        let Some(replay) = sid.and_then(|id| replayed(run, plan, id)) else {
            v.missing.push(origin);
            continue;
        };
        v.surfaces += 1;
        v.nodes += live.len();
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
            let px = l.px_off(r);
            let op = (l.opacity - r.opacity).abs();
            if px > PX || op > OPACITY {
                v.drifts.push(Drift {
                    surface: origin.clone(),
                    replay: r.clone(),
                    live: l,
                    px,
                    opacity: op,
                });
            }
        }
        for (i, r) in replay.into_iter().enumerate() {
            if !used[i] {
                v.replay_only.push((origin.clone(), r));
            }
        }
    }
    if v.surfaces == 0 && v.missing.is_empty() {
        v.unchecked = Some(format!(
            "drawn.json has no surface on {}, the output replayed",
            plan.output.name
        ));
    }
    v
}

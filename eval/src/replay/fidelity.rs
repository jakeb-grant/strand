//! The replay held to what the live shell recorded (docs/eval.md,
//! "Replaying a glitch report", "The comparison with the live shell"):
//! each press's hit, each surface root mapped and gone, each cell write,
//! and the frame at the report against the bundle's PNGs.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::bundle::Bundle;
use super::plan::Plan;
use crate::checks::cpu_gpu::{EDGE_SHARE, GPU_TOLERANCE};
use crate::clock::T0;
use crate::harness::{NodeRef, RunOutput};
use crate::trace::RunTrace;

/// Two frames: how far apart a surface may map in the two runs.
pub const MAP_FRAMES: u64 = 2;

/// Where the replay parts from the live shell.
#[derive(Clone, Debug, PartialEq)]
pub struct Divergence {
    /// Since the window's start, ns.
    pub t: u64,
    /// `press`, `step`, `surface`, `cell` or `frame`.
    pub what: &'static str,
    pub text: String,
}

/// The bundle's PNG of a surface against the replay's frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDiff {
    /// The bundle's PNG (relative to it).
    pub png: String,
    /// The surface root's origin.
    pub origin: String,
    /// (fidelity) The output the live surface was on.
    pub output: Option<String>,
    /// The replay's tick it is held to (`None`: no frame of it).
    pub k: Option<u32>,
    /// Pixels past the `cpu-gpu` tolerance, of `total`.
    pub over: u64,
    pub total: u64,
    /// Their bounding box, buffer px: `[x, y, w, h]`.
    pub bbox: Option<[u32; 4]>,
    /// Why there is no comparison (sizes differ, no such surface).
    pub why: Option<String>,
}

impl FrameDiff {
    pub fn share(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.over as f64 / self.total as f64
        }
    }

    /// Whether it parts from the live frame: past `EDGE_SHARE`, or not
    /// compared at all.
    pub fn diverges(&self) -> bool {
        self.why.is_some() || self.share() > EDGE_SHARE
    }
}

/// The comparison.
#[derive(Clone, Debug, Default)]
pub struct Fidelity {
    /// Every divergence, in time order.
    pub divergences: Vec<Divergence>,
    pub frames: Vec<FrameDiff>,
    /// Calls the mock answered itself: method and how many.
    pub mock_answered: BTreeMap<String, usize>,
    pub presses: (usize, usize),
    pub writes: (usize, usize),
    /// (report review) What the comparison could not tell: a live press
    /// whose hit the bundle cannot name.
    pub notes: Vec<String>,
}

/// Whether two origins name the same place (one may be under a folder
/// the other leaves out).
pub fn same_origin(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn same_node(a: &Option<NodeRef>, b: &Option<NodeRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_origin(&a.origin, &b.origin) && a.key == b.key,
        _ => false,
    }
}

fn show(n: &Option<NodeRef>) -> String {
    n.as_ref()
        .map_or_else(|| "nothing".into(), ToString::to_string)
}

/// A replay time as ns since the window's start.
fn since_start(t: Duration) -> u64 {
    t.saturating_sub(T0).as_nanos() as u64
}

/// The origin of a trace surface's root.
fn root_origin(trace: &RunTrace, root: &str) -> Option<String> {
    trace
        .nodes
        .get(root)
        .and_then(|m| m.origin.clone())
        .or_else(|| root.split('|').nth(1).map(str::to_string))
}

fn premul(p: &[u8]) -> [u8; 4] {
    let a = u16::from(p[3]);
    let m = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
    [m(p[0]), m(p[1]), m(p[2]), p[3]]
}

/// Compares two straight-alpha RGBA buffers in premultiplied alpha:
/// pixels past `GPU_TOLERANCE` in any channel and their bounding box.
pub fn diff_pixels(w: u32, a: &[u8], b: &[u8]) -> (u64, Option<[u32; 4]>) {
    diff_pixels_masked(w, a, b, &[])
}

/// (replay-owner) [`diff_pixels`] leaving out the pixels inside any of
/// `mask` (`[x, y, w, h]`, buffer px).
pub fn diff_pixels_masked(
    w: u32,
    a: &[u8],
    b: &[u8],
    mask: &[[u32; 4]],
) -> (u64, Option<[u32; 4]>) {
    let mut over = 0u64;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let masked = |x: u32, y: u32| {
        mask.iter().any(|m| {
            x >= m[0] && y >= m[1] && x < m[0].saturating_add(m[2]) && y < m[1].saturating_add(m[3])
        })
    };
    for (i, (pa, pb)) in a.chunks_exact(4).zip(b.chunks_exact(4)).enumerate() {
        let (pa, pb) = (premul(pa), premul(pb));
        if pa
            .iter()
            .zip(&pb)
            .any(|(x, y)| x.abs_diff(*y) > GPU_TOLERANCE)
        {
            let (x, y) = (i as u32 % w.max(1), i as u32 / w.max(1));
            if masked(x, y) {
                continue;
            }
            over += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    let bbox = (over > 0).then(|| [x0, y0, x1 - x0 + 1, y1 - y0 + 1]);
    (over, bbox)
}

/// Holds `run` to what `b` recorded, as `plan` read it.
pub fn compare(b: &Bundle, plan: &Plan, run: &RunOutput) -> Fidelity {
    let mut f = Fidelity::default();
    let period = plan.period.max(1);
    let traced_at = (plan.end - plan.start).saturating_sub(plan.traced_ns);

    // Presses, in order.
    let replay = &run.log.presses;
    f.presses = (plan.presses.len(), replay.len());
    for (i, live) in plan.presses.iter().enumerate() {
        let Some(r) = replay.get(i) else {
            f.divergences.push(Divergence {
                t: live.t,
                what: "press",
                text: format!(
                    "press {}: the replay never pressed (it pressed {} times)",
                    i + 1,
                    replay.len()
                ),
            });
            break;
        };
        if let Some(u) = &live.unknown {
            // (report review) A hit the bundle cannot name is no evidence
            // either way: it is noted, not a divergence.
            f.notes.push(format!(
                "press {}: the live hit {u} is not in nodes.json, so it is not compared; the replay hit {}",
                i + 1,
                show(&r.hit)
            ));
            continue;
        }
        if !same_node(&live.hit, &r.hit) {
            f.divergences.push(Divergence {
                t: live.t,
                what: "press",
                text: format!(
                    "press {}: live hit {}, the replay hit {}",
                    i + 1,
                    show(&live.hit),
                    show(&r.hit)
                ),
            });
        }
    }
    if replay.len() > plan.presses.len() {
        let r = &replay[plan.presses.len()];
        f.divergences.push(Divergence {
            t: since_start(r.t),
            what: "press",
            text: format!(
                "the replay pressed {} times, the live shell {}",
                replay.len(),
                plan.presses.len()
            ),
        });
    }

    // (report review) Steps the sandbox could not take.
    for sk in &run.log.skipped {
        f.divergences.push(Divergence {
            t: since_start(sk.t),
            what: "step",
            text: format!("the replay could not take line {}: {}", sk.line, sk.msg),
        });
    }

    // Surfaces mapped and gone, in the traced part.
    let trace = &run.trace;
    let k0 = trace.frames.first().map_or(0, |fr| fr.k);
    let mut used = vec![(false, false); trace.surfaces.len()];
    let tick_t = |k: u32| u64::from(k) * period;
    for m in plan.maps.iter().filter(|m| m.t >= traced_at) {
        // A surface the live shell mapped as it started, before any
        // input, is one the sandbox's boot shows on its first frame.
        let booting = m.mapped && m.t < plan.boot_until;
        let near = trace.surfaces.iter().enumerate().find(|(i, s)| {
            let at = if m.mapped {
                Some(s.attached)
            } else {
                s.detached
            };
            let taken = if m.mapped { used[*i].0 } else { used[*i].1 };
            let close = at.is_some_and(|k| {
                tick_t(k).abs_diff(m.t) <= MAP_FRAMES * period || (booting && k <= k0)
            });
            // (fidelity) On the same output, when both say.
            let same_output = match (&s.output, &m.output) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            !taken
                && close
                && same_output
                && root_origin(trace, &s.root).is_some_and(|o| same_origin(&o, &m.origin))
        });
        match near {
            Some((i, _)) => {
                if m.mapped {
                    used[i].0 = true;
                } else {
                    used[i].1 = true;
                }
            }
            None => f.divergences.push(Divergence {
                t: m.t,
                what: "surface",
                text: format!(
                    "surface @{}{} {} live; the replay's did not within {MAP_FRAMES} frames",
                    m.origin,
                    super::plan::on_words(&m.output, plan.outputs.len()),
                    if m.mapped { "mapped" } else { "went" }
                ),
            }),
        }
    }
    for (i, s) in trace.surfaces.iter().enumerate() {
        let origin = root_origin(trace, &s.root).unwrap_or_default()
            + &super::plan::on_words(&s.output, plan.outputs.len());
        if s.attached > k0 && !used[i].0 && tick_t(s.attached) >= traced_at {
            f.divergences.push(Divergence {
                t: tick_t(s.attached),
                what: "surface",
                text: format!("surface @{origin} mapped in the replay only"),
            });
        }
        if let Some(d) = s.detached
            && !used[i].1
            && tick_t(d) >= traced_at
        {
            f.divergences.push(Divergence {
                t: tick_t(d),
                what: "surface",
                text: format!("surface @{origin} went in the replay only"),
            });
        }
    }

    // Cell writes: each live one against the replay's next of the path.
    // (fidelity) In passes, so a write the ring kept whole takes its
    // own first: each live write with its value against the replay's
    // write of the same path and value nearest it in time, within
    // MAP_FRAMES frames, then against the next of them at any time (so
    // a live write the replay did not make takes no later write that
    // matches one it did, and a divergence is at the write that
    // parted); then each whose value was lost
    // against the replay's write of its path nearest it in time, within
    // MAP_FRAMES frames; then each whose name was lost too against the
    // nearest write of a cell whose name the ring's table does not
    // hold; last, each live write still unmatched against the replay's
    // next write of its path, or none.
    let writes = &run.log.writes;
    f.writes = (plan.writes.len(), writes.len());
    let mut taken = vec![false; writes.len()];
    let mut done = vec![false; plan.writes.len()];
    let near = MAP_FRAMES * period;
    let value_of = |r: &crate::harness::WriteLog| serde_json::from_str::<Json>(&r.value).ok();
    for close in [true, false] {
        for (wi, w) in plan.writes.iter().enumerate() {
            let (Some(path), Some(value)) = (&w.path, &w.value) else {
                continue;
            };
            if done[wi] {
                continue;
            }
            let mut same = writes.iter().enumerate().filter(|(i, r)| {
                !taken[*i] && r.path == *path && value_of(r).as_ref() == Some(value)
            });
            let hit = if close {
                same.filter(|(_, r)| since_start(r.t).abs_diff(w.t) <= near)
                    .min_by_key(|(_, r)| since_start(r.t).abs_diff(w.t))
            } else {
                same.next()
            };
            if let Some((i, _)) = hit {
                taken[i] = true;
                done[wi] = true;
            }
        }
    }
    let mut by_time = 0usize;
    for pass in 0..2 {
        for (wi, w) in plan.writes.iter().enumerate() {
            if done[wi] || w.value.is_some() {
                continue;
            }
            let fits = |r: &crate::harness::WriteLog| match (&w.path, pass) {
                (Some(p), 0) => r.path == *p,
                (None, 1) => !plan.interned.contains(&r.path),
                _ => false,
            };
            let hit = writes
                .iter()
                .enumerate()
                .filter(|(i, r)| !taken[*i] && fits(r) && since_start(r.t).abs_diff(w.t) <= near)
                .min_by_key(|(_, r)| since_start(r.t).abs_diff(w.t));
            if let Some((i, _)) = hit {
                taken[i] = true;
                done[wi] = true;
                by_time += 1;
            }
        }
    }
    if by_time > 0 {
        f.notes.push(format!(
            "{by_time} live cell write{} whose value (or name and value) the ring lost {} held \
             to the replay's write nearest in time (within {MAP_FRAMES} frames), not by value",
            if by_time == 1 { "" } else { "s" },
            if by_time == 1 { "is" } else { "are" }
        ));
    }
    for (wi, w) in plan.writes.iter().enumerate() {
        if done[wi] {
            continue;
        }
        let text = match (&w.path, &w.value) {
            (Some(path), Some(value)) => {
                let other = writes.iter().enumerate().find(|(i, r)| {
                    !taken[*i] && r.path == *path && since_start(r.t) + 2 * period >= w.t
                });
                match other {
                    Some((i, r)) => {
                        taken[i] = true;
                        format!("cell {path} = {value} live, the replay wrote {}", r.value)
                    }
                    None => format!("cell {path} = {value} live, the replay never wrote it"),
                }
            }
            (Some(path), None) => format!(
                "cell {path} changed live (its value was not kept), the replay did not write it \
                 within {MAP_FRAMES} frames"
            ),
            (None, _) => format!(
                "a cell changed live (its name and value were lost from the ring's arena), the \
                 replay wrote no cell whose name the ring's table does not hold within \
                 {MAP_FRAMES} frames"
            ),
        };
        f.divergences.push(Divergence {
            t: w.t,
            what: "cell",
            text,
        });
    }
    for (i, r) in writes.iter().enumerate() {
        if !taken[i] && since_start(r.t) >= traced_at {
            f.divergences.push(Divergence {
                t: since_start(r.t),
                what: "cell",
                text: format!("cell {} = {} in the replay only", r.path, r.value),
            });
        }
    }

    // The frame at the report against each bundle PNG, (fidelity) on
    // every output:
    // (replay-owner) the replay's frame nearest the time the live one
    // was drawn, never one after the report, of the surface whose root
    // is the same node (`drawn::frame_for`, `drawn::match_surfaces`).
    let len = plan.end - plan.start;
    let matched = super::drawn::match_surfaces(b, plan, run);
    let origin_of_root = |root: &str| -> Option<String> {
        b.nodes
            .get(root)
            .and_then(|n| n["origin"].as_str())
            .map(|o| {
                b.live_config_dir()
                    .and_then(|d| {
                        o.strip_prefix(&format!("{}/", d.display()))
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| o.to_string())
            })
    };
    for s in &b.surfaces {
        let Some(png) = &s.frame else {
            continue;
        };
        let Some(origin) = origin_of_root(&s.root) else {
            f.frames.push(FrameDiff {
                png: png.clone(),
                origin: String::new(),
                output: s.output.clone(),
                k: None,
                over: 0,
                total: 0,
                bbox: None,
                why: Some(format!("its root {} is not in nodes.json", s.root)),
            });
            continue;
        };
        let mut d = FrameDiff {
            png: png.clone(),
            origin: origin.clone(),
            output: s.output.clone(),
            k: None,
            over: 0,
            total: 0,
            bbox: None,
            why: None,
        };
        let sid = matched.get(&s.slot).copied().flatten();
        let last = sid.and_then(|id| {
            let fr =
                super::drawn::frame_for(run, plan, id, super::drawn::live_drawn_ns(b, s.slot))?;
            super::drawn::image_of(run, fr, id).map(|p| (fr.k, p.to_string()))
        });
        let live = std::fs::read(b.dir.join(png))
            .ok()
            .and_then(|bytes| crate::report::png::decode(&bytes).ok());
        match (last, live) {
            (None, _) => d.why = Some("the replay drew no frame of it".into()),
            (_, None) => d.why = Some(format!("{png} does not decode")),
            (Some((k, name)), Some((lw, lh, lpx))) => {
                d.k = Some(k);
                match run
                    .images
                    .get(&name)
                    .and_then(|i| crate::report::png::decode(&i.png).ok())
                {
                    None => d.why = Some("the replay's image was not kept".into()),
                    Some((w, h, px)) if (w, h) != (lw, lh) => {
                        d.why = Some(format!(
                            "the replay's frame is {w}x{h}, the live one {lw}x{lh}"
                        ));
                        let _ = px;
                    }
                    Some((w, h, px)) => {
                        let (over, bbox) = diff_pixels(w, &lpx, &px);
                        d.over = over;
                        d.total = u64::from(w) * u64::from(h);
                        d.bbox = bbox;
                    }
                }
            }
        }
        if d.diverges() {
            let on = super::plan::on_words(&s.output, plan.outputs.len());
            let text = match &d.why {
                Some(w) => format!("frame {png} (@{origin}{on}): {w}"),
                None => format!(
                    "frame {png} (@{origin}{on}): {:.1}% of the pixels differ past {GPU_TOLERANCE}, in {}",
                    d.share() * 100.0,
                    d.bbox.map_or_else(String::new, |b| format!(
                        "{}x{} at {},{}",
                        b[2], b[3], b[0], b[1]
                    ))
                ),
            };
            f.divergences.push(Divergence {
                t: len,
                what: "frame",
                text,
            });
        }
        f.frames.push(d);
    }

    for (_, m, queued) in &run.log.fetches {
        if !queued {
            *f.mock_answered.entry(m.clone()).or_default() += 1;
        }
    }
    f.divergences.sort_by_key(|d| d.t);
    f
}

impl Fidelity {
    /// `fidelity.json`.
    pub fn json(&self, b: &Bundle, plan: &Plan, limits: &[String]) -> Json {
        let len = plan.end - plan.start;
        let rel = |t: u64| (t as f64 - len as f64) / 1e9;
        json!({
            "bundle": b.dir.display().to_string(),
            "words": b.words,
            "window": {
                "seconds": len as f64 / 1e9,
                "starts_after": plan.start_why,
                "traced_seconds": plan.traced_ns as f64 / 1e9,
            },
            "output": {
                "name": plan.output.name,
                "size": [plan.output.width, plan.output.height],
                "scale": plan.output.scale,
                "hz": plan.output.hz,
                "why": plan.output.why,
            },
            // (fidelity) Every output replayed, in `screens.all`'s order.
            "outputs": plan.outputs.iter().map(|o| json!({
                "name": o.name,
                "id": o.id,
                "size": [o.width, o.height],
                "scale": o.scale,
                "hz": o.hz,
                "at": o.at,
                "main": o.name == plan.output.name,
            })).collect::<Vec<_>>(),
            "faithful": self.divergences.is_empty(),
            "first": self.divergences.first().map(|d| json!({"t": rel(d.t), "what": d.what, "text": d.text})),
            "divergences": self.divergences.iter().map(|d| json!({"t": rel(d.t), "what": d.what, "text": d.text})).collect::<Vec<_>>(),
            "presses": {"live": self.presses.0, "replay": self.presses.1},
            "cell_writes": {"live": self.writes.0, "replay": self.writes.1},
            "frames": self.frames.iter().map(|d| json!({
                "png": d.png,
                "origin": d.origin,
                "output": d.output,
                "k": d.k,
                "over": d.over,
                "total": d.total,
                "share": d.share(),
                "bbox": d.bbox,
                "why": d.why,
            })).collect::<Vec<_>>(),
            "limits": limits,
            "notes": plan.notes.iter().chain(&self.notes).collect::<Vec<_>>(),
        })
    }
}

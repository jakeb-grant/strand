//! The surface leg's checks (docs/eval.md, "Surface leg";
//! docs/decisions.md eval-surface-leg), over what the fake compositor
//! received ([`crate::wire::WireLog`]):
//!
//! - `regions-on-wire`: after each commit that carries a frame, and each
//!   bare commit after it, the compositor holds the regions the
//!   renderer's painter computed for that frame, on either path (the
//!   manager's shm commits and the GPU thread's presents): its opaque
//!   region (logical, rounded in), the input region it asked for (rounded
//!   out; none asked: the manager's own, not compared) and its blur
//!   region, compared as the pixels they cover; and its buffer is the
//!   frame's size. design.md makes each the frame's own: `set_opaque_region`
//!   with every frame ("Damage and pacing"), the blur region following the
//!   rounded shape ("Blur ladder"), and spec.md 6.7's hover popup taking
//!   input only where it draws this frame. A region one frame behind is a
//!   hover lost or a blur on nothing (owner-testing.md bug 23). A frame
//!   whose surface the compositor poses at a scale (`compositor_poses`)
//!   sends its regions in the pose's coordinates and is not compared.
//!   (eval-r3) A frame that drew nothing (no damage, so no buffer
//!   commit) still asks for its regions, which the manager sends with a
//!   bare commit: a bare commit after such a frame is held to it, not to
//!   the last frame a buffer carried.
//!   (eval-rules-6) A layer surface's margins after the commit that
//!   carries a frame (or the bare one that sends a frame that drew
//!   nothing) are the ones its state asks for at rest moved by the
//!   frame's pose's offset as layer shell reads margins
//!   (`wire::posed_margins`), within half a pixel; (integration-r7)
//!   after a bare commit following such a frame (the last of its tick:
//!   one replaced in the same tick is never composited), the ones its
//!   state asks for at that commit's tick's end (`WireLog::held`), or the
//!   frame's own in its tick:
//!   the offset a compositor pose delegates goes out with its frame's
//!   commit (design.md "Poses"), on the side the surface is anchored to.
//! - `late-present`: every frame that drew is committed in the tick it
//!   was painted for and shown by that tick's refresh: not later, not
//!   replaced by another before it, not never. Frames lock to the
//!   refresh (design.md, "Damage and pacing": timing comes from
//!   `wp_presentation`), and render samples each at the time it is
//!   painted for, so one shown a refresh late shows the animation a frame
//!   behind. The frames of a tick in which the manager's throttle gave
//!   up (`WireLog::gave_up`: a second of real time without a refresh,
//!   which only a run that slow sees) and of the tick after it are not
//!   judged: that give-up is the machine's pace, not the virtual clock's.

use std::collections::HashMap;

use super::Finding;
use crate::scenario::CheckId;
use crate::trace::RunTrace;
use crate::wire::{Path, R, WireFrame};

fn path(p: Option<Path>) -> &'static str {
    match p {
        Some(Path::Shm) => "shm",
        Some(Path::Gpu) => "GPU",
        None => "?",
    }
}

fn label(f: &WireFrame) -> String {
    format!("surface {} ({} frame of k{})", f.surface, path(f.path), f.k)
}

/// Rects as `x,y wxh` for messages; more than four as their count and
/// bounds.
fn rects(v: &[R]) -> String {
    if v.is_empty() {
        return "none".into();
    }
    if v.len() > 4 {
        let x0 = v.iter().map(|r| r[0]).min().unwrap_or(0);
        let y0 = v.iter().map(|r| r[1]).min().unwrap_or(0);
        let x1 = v.iter().map(|r| r[0] + r[2]).max().unwrap_or(0);
        let y1 = v.iter().map(|r| r[1] + r[3]).max().unwrap_or(0);
        return format!("{} rects within {x0},{y0} {}x{}", v.len(), x1 - x0, y1 - y0);
    }
    v.iter()
        .map(|[x, y, w, h]| format!("{x},{y} {w}x{h}"))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// The root node key of each surface of `trace`, for the findings.
fn roots(trace: &RunTrace) -> HashMap<u32, &str> {
    trace
        .surfaces
        .iter()
        .map(|s| (s.id, s.root.as_str()))
        .collect()
}

/// `regions-on-wire` over `trace`'s surface leg (none: no findings).
pub fn regions_on_wire(trace: &RunTrace) -> Vec<Finding> {
    let Some(log) = &trace.wire else {
        return Vec::new();
    };
    let roots = roots(trace);
    let mut out = Vec::new();
    // The frame each commit carries, by commit index.
    let carried: HashMap<usize, &WireFrame> = log
        .frames
        .iter()
        .filter_map(|f| Some((f.commit?, f)))
        .collect();
    // The frame each surface's compositor state comes from now.
    let mut now: HashMap<u32, &WireFrame> = HashMap::new();
    for (i, c) in log.commits.iter().enumerate() {
        let Some(s) = c.surface else { continue };
        let fresh = carried.get(&i).copied();
        if let Some(f) = fresh {
            now.insert(s, f);
        } else if let Some(f) = log.frames.iter().rfind(|f| {
            f.surface == s
                && f.commit.is_none()
                && f.k <= c.k
                && now.get(&s).is_none_or(|n| f.k > n.k)
        }) {
            // The latest frame painted since that drew nothing: its
            // regions go out with this bare commit.
            now.insert(s, f);
        }
        let Some(f) = now.get(&s).copied() else {
            continue;
        };
        if f.posed {
            continue;
        }
        let what = match fresh {
            Some(_) => format!("commit {i}, which carries it,"),
            None => format!("commit {i} (bare, k{}) after it", c.k),
        };
        let mut fail = |kind: &'static str, msg: String| {
            let mut finding =
                Finding::fail(CheckId::RegionsOnWire, format!("{}: {msg}", label(f))).at(f.k);
            if let Some(root) = roots.get(&f.surface) {
                finding = finding.node(root);
            }
            out.push((f.surface, kind, finding));
        };
        if fresh.is_some()
            && let Some(b) = c.buffer
            && b != f.size
        {
            fail(
                "buffer",
                format!(
                    "{what} attaches a {}x{} buffer; the frame was painted at {}x{}",
                    b[0], b[1], f.size[0], f.size[1]
                ),
            );
        }
        if c.opaque != f.opaque {
            fail(
                "opaque",
                format!(
                    "after {what} the compositor's opaque region is {}; the painter's is {}",
                    rects(&c.opaque),
                    rects(&f.opaque)
                ),
            );
        }
        if let Some(want) = &f.input {
            let got = c.input.as_deref();
            if got != Some(want.as_slice()) {
                fail(
                    "input",
                    format!(
                        "after {what} the compositor's input region is {}; the painter asked for {}",
                        got.map_or("the whole surface".into(), rects),
                        rects(want)
                    ),
                );
            }
        }
        // (eval-rules-6) Its margins: where its pose's offset puts it.
        // Held on the commit that carries the frame, or the bare one
        // that sends a frame that drew nothing, to that frame's state.
        // (integration-r7) A bare commit after a frame a buffer carried
        // may send a state the tick's paint changed after it (a new
        // shadow's overhang moves the margins): it is held to the state
        // of its moment, its tick's end or start (`WireLog::held`), or,
        // in the frame's own tick, to the frame's; never left unjudged, so a
        // manager that sends the rest margins, or stale posed ones, on
        // such a commit fails.
        let near = |a: &[i32; 4], w: [f32; 4]| {
            a.iter()
                .zip(w)
                .all(|(g, w)| (f64::from(*g) - f64::from(w)).abs() <= 0.5 + 1e-3)
        };
        // The state at its tick's end, and at the end of the tick before
        // (what it was as its tick began).
        let held = |k: u32| {
            log.held
                .iter()
                .rfind(|h| h.surface == s && h.k <= k)
                .map(|h| h.margin)
        };
        // (integration-r7) A bare commit replaced by a later one of its
        // surface in the same tick is never shown: the compositor
        // composites at the refresh, after the tick's last commit.
        let replaced = || {
            log.commits[i + 1..]
                .iter()
                .any(|d| d.surface == Some(s) && d.k == c.k)
        };
        let wants: Vec<[f32; 4]> = if fresh.is_some() || f.commit.is_none() {
            f.margin.into_iter().collect()
        } else if replaced() {
            Vec::new()
        } else {
            held(c.k)
                .into_iter()
                .chain(c.k.checked_sub(1).and_then(held))
                .chain(f.margin.filter(|_| f.k == c.k))
                .collect()
        };
        if let (Some(want), Some(got)) = (wants.first().copied(), c.margin)
            && !wants.iter().any(|w| near(&got, *w))
        {
            fail(
                "margin",
                format!(
                    "after {what} the compositor holds margins {got:?}; its state's at rest moved \
                     by its pose's offset are [{}] (layer shell: from the edges it is anchored to)",
                    want.map(|v| format!("{v:.1}")).join(", ")
                ),
            );
        }
        if c.blur != f.blur {
            fail(
                "blur",
                format!(
                    "after {what} the compositor blurs behind {}; the painter asks for {}",
                    rects(&c.blur),
                    rects(&f.blur)
                ),
            );
        }
    }
    first_per_surface(out)
}

/// `late-present` over `trace`'s surface leg (none: no findings).
pub fn late_present(trace: &RunTrace) -> Vec<Finding> {
    let Some(log) = &trace.wire else {
        return Vec::new();
    };
    let roots = roots(trace);
    let mut out = Vec::new();
    let slow = |k: u32| {
        log.gave_up
            .iter()
            .any(|g| k == *g || k == g.saturating_add(1))
    };
    for f in log.frames.iter().filter(|f| f.due && !slow(f.k)) {
        let (kind, msg) = match f.commit.and_then(|i| log.commits.get(i).map(|c| (i, c))) {
            None => (
                "never",
                "no commit carried it: it was never shown".to_string(),
            ),
            Some((i, c)) if c.superseded => (
                "replaced",
                format!(
                    "commit {i} (k{}) carried it, but another buffer replaced it before refresh k{}",
                    c.k,
                    c.shown.unwrap_or(c.k)
                ),
            ),
            Some((i, c)) => match c.shown {
                Some(v) if v == f.k => continue,
                Some(v) => (
                    "late",
                    format!(
                        "commit {i} (k{}) carried it, shown at refresh k{v}, {} late",
                        c.k,
                        plural(v.saturating_sub(f.k))
                    ),
                ),
                None => (
                    "unshown",
                    format!("commit {i} (k{}) carried it, and no refresh showed it", c.k),
                ),
            },
        };
        let mut finding =
            Finding::fail(CheckId::LatePresent, format!("{}: {msg}", label(f))).at(f.k);
        if let Some(root) = roots.get(&f.surface) {
            finding = finding.node(root);
        }
        out.push((f.surface, kind, finding));
    }
    first_per_surface(out)
}

fn plural(n: u32) -> String {
    if n == 1 {
        "1 refresh".into()
    } else {
        format!("{n} refreshes")
    }
}

/// The first finding of each surface and kind, the rest counted in
/// `more`.
fn first_per_surface(findings: Vec<(u32, &'static str, Finding)>) -> Vec<Finding> {
    let mut seen: HashMap<(u32, &'static str), usize> = HashMap::new();
    let mut out: Vec<Finding> = Vec::new();
    for (surface, kind, f) in findings {
        match seen.get(&(surface, kind)) {
            Some(&i) => out[i].more += 1,
            None => {
                seen.insert((surface, kind), out.len());
                out.push(f);
            }
        }
    }
    out
}

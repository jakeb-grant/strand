//! `regions` (opt-in): where a surface takes input follows what it draws,
//! frame by frame. docs/spec.md (section 6.7, "A `trigger: hover` popup
//! that draws nothing itself"; decisions.md m6-w2-surface) states it: such
//! a popup takes input only in the boxes of its top-level elements that
//! draw, as they are drawn this frame, and everywhere else the pointer
//! passes through. So on a frame whose surface reports a partial input
//! region (`Painter::input_region` is `Some`):
//!
//! - **covered:** every top-level element that is shown and draws
//!   content (a shown `text`, `icon` or `image` under it) is inside one
//!   of the region's rects, or else (a `hit: none` element gives its
//!   children's boxes) each of its shown children is;
//! - **follows:** every rect of the region is a box drawn this frame (a
//!   top-level element's, or one of its children's): a rect left where an
//!   element was drawn before, or before it moved, takes the pointer
//!   where nothing is.
//!
//! Boxes are compared as drawn (paint offsets in, before transforms, as
//! the region is made), within `tol`. A top-level element that draws only
//! its own background, with nothing under it, is not held to `covered`:
//! the trace cannot tell a box with a background from an empty one.

use std::collections::HashMap;

use strand_render::probe::Axes;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Rect, Row};

/// Kinds that draw whenever they are shown.
const DRAWS: [&str; 3] = ["text", "icon", "image"];

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    for f in &cx.trace.frames {
        for (s, reg) in &f.regions {
            let Some(rects) = &reg.input else { continue };
            let Some(root) = cx
                .trace
                .surfaces
                .iter()
                .find(|x| x.id == *s)
                .map(|x| x.root.as_str())
            else {
                continue;
            };
            let rows: HashMap<&str, &Row> =
                f.n.iter()
                    .filter(|r| r.surface == *s)
                    .map(|r| (r.key.as_str(), r))
                    .collect();
            let parent = |k: &str| cx.trace.nodes.get(k).and_then(|m| m.parent.as_deref());
            let kind = |k: &str| cx.trace.nodes.get(k).map_or("", |m| m.kind.as_str());
            let shown = |r: &Row| {
                let hidden = matches!(r.hidden.as_deref(), Some("opacity0" | "scale0" | "clipped"));
                !hidden && r.op >= 0.02 && r.drawn[2] * r.drawn[3] >= 1.0
            };
            // The top-level elements, and the shown rows under each.
            let top: Vec<&Row> = rows
                .values()
                .filter(|r| parent(&r.key) == Some(root))
                .copied()
                .collect();
            let under = |t: &str, r: &str| -> bool {
                let mut at = parent(r);
                while let Some(p) = at {
                    if p == t {
                        return true;
                    }
                    at = parent(p);
                }
                false
            };
            let inside = |b: &Rect| {
                rects.iter().any(|r| {
                    b[0] >= r[0] - cx.tol as f32
                        && b[1] >= r[1] - cx.tol as f32
                        && b[0] + b[2] <= r[0] + r[2] + cx.tol as f32
                        && b[1] + b[3] <= r[1] + r[3] + cx.tol as f32
                })
            };
            for t in &top {
                if !shown(t) || cx.exempt(CheckId::Regions, usize::MAX, &t.key, Axes(u8::MAX)) {
                    continue;
                }
                let draws = rows
                    .values()
                    .any(|r| shown(r) && DRAWS.contains(&kind(&r.key)) && under(&t.key, &r.key));
                if !draws || inside(&t.drawn) {
                    continue;
                }
                let children: Vec<&&Row> = rows
                    .values()
                    .filter(|r| parent(&r.key) == Some(t.key.as_str()) && shown(r))
                    .collect();
                if !children.is_empty() && children.iter().all(|c| inside(&c.drawn)) {
                    continue;
                }
                out.push(
                    Finding::fail(
                        CheckId::Regions,
                        format!(
                            "covered: drawn at {:?}, outside the input region {rects:?}",
                            t.drawn
                        ),
                    )
                    .at(f.k)
                    .node(&t.key),
                );
            }
            let same = |a: &Rect, b: &Rect| {
                a.iter()
                    .zip(b)
                    .all(|(x, y)| f64::from((x - y).abs()) <= cx.tol)
            };
            for r in rects {
                let drawn = top.iter().any(|t| {
                    same(&t.drawn, r)
                        || rows
                            .values()
                            .any(|c| parent(&c.key) == Some(t.key.as_str()) && same(&c.drawn, r))
                });
                if !drawn {
                    out.push(
                        Finding::fail(
                            CheckId::Regions,
                            format!(
                                "follows: the input region's rect {r:?} is no box drawn this frame"
                            ),
                        )
                        .at(f.k),
                    );
                }
            }
        }
    }
    first_per_node(out)
}

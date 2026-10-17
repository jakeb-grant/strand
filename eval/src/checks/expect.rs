//! `expect` steps, evaluated where they stand: over the segment they are
//! in (from its `s`, or `from +K` ticks after it) up to their tick.
//!
//! - `still <sel> [axis]`: its value (x, y, w and h without an axis)
//!   stays within `tol` of where it was at the window's start;
//! - `moves <sel> [axis]`: it leaves that;
//! - `visible` / `hidden <sel>`: it shows, or does not, at the step.

use super::{Cx, Finding, GEOMETRY, value};
use crate::scenario::CheckId;
use crate::steps::Axis;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = Vec::new();
    for x in &cx.trace.expects {
        let seg = x.segment.and_then(|i| cx.trace.segments.get(i));
        let name = seg.map_or("", |s| s.name.as_str());
        let start = seg.and_then(|s| s.s).map(|s| s + x.from);
        let fail = |msg: String| {
            Finding::fail(
                CheckId::Expect,
                format!("line {} `{}`: {msg}", x.line, x.text),
            )
            .at(x.k)
            .in_segment(name)
        };
        let now = tl.at(x.k);
        match x.what.as_str() {
            "visible" | "hidden" => {
                let want = x.what == "visible";
                for key in &x.keys {
                    let shown = now.is_some_and(|i| tl.visible_key(i, key));
                    if shown != want {
                        out.push(
                            fail(format!(
                                "it is {}",
                                if shown { "shown" } else { "not shown" }
                            ))
                            .node(key),
                        );
                    }
                }
            }
            what => {
                let axes: Vec<Axis> = match x.axis.as_deref().and_then(Axis::parse) {
                    Some(a) => vec![a],
                    None => GEOMETRY.to_vec(),
                };
                let base = match start {
                    Some(s) => tl.at(s),
                    None => tl.within(None, x.k).next(),
                };
                let frames: Vec<usize> = tl.within(start, x.k).collect();
                for key in &x.keys {
                    let Some(b) = base.and_then(|i| tl.row(i, key)) else {
                        out.push(fail("it is not drawn at the window's start".into()).node(key));
                        continue;
                    };
                    let mut moved = false;
                    let mut broke = None;
                    for i in &frames {
                        let Some(r) = tl.row(*i, key) else { continue };
                        for a in &axes {
                            let (v0, v) = (value(b, *a), value(r, *a));
                            let give = match a {
                                Axis::Opacity => 0.01,
                                Axis::Scale => 0.005,
                                _ => cx.tol,
                            };
                            if (v - v0).abs() > give {
                                moved = true;
                                if broke.is_none() {
                                    broke = Some((tl.ks[*i], *a, v, v0, give));
                                }
                            }
                        }
                    }
                    match (what, broke) {
                        ("still", Some((k, a, v, v0, give))) => out.push(
                            fail(format!("it moved on {}", a.name()))
                                .node(key)
                                .at(k)
                                .axis(a.name())
                                .got(v, [v0 - give, v0 + give]),
                        ),
                        ("moves", None) if !moved => {
                            out.push(fail("it did not move".into()).node(key))
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

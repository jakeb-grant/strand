//! `budget`: per frame and surface, the deterministic costs stay under
//! the scenario's `[budget]` (items, records, damage, layout nodes and
//! passes). Wall times never gate.

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let b = cx.sc.budget;
    let mut out = Vec::new();
    for f in &cx.trace.frames {
        for (s, c) in &f.cost {
            for (name, got, max) in [
                ("items", u64::from(c.items), b.items),
                ("records", u64::from(c.records), b.records),
                ("damage_px", c.damage_px, b.damage_px),
                ("layout_nodes", u64::from(c.layout_nodes), b.layout_nodes),
                ("layout_passes", u64::from(c.layout_passes), b.layout_passes),
            ] {
                if let Some(max) = max
                    && got > max
                {
                    out.push(
                        Finding::fail(CheckId::Budget, format!("surface {s}: {name} over budget"))
                            .at(f.k)
                            .axis(name)
                            .got(got as f64, [0.0, max as f64]),
                    );
                }
            }
        }
    }
    first_per_node(out)
}

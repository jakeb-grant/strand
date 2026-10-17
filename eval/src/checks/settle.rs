//! `settle`: every `rest` reaches rest within its `max` ticks (no
//! surface wanting a frame, no router or logic wake due soon, no text
//! held).

use super::{Cx, Finding};
use crate::scenario::CheckId;

pub fn run(cx: &Cx) -> Vec<Finding> {
    cx.trace
        .rests
        .iter()
        .filter(|r| !r.settled)
        .map(|r| {
            Finding::fail(
                CheckId::Settle,
                format!(
                    "`rest` at line {} did not reach rest within {} ticks",
                    r.line, r.max
                ),
            )
            .at(r.k + r.ticks.saturating_sub(1))
        })
        .collect()
}

//! (M4) Time-bound values on the render thread (design.md, "Motion and
//! time"): `t`, `wave(…)` and `noise(t)` arrive as token expressions with
//! time leaves ([`strand_scene::TokenExpr::Time`] and friends), and each
//! node that reads one is evaluated at its own [`TimeContext`] every frame
//! it is drawn.
//!
//! `t` counts seconds from the first painted frame that drew the node (its
//! appearance), on the presentation clock frames are painted at. A reload
//! that keeps the node keeps its id and so its `t`; a remount is a new id
//! and starts again from 0. A frame at time zero (no clock, offline) and
//! `reduced_motion` read [`TimeContext::frozen`]: the clock stands at
//! `t = 0`, so time signals hold their value at 0 and nothing moves.
//!
//! Only paint reads time: layout resolves its props with no time context
//! (`t = 0`), so a time signal never lays a surface out again
//! (decisions.md, m4-runtime-w1).

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::{NodeId, Prop, PropValue, TimeContext};

use crate::tree::Node;

/// When each node that reads time first drew, on the presentation clock.
#[derive(Debug, Default)]
pub(crate) struct NodeTimes {
    start: HashMap<NodeId, Duration>,
}

impl NodeTimes {
    /// The time context of `id` in a frame at `frame`. A painted frame
    /// (`commit`) starts the node's clock if it has none; a preview reads
    /// an unstarted clock as 0 without starting it. `frozen` (reduced
    /// motion, or a frame with no clock) stops it at `t = 0`.
    pub(crate) fn context(
        &mut self,
        id: NodeId,
        frame: Duration,
        commit: bool,
        frozen: bool,
    ) -> TimeContext {
        let (index, count) = letter(id);
        if frozen {
            return TimeContext::frozen(index, count);
        }
        let start = if commit {
            *self.start.entry(id).or_insert(frame)
        } else {
            self.start.get(&id).copied().unwrap_or(frame)
        };
        TimeContext {
            t: frame.saturating_sub(start).as_secs_f32(),
            index,
            count,
        }
    }

    /// When the clock of `id` started, if it has.
    #[cfg(test)]
    pub(crate) fn start(&self, id: NodeId) -> Option<Duration> {
        self.start.get(&id).copied()
    }

    /// Drops the clocks of nodes `keep` rejects (gone from the tree).
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.start.retain(|id, _| keep(*id));
    }

    /// Forgets the clock of `id` (its id now names another node).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.start.remove(&id);
    }

    /// Clocks kept (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.start.len()
    }
}

/// A `letters` letter's `index` and the letters' `count`: 0 and 0 until
/// `letters` renders (S-effects), which passes its letters' own.
fn letter(_id: NodeId) -> (u32, u32) {
    (0, 0)
}

/// True if a prop of `node` reads time (its `tokens` override aside: see
/// [`overrides_read_time`]).
pub(crate) fn reads_time(node: &Node) -> bool {
    node.props
        .iter()
        .any(|e| e.prop != Prop::Tokens && e.value.reads_time())
}

/// True if `node`'s `tokens` override reads time (`set { $spin: t *
/// 20deg }`): every node in its scope is evaluated at its own time.
pub(crate) fn overrides_read_time(node: &Node) -> bool {
    matches!(node.get(Prop::Tokens), Some(PropValue::Tokens(t)) if t.reads_time())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u32) -> NodeId {
        NodeId::new(i, 0)
    }

    #[test]
    fn t_counts_from_the_first_painted_frame() {
        let mut times = NodeTimes::default();
        let s = Duration::from_secs;
        // A preview does not start the clock.
        assert_eq!(times.context(id(1), s(5), false, false).t, 0.0);
        assert_eq!(times.len(), 0);
        assert_eq!(times.context(id(1), s(10), true, false).t, 0.0);
        assert_eq!(times.context(id(1), s(12), true, false).t, 2.0);
        assert_eq!(times.context(id(1), s(13), false, false).t, 3.0);
        // Another node starts its own clock.
        assert_eq!(times.context(id(2), s(13), true, false).t, 0.0);
        // Frozen: `t = 0`, the clock kept.
        assert_eq!(
            times.context(id(1), s(20), true, true),
            TimeContext::frozen(0, 0)
        );
        assert_eq!(times.start(id(1)), Some(s(10)));
        // A remount (the id gone) starts again.
        times.retain(|n| n != id(1));
        assert_eq!(times.context(id(1), s(30), true, false).t, 0.0);
    }
}

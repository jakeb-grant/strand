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

use strand_scene::{NodeId, Prop, PropValue, TimeContext, TokenTable};

use crate::tree::Node;

/// When each node that reads time first drew, on the presentation clock.
#[derive(Debug, Default)]
pub(crate) struct NodeTimes {
    start: HashMap<NodeId, Duration>,
}

impl NodeTimes {
    /// The time context of `id` in a frame at `frame`, and when its clock
    /// next ticks (`None`: every frame, or frozen). A painted frame
    /// (`commit`) starts the node's clock if it has none; a preview reads
    /// an unstarted clock as 0 without starting it. `frozen` (reduced
    /// motion, or a frame with no clock) stops it at `t = 0`. A capped
    /// clock (`period`) reads `t` in whole ticks, a tick counting as
    /// reached `slack` early (half a frame: the frame nearest it draws
    /// it).
    pub(crate) fn context(
        &mut self,
        id: NodeId,
        frame: Duration,
        commit: bool,
        frozen: bool,
        period: Option<Duration>,
        slack: Duration,
    ) -> (TimeContext, Option<Duration>) {
        let (index, count) = letter(id);
        if frozen {
            return (TimeContext::frozen(index, count), None);
        }
        let start = if commit {
            *self.start.entry(id).or_insert(frame)
        } else {
            self.start.get(&id).copied().unwrap_or(frame)
        };
        let elapsed = frame.saturating_sub(start);
        let (t, next) = match period.filter(|p| !p.is_zero()) {
            None => (elapsed, None),
            Some(p) => {
                let ticks = ((elapsed + slack).as_nanos() / p.as_nanos()) as u32;
                (p * ticks, Some(start + p * (ticks + 1)))
            }
        };
        let cx = TimeContext {
            t: t.as_secs_f32(),
            index,
            count,
        };
        (cx, next)
    }

    /// Starts the clock of `id` at `frame` if it has none (its first
    /// painted frame that drew it).
    pub(crate) fn begin(&mut self, id: NodeId, frame: Duration) {
        self.start.entry(id).or_insert(frame);
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
/// [`overrides_read_time`]), directly or through a token of the global
/// table that does (`glow: $pulse` with `$pulse: 8 * wave(2s)`).
pub(crate) fn reads_time(node: &Node, global: &TokenTable) -> bool {
    let timed = |p: &str| global.time_reads(p);
    node.props
        .iter()
        .any(|e| e.prop != Prop::Tokens && e.value.reads_time_with(&timed))
}

/// True if `node`'s `tokens` override reads time (`set { $spin: t *
/// 20deg }`, or `set { $glow: $pulse }` of a global `$pulse` that does):
/// every node in its scope is evaluated at its own time.
pub(crate) fn overrides_read_time(node: &Node, global: &TokenTable) -> bool {
    let timed = |p: &str| global.time_reads(p);
    matches!(node.get(Prop::Tokens), Some(PropValue::Tokens(t)) if t.reads_time_with(&timed))
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
        let at = |times: &mut NodeTimes, n: u32, f: Duration, commit: bool, frozen: bool| {
            times.context(id(n), f, commit, frozen, None, Duration::ZERO)
        };
        // A preview does not start the clock.
        assert_eq!(at(&mut times, 1, s(5), false, false).0.t, 0.0);
        assert_eq!(times.len(), 0);
        assert_eq!(at(&mut times, 1, s(10), true, false).0.t, 0.0);
        assert_eq!(
            at(&mut times, 1, s(12), true, false),
            (
                TimeContext {
                    t: 2.0,
                    index: 0,
                    count: 0
                },
                None
            )
        );
        assert_eq!(at(&mut times, 1, s(13), false, false).0.t, 3.0);
        // Another node starts its own clock.
        assert_eq!(at(&mut times, 2, s(13), true, false).0.t, 0.0);
        // Frozen: `t = 0`, the clock kept.
        assert_eq!(
            at(&mut times, 1, s(20), true, true),
            (TimeContext::frozen(0, 0), None)
        );
        assert_eq!(times.start(id(1)), Some(s(10)));
        // A remount (the id gone) starts again.
        times.retain(|n| n != id(1));
        assert_eq!(at(&mut times, 1, s(30), true, false).0.t, 0.0);
    }

    #[test]
    fn a_capped_clock_reads_whole_ticks() {
        let mut times = NodeTimes::default();
        let ms = Duration::from_millis;
        let p = Some(ms(100));
        let (cx, next) = times.context(id(1), ms(1000), true, false, p, ms(8));
        assert_eq!((cx.t, next), (0.0, Some(ms(1100))));
        let (cx, next) = times.context(id(1), ms(1090), true, false, p, ms(8));
        assert_eq!((cx.t, next), (0.0, Some(ms(1100))), "too early");
        let (cx, next) = times.context(id(1), ms(1093), true, false, p, ms(8));
        assert_eq!(
            (cx.t, next),
            (0.1, Some(ms(1200))),
            "the frame nearest the tick"
        );
        let (cx, _) = times.context(id(1), ms(1250), true, false, p, ms(8));
        assert_eq!(cx.t, 0.2);
    }
}

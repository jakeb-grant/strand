//! (M4) `stagger: 30ms` on a container (design.md, "Motion and time";
//! the schema: "Delays each child's `enter` this much after the one
//! before").
//!
//! Children that start their `enter` together (created in the same diff,
//! or shown with their surface) are numbered in child order when the
//! first of them is drawn. Child `i` holds at its enter pose for `i ·
//! stagger`, then plays it as usual. A later batch starts its own count.
//! A step is capped at [`MAX_STEP`]. `reduced_motion` (and frames with no
//! clock) show every child at once, as they do every pose.

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::NodeId;

/// The longest step between two children.
pub(crate) const MAX_STEP: Duration = Duration::from_secs(1);

/// When each held child starts its enter.
#[derive(Debug, Default)]
pub(crate) struct Staggers {
    starts: HashMap<NodeId, Duration>,
}

impl Staggers {
    /// True if `id` already has its start.
    pub(crate) fn planned(&self, id: NodeId) -> bool {
        self.starts.contains_key(&id)
    }

    /// Numbers `children` (entering together, in order) from `now`, one
    /// `step` apart; children already numbered keep their start.
    pub(crate) fn plan(
        &mut self,
        children: impl Iterator<Item = NodeId>,
        step: Duration,
        now: Duration,
    ) {
        let step = step.min(MAX_STEP);
        for (i, c) in children.enumerate() {
            let at = now.saturating_add(step.saturating_mul(i as u32));
            self.starts.entry(c).or_insert(at);
        }
    }

    /// True while `id` waits for its turn at `now`; its start is dropped
    /// once reached.
    pub(crate) fn held(&mut self, id: NodeId, now: Duration) -> bool {
        match self.starts.get(&id) {
            Some(at) if now < *at => true,
            Some(_) => {
                self.starts.remove(&id);
                false
            }
            None => false,
        }
    }

    /// Anything `under` a surface waiting for its turn.
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.starts.keys().any(|id| under(*id))
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.starts.retain(|id, _| keep(*id));
    }

    pub(crate) fn forget(&mut self, id: NodeId) {
        self.starts.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_start_one_step_apart_and_a_step_is_capped() {
        let ms = Duration::from_millis;
        let ids: Vec<NodeId> = (1..=3).map(|i| NodeId::new(i, 0)).collect();
        let mut s = Staggers::default();
        s.plan(ids.iter().copied(), ms(30), ms(1000));
        assert!(!s.held(ids[0], ms(1000)));
        assert!(s.held(ids[1], ms(1029)));
        assert!(!s.held(ids[1], ms(1030)));
        assert!(s.held(ids[2], ms(1059)));
        // Planned again: the start stays.
        s.plan(ids.iter().copied(), ms(30), ms(1050));
        assert!(!s.held(ids[2], ms(1060)));
        let mut s = Staggers::default();
        s.plan(ids.iter().copied(), Duration::from_secs(5), ms(0));
        assert!(!s.held(ids[2], ms(2000)));
    }
}

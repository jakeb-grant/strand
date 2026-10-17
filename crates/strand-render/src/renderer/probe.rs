//! (eval-sandbox) The renderer's side of `crate::probe`: the frame
//! trace's switch and frames, and the inline text hold.

use strand_text::TextLayout;

use super::Renderer;
use crate::probe::{FrameTrace, HeldText};

impl Renderer {
    /// (eval-sandbox) Records a [`FrameTrace`] for every frame painted
    /// from now on (on) or none (off, which drops those not taken).
    pub fn set_frame_trace(&mut self, on: bool) {
        self.probe.trace_on = on;
        self.extras.trace = on;
        if !on {
            self.probe.frames.clear();
        }
        // A scene flattened before has no nodes to report.
        for s in self.surfaces.values_mut() {
            s.cache = None;
        }
    }

    /// (eval-sandbox) The frames painted since the last call, in paint
    /// order.
    pub fn take_frame_traces(&mut self) -> Vec<FrameTrace> {
        std::mem::take(&mut self.probe.frames)
    }

    /// (eval-sandbox) Holds back every layout inline shaping makes from
    /// now on (on), its request left pending as a late worker's is; off
    /// delivers what is held.
    pub fn set_text_hold(&mut self, on: bool) {
        match on {
            true => self.set_text_hold_with(Some(Box::new(|_, _| true))),
            false => self.set_text_hold_with(None),
        }
    }

    /// (eval-sandbox) Holds back the layouts inline shaping makes from
    /// now on of the texts `pick` chooses (by node and text); the others
    /// are delivered at once, as without a hold. `None` holds nothing and
    /// delivers what is held. What is already held stays held under a
    /// new pick.
    pub fn set_text_hold_with(&mut self, pick: Option<crate::probe::HoldPick>) {
        let off = pick.is_none();
        self.probe.hold = pick;
        if off {
            self.release_text(&mut |_| true);
        }
    }

    /// (eval-sandbox) The layouts the hold keeps back, oldest first.
    pub fn held_text(&self) -> Vec<HeldText> {
        self.probe.held.iter().map(|(h, _)| h.clone()).collect()
    }

    /// (eval-sandbox) Delivers the held layouts `pick` chooses, as the
    /// text worker's arrive (a stale one, whose request a newer one
    /// replaced, is dropped by delivery); returns how many.
    pub fn release_text(&mut self, pick: &mut dyn FnMut(&HeldText) -> bool) -> usize {
        let held = std::mem::take(&mut self.probe.held);
        let (go, keep): (Vec<_>, Vec<_>) = held.into_iter().partition(|(h, _)| pick(h));
        self.probe.held = keep;
        let n = go.len();
        for (_, layout) in go {
            self.deliver(layout);
        }
        n
    }

    /// Inline shaping made `layout` for `node`'s `text`: the layout to
    /// deliver now, or `None` when the hold keeps it back.
    pub(super) fn probe_hold(
        &mut self,
        node: strand_scene::NodeId,
        text: &str,
        layout: TextLayout,
    ) -> Option<TextLayout> {
        if !self
            .probe
            .hold
            .as_mut()
            .is_some_and(|pick| pick(node, text))
        {
            return Some(layout);
        }
        let held = HeldText {
            node,
            key: layout.key.0,
            text: text.to_owned(),
            paints: 0,
        };
        self.probe.held.push((held, layout));
        None
    }
}

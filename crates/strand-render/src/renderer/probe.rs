//! (eval-sandbox) The renderer's side of `crate::probe`: the frame
//! trace's switch and frames, and the inline text hold.

use strand_text::TextLayout;

use strand_scene::{Damage, PaintTarget, SurfaceId};

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
            self.extras.texts.clear();
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

    /// (eval-sandbox) Reads `now` in place of the wall clock in the
    /// renderer's own timers (`None`: the wall clock again): a tooltip's
    /// delay and a capped clock's ticks (`effect shimmer`, a raster
    /// rate) run on the evaluation sandbox's virtual time, and
    /// [`Renderer::next_wake`] answers on it (docs/decisions.md
    /// eval-sandbox).
    pub fn set_now(&mut self, now: Option<strand_scene::time::Instant>) {
        self.probe.now = now;
    }

    /// (eval-sandbox) How many layouts inline shaping delivered at once
    /// since the last call (held ones not counted). A text worker wakes
    /// the render loop for every layout it delivers (the host then runs
    /// [`Renderer::update`]); with inline shaping nothing wakes it, so
    /// the sandbox wakes its loop for these.
    pub fn take_inline_shaped(&mut self) -> u32 {
        std::mem::take(&mut self.probe.shaped)
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

    /// (eval-sandbox) Keeps each surface's last painted frame as drawn
    /// from now on (on), for [`Renderer::repaint_fresh`], or nothing
    /// (off, which drops those kept).
    pub fn set_keep_frames(&mut self, on: bool) {
        self.probe.keep = on;
        if !on {
            self.probe.last.clear();
        }
    }

    /// (eval-sandbox) `surface`'s last painted frame drawn again in
    /// full into `target`, as into a fresh buffer (age 0): the display
    /// list that frame drew, flattened at its time, rasterised by a
    /// raster with nothing cached ([`crate::raster::Raster`]'s `fresh`),
    /// and blended over the crossfade snapshot at the frame's weight
    /// when it was. What a partial redraw left in the presented buffer
    /// must equal it (the `redraw` check). It reads the renderer and
    /// changes nothing a later frame reads (`&self`).
    ///
    /// False, with `target` untouched, when no frame is kept for the
    /// surface ([`Renderer::set_keep_frames`] off, or none painted since)
    /// or `target` is not the frame's size and scale.
    pub fn repaint_fresh(&self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> bool {
        let Some(last) = self.probe.last.get(&surface) else {
            return false;
        };
        if target.validate().is_err() || target.size != last.size || target.scale != last.scale {
            return false;
        }
        let mut raster = self.raster.fresh();
        raster.paint(
            &last.items,
            &Damage::full(target.size),
            &self.atlas,
            last.scale,
            target,
        );
        if let Some(w) = last.fade {
            self.blend_fade_into(surface, w, target);
        }
        true
    }

    /// (eval-rules-4) [`Renderer::repaint_fresh`] of `surface`'s last
    /// frame when it was blended over a crossfade snapshot standing
    /// alone, without that blend: the new frame into `target`, the
    /// snapshot's pixels and the weight it was blended at, so a check
    /// blends them again by its own arithmetic. `None`, with `target`
    /// untouched or not, when the frame was not blended or no frame is
    /// kept.
    pub fn repaint_unblended(
        &self,
        surface: SurfaceId,
        target: &mut PaintTarget<'_>,
    ) -> Option<(&[u8], f32)> {
        let last = self.probe.last.get(&surface)?;
        let w = last.fade?;
        if target.validate().is_err() || target.size != last.size || target.scale != last.scale {
            return None;
        }
        let old = self.fade_snapshot(surface, target.size)?;
        let mut raster = self.raster.fresh();
        raster.paint(
            &last.items,
            &Damage::full(target.size),
            &self.atlas,
            last.scale,
            target,
        );
        Some((old, w))
    }

    /// (eval-sandbox, the `redraw` check's own tests only) A mutant:
    /// from now on each frame's own damage, where it is worked out from
    /// what changed, is shrunk by `px` pixels on every side, so a
    /// partial redraw leaves a band of stale pixels around each change.
    /// 0 turns it off. Nothing but eval/tests sets it.
    pub fn set_damage_inset(&mut self, px: u32) {
        self.probe.damage_inset = px;
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
            self.probe.shaped = self.probe.shaped.saturating_add(1);
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

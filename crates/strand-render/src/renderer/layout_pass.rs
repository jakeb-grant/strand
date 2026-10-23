//! The layout step of a frame: when a surface lays out again, size
//! springs and FLIP glides, layout facts for logic, and flattening.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::Curve;
use strand_scene::{LogicalRect, NodeId, NodeKind, Prop, Scale, SurfaceId, TokenScope};

use super::Renderer;
use super::specs::{centred_overhang, within_edges};
use super::text::TextSlot;
use crate::anim::{Animator, SizeMap, StepFills};
use crate::flatten::{Flattened, Shaped, flatten, pick};
use crate::layout::{Boxes, RootSize, TextSizes, layout};
use crate::tree::SceneTree;

/// (growth) The least step of a content-sized surface's content size, in
/// logical px, its box springs over: less is a rounding, taken at once.
pub(super) const STEP_MIN: f32 = 0.5;

/// Delivered text layouts as layout measures them.
pub(super) struct TextInfo<'a> {
    pub(super) shaped: &'a HashMap<NodeId, Vec<Shaped>>,
    pub(super) scale: Scale,
    /// Sizing a surface: text not shaped yet is estimated
    /// (`TextSizes::estimates`).
    pub(super) estimate: bool,
}

impl TextSizes for TextInfo<'_> {
    fn natural(&self, node: NodeId) -> Option<strand_scene::LogicalSize> {
        pick(self.shaped.get(&node)?, self.scale, None).map(|l| l.size)
    }

    fn fitted(&self, node: NodeId, width: f32) -> Option<strand_scene::LogicalSize> {
        pick(self.shaped.get(&node)?, self.scale, Some(width)).map(|l| l.size)
    }

    fn part(&self, node: NodeId, part: u8) -> Option<strand_scene::LogicalSize> {
        crate::flatten::pick_part(self.shaped.get(&node)?, part, self.scale, None).map(|l| l.size)
    }

    fn estimates(&self) -> bool {
        self.estimate
    }
}

impl Renderer {
    /// Layout passes run so far (paint-only changes run none).
    pub fn layout_passes(&self) -> u64 {
        self.layout_passes
    }

    /// The last layout of `surface`: every laid-out node's box in
    /// surface logical pixels (before paint offsets).
    pub fn boxes(&self, surface: SurfaceId) -> Option<&Boxes> {
        self.surfaces.get(&surface)?.boxes.as_ref()
    }

    /// Laid-out boxes that changed since the last call, `(node, box)` in
    /// logical pixels, of the nodes logic reads them of (`Prop::Watch`):
    /// what `self.width`, container queries and (M6) `id.box` read. A
    /// box is in its surface's coordinates (`facts.rs`); a node read
    /// only for its size reports a move only with a size change. A node
    /// shown on several surfaces reports the box of the last one laid
    /// out. A non-empty batch gets the next [`Renderer::layout_seq`].
    pub fn take_layout_facts(&mut self) -> Vec<(NodeId, strand_scene::LogicalRect)> {
        if std::mem::take(&mut self.placed_dirty) {
            self.report_placed();
        }
        let facts = std::mem::take(&mut self.facts);
        if !facts.is_empty() {
            self.facts_seq += 1;
        }
        facts
    }

    /// The sequence number of the last fact batch taken: logic echoes it
    /// as [`SceneDiff::layout_seen`].
    pub fn layout_seq(&self) -> u64 {
        self.facts_seq
    }

    /// How long a frame whose layout changed a size a container query
    /// reads waits for logic's answer (see [`QUERY_WAIT`]); zero, the
    /// default, never waits.
    pub fn set_query_wait(&mut self, wait: Duration) {
        self.query_wait = wait;
    }

    /// Flattens a surface, issuing text requests for changed text. With the
    /// inline backend new layouts are available at once, so it flattens
    /// again until text is stable.
    pub(super) fn flatten_surface(&mut self, id: SurfaceId) -> Flattened {
        let mut stable = None;
        // One wait a frame, however many passes it takes: text mounted by
        // a pass (uncovered as a card grows) waits only for what is left
        // of it, so a slow worker costs a frame the wait once, not once
        // a pass. The frame's `update` and `paint` (which flattens again
        // while the surface moves) share it until the frame is painted.
        // (A first frame is held for all of its text anyway, see
        // `frame_deadline`.)
        let mut deadline = self.surfaces.get(&id).and_then(|s| s.text_deadline);
        for _ in 0..6 {
            let flat = self.flatten_now(id);
            // New text the worker has not shaped yet was laid out by an
            // estimate: wait briefly for it and lay out again.
            let text =
                self.request_text(&flat.text) | self.await_unmeasured(&flat.text, &mut deadline);
            // Decoded inline (offline): draw them at once too.
            // While a size springs here, images at a new size draw their
            // last decode scaled and are decoded once it rests.
            let defer = self
                .surfaces
                .get(&id)
                .is_some_and(|s| self.anim.sizes_moving(&self.tree, s.root));
            let images = self.extras.images.want(id, &flat.images, defer);
            if !text && !images {
                stable = Some(flat);
                break;
            }
        }
        let f = match stable {
            Some(f) => f,
            None => self.flatten_now(id),
        };
        // Once every text here has a layout at this scale, the previous
        // scale's layouts are no longer needed as stand-ins.
        let texts = &self.texts;
        let settled = f.text.iter().all(|(node, spec)| {
            texts
                .get(&TextSlot::of(*node, spec))
                .is_some_and(|t| t.requested.is_none())
        });
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.text_deadline = deadline;
            s.wanted = f
                .text
                .iter()
                .map(|(node, spec)| TextSlot::of(*node, spec))
                .collect();
            s.flip_settled = settled;
            if settled && s.flip_drawn {
                // What a structural change mounted is measured and was
                // drawn: later passes move nothing it placed by an
                // estimate or in a buffer it outgrew.
                s.flip_open = None;
            }
            if settled && s.prev_scale.take().is_some() {
                self.prune_scales();
            }
        }
        if self.prune_texts().contains(&id) {
            // `f` drew a stand-in that is gone now.
            return self.flatten_now(id);
        }
        f
    }

    /// Lays `id` out if anything layout reads changed (at most one extra
    /// pass, when a list measured rows it had only estimated), then
    /// flattens it.
    pub(super) fn flatten_now(&mut self, id: SurfaceId) -> Flattened {
        // A surface a theme swap crossfades while the roots spring
        // elsewhere is drawn from the new table.
        let held = self.hold_tokens(id);
        let f = self.flatten_tokens(id);
        self.release_tokens(held);
        f
    }

    pub(super) fn flatten_tokens(&mut self, id: SurfaceId) -> Flattened {
        let layouts = self.shaped();
        self.lay_out(id, &layouts);
        // (M4) A GPU-presented surface paints its poses (`Extras`).
        #[cfg(feature = "gpu")]
        if let Some(root) = self.surfaces.get(&id).map(|s| s.root) {
            if self.backend(id) == strand_scene::Backend::GpuPresent {
                self.extras.gpu_presented.insert(root);
            } else {
                self.extras.gpu_presented.remove(&root);
            }
        }
        // (M4) Whether bundled effects take their GPU form (3-D tilt, an
        // animated aurora): not while the device is unavailable.
        #[cfg(feature = "gpu")]
        {
            self.extras.gpu_ok = !matches!(
                self.gpu_status(),
                strand_scene::GpuStatus::Unavailable { .. }
            );
        }
        let Some(s) = self.surfaces.get(&id) else {
            return Flattened::default();
        };
        let empty = Boxes::default();
        let boxes = s.boxes.as_ref().unwrap_or(&empty);
        flatten(
            &self.tree,
            s.root,
            s.size,
            s.scale,
            &layouts,
            boxes,
            &mut self.anim,
            &self.extras,
        )
    }

    /// One layout pass of `root` (two when a list measured rows it had
    /// estimated), with `sizes` forced.
    pub(super) fn run_layout(
        &mut self,
        root: NodeId,
        size: RootSize,
        info: &TextInfo<'_>,
        sizes: &SizeMap,
    ) -> Boxes {
        let mut boxes = layout(&self.tree, root, size, info, &mut self.scrolls, sizes);
        self.layout_passes += 1;
        self.laid_out_nodes += boxes.rects.len();
        if boxes.unsettled {
            boxes = layout(&self.tree, root, size, info, &mut self.scrolls, sizes);
            self.layout_passes += 1;
            self.laid_out_nodes += boxes.rects.len();
        }
        boxes
    }

    /// The curve a FLIP glide on surface root `root` plays: the default
    /// transition's, or (growth) while its box springs over a step, the
    /// step's (its height's where that steps, else its width's), so what
    /// the step moves in its box glides with it.
    fn flip_curve(&self, root: NodeId) -> Curve {
        let axes = self.anim.step_axes(root);
        if let Some(axis) = [1, 0].into_iter().find(|a| axes[*a]) {
            return self.step_curve(root, axis);
        }
        let tables = [&self.tree.tokens];
        Curve::of(&TokenScope::new(&tables).transition(&strand_scene::Transition::Default, Prop::X))
    }

    /// (popup-shrink) How the surface node `root` is laid out in `frame`,
    /// its buffer's box. A content-sized panel, OSD or popup held at its
    /// larger size while something on it moves (it shrinks once nothing
    /// does, [`Renderer::release_holds`]) is laid out at its content's
    /// size and placed where the compositor will keep the smaller box
    /// ([`super::specs::resize_share`]): its content is drawn where it
    /// will be on screen, so nothing moves when the buffer shrinks; one
    /// that grows is placed so too while its box springs
    /// ([`Renderer::lay_out_stepped`], growth). Any other root fills its
    /// frame: a bar's or a lock's size is the output's, and a surface
    /// configured at another size than it asked for is drawn at the size
    /// it has.
    fn root_size(&self, root: NodeId, frame: LogicalRect) -> RootSize {
        let fixed = RootSize::Fixed(frame);
        let (Some(&(cw, ch)), Some(spec)) = (self.content_sized.get(&root), self.specs.get(&root))
        else {
            return fixed;
        };
        let asks = matches!(spec.kind, NodeKind::Panel | NodeKind::Osd | NodeKind::Popup);
        if !asks || !self.held.contains(&root) {
            return fixed;
        }
        let Some(at) = self.resize_share_of(root) else {
            return fixed;
        };
        RootSize::Placed {
            frame,
            width: (!cw).then_some(frame.w),
            height: (!ch).then_some(frame.h),
            at,
            exact: false,
        }
    }

    /// (bottom-osd-shrink, growth) How a content-sized surface's buffer
    /// keeps its place when it resizes ([`super::specs::resize_share`]);
    /// `None` for any other surface.
    fn shrink_share(&self, root: NodeId) -> Option<(f32, f32)> {
        let spec = self.specs.get(&root)?;
        if spec.kind == NodeKind::Bar || !self.content_sized.contains_key(&root) {
            return None;
        }
        self.resize_share_of(root)
    }

    /// (growth) A content-sized panel, OSD or popup: the axes it sizes
    /// by its content, and where the compositor keeps its box as it
    /// resizes ([`super::specs::resize_share`]). `None` for any other
    /// surface (a bar's length is the output's).
    fn stepped_root(&self, root: NodeId) -> Option<((bool, bool), (f32, f32))> {
        let spec = self.specs.get(&root)?;
        if !matches!(spec.kind, NodeKind::Panel | NodeKind::Osd | NodeKind::Popup) {
            return None;
        }
        let axes = *self.content_sized.get(&root)?;
        Some((axes, self.resize_share_of(root)?))
    }

    /// (growth) The curve a step of `root`'s box springs along on `axis`
    /// (0 the width, 1 the height): its `width`'s (`height`'s) transition,
    /// else `size`'s, else the default, `$motion.spatial` (design.md
    /// "Springs everywhere"; `width: auto ~ instant` snaps it).
    pub(super) fn step_curve(&self, root: NodeId, axis: usize) -> Curve {
        let Some(node) = self.tree.get(root) else {
            return Curve::Instant;
        };
        let prop = if axis == 0 { Prop::Width } else { Prop::Height };
        let transition = node
            .props
            .iter()
            .find(|e| e.prop == prop)
            .or_else(|| node.props.iter().find(|e| e.prop == Prop::Size))
            .map_or(strand_scene::Transition::Default, |e| e.transition.clone());
        let scopes = crate::flatten::scope_tables(&self.tree, root);
        Curve::of(&TokenScope::new(&scopes).transition(&transition, prop))
    }

    /// (growth) The last pass of a content-sized panel, OSD or popup
    /// (`step`: [`Renderer::stepped_root`]), with `sizes` forced. Its box
    /// takes its content's size, the sizes in flight included, and where
    /// that size steps (a text grew, a row mounted or left with no pose
    /// that springs its size) its box springs over the step, from where
    /// it was drawn, along its `width`'s or `height`'s curve
    /// ([`Renderer::step_curve`]). Laid out at the size in flight: the
    /// boxes that fill it follow it (a card filling a panel) and clip what
    /// they hold, which keeps its layout at rest ([`step_layout`]). The
    /// box is placed where the compositor keeps it as the buffer,
    /// configured at the larger of the two sizes, is resized
    /// (`RootSize::Placed`), so nothing on screen jumps. The same
    /// placement holds while its content's own springs move it in a
    /// buffer of the size it asked for (a growth as a shrink,
    /// popup-shrink). At rest it is laid out as `fallback`, today's root
    /// size. `measure`: a pass where the content's size may have stepped
    /// (a change, a text, a resize).
    #[allow(clippy::too_many_arguments)]
    fn lay_out_stepped(
        &mut self,
        id: SurfaceId,
        root: NodeId,
        frame: LogicalRect,
        fallback: RootSize,
        info: &TextInfo<'_>,
        sizes: &SizeMap,
        old: Option<&Boxes>,
        measure: bool,
        step: ((bool, bool), (f32, f32)),
    ) -> Boxes {
        let ((cw, ch), at) = step;
        let placed = RootSize::Placed {
            frame,
            width: (!cw).then_some(frame.w),
            height: (!ch).then_some(frame.h),
            at,
            exact: false,
        };
        let kids = sizes
            .iter()
            .any(|(n, f)| *n != root && f.size.iter().any(Option::is_some));
        let held = self.held.contains(&root);
        let stepping = self.anim.stepping(root);
        let Some(s) = self.surfaces.get(&id) else {
            return self.run_layout(root, fallback, info, sizes);
        };
        // Open, and configured at the size it asked for (a growth's buffer
        // once its configure came, a held one's): one of another size (the
        // compositor's, an output too small) fills it, and never springs,
        // and one closing (what it held gone with its exit) takes no step.
        let spec = self.specs.get(&root);
        let asked = |v: Option<f32>, have: f32| v.is_none_or(|v| (v - have).abs() < 0.5);
        let asked =
            spec.is_some_and(|sp| sp.open && asked(sp.width, frame.w) && asked(sp.height, frame.h));
        // Measured once the size springs it holds have started: one that
        // waits (an `enter` pose's, in a preview with no clock) leaves the
        // step to the first pass it has started in.
        let want = (measure || s.step_due) && asked && s.painted && !self.anim.reduced();
        let ready = !self.anim.size_pending(&self.tree, root);
        let measure = want && ready;
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.step_due = want && !ready;
        }
        let Some(s) = self.surfaces.get(&id) else {
            return self.run_layout(root, fallback, info, sizes);
        };
        if !measure && !kids && !held && !stepping {
            self.anim.set_step_fills(root, None);
            return self.run_layout(root, fallback, info, sizes);
        }
        // Where the last frame drew its boxes (or the last pass laid them).
        let drawn = s
            .drawn_boxes
            .as_ref()
            .map(|(d, _)| d)
            .or_else(|| old.map(|o| &o.rects));
        let was = drawn.and_then(|d| d.get(&root)).map(|r| [r.w, r.h]);
        let drawn: HashMap<NodeId, [f32; 2]> = if stepping || measure {
            drawn
                .map(|d| d.iter().map(|(n, r)| (*n, [r.w, r.h])).collect())
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        // The content at its size in flight: laid out with the box free.
        let free = self.run_layout(root, placed, info, sizes);
        let Some(content) = free.rects.get(&root).map(|r| [r.w, r.h]) else {
            self.anim.set_step_fills(root, None);
            return free;
        };
        if measure && let Some(was) = was {
            // Against its content as the last frame drew what springs in
            // it: what the springs move since is no step.
            let then = self.anim.sizes_drawn(&self.tree, root, sizes);
            let then = if then == *sizes {
                content
            } else {
                let b = self.run_layout(root, placed, info, &then);
                b.rects.get(&root).map_or(content, |r| [r.w, r.h])
            };
            let now = self.anim.step_was(root);
            for (axis, sized) in [cw, ch].into_iter().enumerate() {
                let delta = was[axis] - (then[axis] + now[axis]);
                if sized && delta.abs() > STEP_MIN {
                    let curve = self.step_curve(root, axis);
                    self.anim.step_size(root, axis, delta, curve);
                    if let Some(s) = self.surfaces.get_mut(&id) {
                        s.step_started = true;
                    }
                }
            }
        }
        let step = self.anim.sample_step(root);
        if step.iter().any(Option::is_some) {
            let (forced, chains) =
                step_layout(&self.tree, root, (&free, &drawn), sizes, content, step);
            // What a far-anchored box holds keeps to its far edge.
            let far = [0, 1].map(|a| step[a].is_some() && [at.0, at.1][a] >= 1.0);
            let fills = StepFills { chains, far };
            let laid = self.run_layout(root, exact_of(placed), info, &forced);
            // A step started here: its FLIP compares what the change
            // moved with the box where the last frame drew it.
            if self.surfaces.get(&id).is_some_and(|s| s.step_started) {
                let at = self.anim.step_was(root);
                let then = [0, 1].map(|a| step[a].map(|_| at[a]));
                let (forced, _) =
                    step_layout(&self.tree, root, (&free, &drawn), sizes, content, then);
                let then = self.run_layout(root, exact_of(placed), info, &forced);
                let then = keep_far(&self.tree, root, &free, then, &fills);
                if let Some(s) = self.surfaces.get_mut(&id) {
                    s.step_flip = Some(then.rects);
                }
            }
            let boxes = keep_far(&self.tree, root, &free, laid, &fills);
            self.anim.set_step_fills(root, Some(fills));
            return boxes;
        }
        self.anim.set_step_fills(root, None);
        // Its content's own springs move it smaller than the buffer of the
        // size it asked for: it follows them, placed where it stays; past
        // it (a spring's overshoot), it follows them too, at its exact
        // share, cut by the buffer.
        let off = |sized: bool, c: f32, have: f32| sized && have - c.ceil() >= 0.5;
        let off = off(cw, content[0], frame.w) || off(ch, content[1], frame.h);
        if held || (kids && asked && off) {
            return free;
        }
        // The free pass lays a box larger than its buffer at the buffer's
        // size: one that fills it may be past it.
        let full = |sized: bool, c: f32, have: f32| sized && (c - have).abs() < 1e-3;
        if kids && asked && (full(cw, content[0], frame.w) || full(ch, content[1], frame.h)) {
            let past = self.run_layout(root, exact_of(placed), info, sizes);
            let over = |sized: bool, c: f32, have: f32| sized && c - have > 1e-3;
            if let Some(r) = past.rects.get(&root)
                && (over(cw, r.w, frame.w) || over(ch, r.h, frame.h))
            {
                return past;
            }
        }
        self.run_layout(root, fallback, info, sizes)
    }

    /// Lays out surface `id` if anything layout reads changed, or its
    /// size springs moved. Sizes spring from the boxes of the last pass
    /// to the ones laid out at rest; a frame where only size springs
    /// moved lays out just the subtrees under their nearest size-stable
    /// ancestors. A structural change glides the boxes it moved from
    /// where they were (FLIP).
    pub(super) fn lay_out(&mut self, id: SurfaceId, layouts: &HashMap<NodeId, Vec<Shaped>>) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let full = s.layout_dirty || s.boxes.is_none();
        let step_due = s.step_due;
        if !full && !s.anim_layout {
            return;
        }
        self.laid_out_nodes = 0;
        let root = s.root;
        let logical = s.scale.logical_size(s.size);
        let o = s.overhang;
        let frame = strand_scene::LogicalRect::new(
            o.left,
            o.top,
            (logical.w - o.left - o.right).max(0.0),
            (logical.h - o.top - o.bottom).max(0.0),
        );
        let info = TextInfo {
            shaped: layouts,
            scale: s.scale,
            estimate: false,
        };
        let root_size = self.root_size(root, frame);
        // Replaced below on every path: taken, not copied.
        let old = self.surfaces.get_mut(&id).and_then(|s| s.boxes.take());
        if let Some(old) = &old {
            self.anim.hold_exits(&self.tree, root, &old.rects);
        }
        let rest = self.anim.rest_sizes(&self.tree, root);
        let step = self.stepped_root(root);
        let sized_only = match &old {
            // (growth) A box springing over a step, or one whose step is
            // to be measured, is laid out whole.
            Some(old) if !full && !step_due && !self.anim.stepping(root) => {
                self.relayout_sized(root, old, &rest, &info)
            }
            _ => None,
        };
        if let Some(boxes) = sized_only {
            // (launcher-flip-pass) After a pass that glided what the
            // change moved, before a frame drew it: what that pass glided
            // and now lies where it was drawn did not move, and is drawn
            // there. What else this pass moves, the sizes in flight move
            // (a folding slot's siblings, a reordered row beside a box
            // that grows).
            let curve = self.flip_curve(root);
            let share = self.shrink_share(root);
            let reduced = self.anim.reduced();
            let moving = self.anim.sizes_moving(&self.tree, root);
            if let Some(s) = self.surfaces.get_mut(&id) {
                if let Some(old) = &old
                    && let Some(shifts) = s.flip_shifts.as_mut()
                    && let Some((flip, all)) = &s.flip_open
                    && !reduced
                {
                    let drawn = match &s.drawn_boxes {
                        Some((d, size)) => moved_by(d, buffer_shift(*size, logical, share)),
                        None => moved_by(&old.rects, (0.0, 0.0)),
                    };
                    glide_moved(
                        &self.tree,
                        &mut self.anim,
                        root,
                        Compare {
                            old: &drawn,
                            new: &boxes.rects,
                            glided_only: true,
                        },
                        flip,
                        *all,
                        &s.unpainted,
                        shifts,
                        curve,
                    );
                }
                s.boxes = Some(boxes);
                s.anim_layout = moving;
                keep_drawn(s, old, s.laid_size);
            }
            self.place_waiting_popups(root);
            self.report_facts(id);
            return;
        }
        let sized = self.anim.size_work(&self.tree, root);
        let mut sizes = rest.clone();
        // The shadows' reach where size springs that start here land.
        let mut rest_reach = None;
        // (growth) A content-sized panel's, OSD's or popup's last pass.
        let last = |r: &mut Self, sizes: &SizeMap, measure: bool| match step {
            Some(step) => r.lay_out_stepped(
                id,
                root,
                frame,
                root_size,
                &info,
                sizes,
                old.as_ref(),
                measure,
                step,
            ),
            None => r.run_layout(root, root_size, &info, sizes),
        };
        let mut boxes = if sized && !full && !self.anim.size_pending(&self.tree, root) {
            // Only springs moved: their targets are the ones learnt when
            // they started (nothing layout reads changed since), so one
            // pass with the in-flight sizes is enough.
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            last(self, &sizes, false)
        } else if sized {
            let at_rest = self.run_layout(root, root_size, &info, &rest);
            rest_reach = Some(crate::fillet::grow(
                &self.tree,
                &at_rest,
                root,
                at_rest.overhang,
            ));
            let old_rects = old.as_ref().map(|b| &b.rects);
            let late = |n| {
                crate::flatten::late_text_under(&self.tree, &at_rest.rects, layouts, info.scale, n)
            };
            self.anim
                .start_sizes(&self.tree, root, &at_rest.rects, old_rects, false, late);
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            if sizes == rest && step.is_none() {
                at_rest
            } else {
                last(self, &sizes, full)
            }
        } else {
            last(self, &rest, full)
        };
        if !self.content_sized.contains_key(&root)
            && let Some(mut spec) = self.specs.get(&root).cloned()
            && spec.open
        {
            let mut grown = crate::fillet::grow(&self.tree, &boxes, root, boxes.overhang);
            // (owner-testing.md bug 9) While a size springs here, the
            // reach is the larger of where it started and where it lands,
            // taken as it starts and kept until it settles: following the
            // spring would resize the buffer (and reconfigure or remake a
            // popup) every frame. What an overshoot carries past it is
            // clipped.
            if self.anim.sizes_moving(&self.tree, root) {
                let held = self.held_reach.entry(root).or_insert(grown);
                if let Some(r) = rest_reach {
                    *held = max_insets(*held, r);
                }
                grown = *held;
            } else {
                self.held_reach.remove(&root);
            }
            let grown = self.hold_shadow_reach(root, grown);
            let o = crate::fillet::flush(
                &self.tree,
                root,
                within_edges(&spec, centred_overhang(&spec, grown)),
            );
            if o != spec.overhang {
                spec.overhang = o;
                self.record_spec(root, spec);
                let frame = strand_scene::LogicalRect::new(
                    o.left,
                    o.top,
                    (logical.w - o.left - o.right).max(0.0),
                    (logical.h - o.top - o.bottom).max(0.0),
                );
                boxes = self.run_layout(root, RootSize::Fixed(frame), &info, &sizes);
            }
        }
        // FLIP: boxes a structural change moved start where they were.
        // So do boxes the shaped size of text it mounted moves again (a
        // pass after a text arrived, in this frame or a later one, while
        // the change's scope is open): their glides add up, and each
        // starts where the box was drawn.
        let share = self.shrink_share(root);
        let curve = self.flip_curve(root);
        if let Some(s) = self.surfaces.get_mut(&id) {
            let flip = std::mem::take(&mut s.flip);
            let all = std::mem::take(&mut s.flip_all);
            let text = std::mem::take(&mut s.text_arrived);
            // (growth) A step this pass started moves what its box holds
            // as a change does: compared where it was drawn.
            let stepped = std::mem::take(&mut s.step_started);
            let step_flip = s.step_flip.take();
            // (eval-coverage) Laid out in a resized buffer: a content-
            // sized surface configured at the size a change asked for.
            // Before its first frame nothing was drawn to glide from.
            let resized = s.painted && s.laid_frame.is_some_and(|f| f != frame);
            // (launcher-flip-pass) Boxes are compared where they were
            // drawn: the boxes the last frame drew, which the first pass
            // since it replaced (`drawn_boxes`), not a later pass's.
            let base = s.drawn_boxes.take();
            let chained = s.flip_shifts.is_some();
            // (bottom-osd-shrink) Where a shrink moved the buffer's near
            // edge on screen: boxes are compared where they were drawn
            // on screen, not in the buffer.
            let shift = if resized {
                buffer_shift(s.laid_size, logical, share)
            } else {
                (0.0, 0.0)
            };
            let drawn_shift = base.as_ref().map(|b| buffer_shift(b.1, logical, share));
            let laid_size = s.laid_size;
            s.laid_frame = Some(frame);
            s.laid_size = Some(logical);
            let fresh = all || !flip.is_empty();
            if fresh {
                let open = s.flip_open.get_or_insert_with(Default::default);
                open.0.extend(flip);
                open.1 |= all;
                s.flip_drawn = false;
            }
            // (launcher-flip-pass) A pass after a FLIP pass, before a
            // frame drew it, keeps what that pass glided where it was
            // drawn, whatever laid it out.
            let scope = s
                .flip_open
                .clone()
                .filter(|_| fresh || text || resized || stepped || chained);
            // (growth) A step that starts moves anything its box holds:
            // all of it is compared where it was drawn.
            let scope = match scope {
                Some((flip, _)) if stepped => Some((flip, true)),
                None if stepped => Some((HashSet::new(), true)),
                scope => scope,
            };
            if let (Some(old), Some(_)) = (&old, &s.flip_open) {
                s.unpainted.extend(
                    boxes
                        .rects
                        .keys()
                        .filter(|n| !old.rects.contains_key(n))
                        .copied(),
                );
            }
            if let Some(old) = &old
                && let Some((flip, all)) = scope
                && !self.anim.reduced()
            {
                let old_rects = match (&base, drawn_shift) {
                    (Some(b), Some(d)) => moved_by(&b.0, d),
                    _ => moved_by(&old.rects, shift),
                };
                let shifts = s.flip_shifts.get_or_insert_with(HashMap::new);
                glide_moved(
                    &self.tree,
                    &mut self.anim,
                    root,
                    Compare {
                        old: &old_rects,
                        new: step_flip.as_ref().unwrap_or(&boxes.rects),
                        glided_only: !(fresh || text || resized || stepped),
                    },
                    &flip,
                    all,
                    &s.unpainted,
                    shifts,
                    curve,
                );
            }
            s.drawn_boxes = base;
            keep_drawn(s, old, laid_size);
        }
        // Springs a clockless preview left to start lay out again on the
        // first frame painted.
        let moving =
            self.anim.sizes_moving(&self.tree, root) || self.anim.size_pending(&self.tree, root);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.boxes = Some(boxes);
            s.layout_dirty = false;
            // (growth) A step left for the next painted frame lays it out.
            s.anim_layout = moving || s.step_due;
        }
        self.place_waiting_popups(root);
        self.report_facts(id);
    }

    /// Lays out only what size springs move: the subtree under each
    /// moving node's nearest size-stable ancestor (a node of fixed width
    /// and height, or a surface root of fixed size), into a copy of
    /// `old`, once, with the in-flight sizes: their targets were learnt
    /// when they started and nothing layout reads changed since. `None`
    /// when that would be the whole surface, or a spring waits to start
    /// (a full pass learns its target).
    pub(super) fn relayout_sized(
        &mut self,
        root: NodeId,
        old: &Boxes,
        rest: &SizeMap,
        info: &TextInfo<'_>,
    ) -> Option<Boxes> {
        if self.anim.size_pending(&self.tree, root) {
            return None;
        }
        let moving = self.anim.sized_nodes(&self.tree, root);
        let mut subs: Vec<NodeId> = Vec::new();
        for n in moving {
            let mut a = self.tree.get(n)?.parent?;
            loop {
                if a == root {
                    if self.content_sized.contains_key(&root) {
                        return None;
                    }
                    break;
                }
                if !rest.contains_key(&a) && crate::layout::size_stable(&self.tree, a) {
                    break;
                }
                a = self.tree.get(a)?.parent?;
            }
            if a == root {
                return None;
            }
            subs.push(a);
        }
        let tree = &self.tree;
        let all = subs.clone();
        subs.retain(|a| !all.iter().any(|b| b != a && tree.is_ancestor(*b, *a)));
        subs.sort();
        subs.dedup();
        let mut frames = Vec::with_capacity(subs.len());
        for a in &subs {
            frames.push(*old.rects.get(a)?);
        }
        let sizes = self.anim.size_overrides(&self.tree, root, rest);
        let mut boxes = old.clone();
        for (a, frame) in subs.iter().zip(frames) {
            let b = self.run_layout(*a, RootSize::Fixed(frame), info, &sizes);
            let mut stack: Vec<NodeId> = self.tree.get(*a)?.children.clone();
            while let Some(n) = stack.pop() {
                boxes.rects.remove(&n);
                boxes.residues.remove(&n);
                if let Some(node) = self.tree.get(n) {
                    stack.extend(node.children.iter().copied());
                }
            }
            for (k, v) in b.rects {
                if k != *a {
                    boxes.rects.insert(k, v);
                }
            }
            // Laid out in the frame `a` was given, its rounded box: the
            // residues are relative to it, as `a`'s own are kept.
            for (k, v) in b.residues {
                if k != *a {
                    boxes.residues.insert(k, v);
                }
            }
        }
        Some(boxes)
    }
}

/// (growth) `placed` at its exact share of the room ([`RootSize::Placed`]).
fn exact_of(placed: RootSize) -> RootSize {
    match placed {
        RootSize::Placed {
            frame,
            width,
            height,
            at,
            ..
        } => RootSize::Placed {
            frame,
            width,
            height,
            at,
            exact: true,
        },
        other => other,
    }
}

/// (growth) The sizes a content-sized surface's root `root` is laid out
/// with while its box springs over a step: `sizes`, its box at its
/// content's size `content` (as `free`, laid out with the box free, has
/// it) plus `step` on each axis that steps (an overshoot past the size
/// its buffer was configured at is cut by the buffer), and the boxes that
/// follow it there.
/// A box follows its parent on an axis when it is a container with no
/// size of its own there (from the root down, and not into a `scroll`,
/// `list` or `pages`, which lay out their own) and either fills it (its box, with
/// its margins, spans its parent's less the padding, at rest and where
/// the last frame drew them: a card filling a panel) or is where the step is (the last frame drew it larger or
/// smaller than it lies at rest by as much as the root: a grid that lost
/// a row): it takes its size at rest plus as much as the root differs
/// from its content. Whatever else they hold keeps its size at rest
/// there ([`crate::anim::Forced::keep`]): no row is squeezed, no text
/// wrapped anew by the motion, and what it holds flows in it as in any
/// box: what a change moved inside it glides there from where it was
/// drawn (FLIP, along the step's curve: [`Renderer::flip_curve`]), so a
/// bottom OSD's last row stays on its edge as a row mounts above it.
/// `drawn`: the sizes the last frame drew. Returned with the forced
/// sizes: per axis, the root and the boxes that follow it, which clip
/// what they hold while they do ([`Animator::set_step_fills`]).
fn step_layout(
    tree: &SceneTree,
    root: NodeId,
    (free, drawn): (&Boxes, &HashMap<NodeId, [f32; 2]>),
    sizes: &SizeMap,
    content: [f32; 2],
    step: [Option<f32>; 2],
) -> (SizeMap, [HashSet<NodeId>; 2]) {
    let mut forced = sizes.clone();
    let mut chains = [HashSet::from([root]), HashSet::from([root])];
    let mut grown = [0.0f32; 2];
    let f = forced.entry(root).or_default();
    for (axis, d) in step.iter().enumerate() {
        if let Some(d) = d {
            let size = (content[axis] + d).max(0.0);
            f.size[axis] = Some(size);
            grown[axis] = size - content[axis];
        }
    }
    for axis in 0..2 {
        if step[axis].is_none() {
            continue;
        }
        let d = grown[axis];
        // How much larger than at rest the last frame drew the root.
        let was_by = drawn.get(&root).map(|w| w[axis] - content[axis]);
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            let (Some(node), Some(nb)) = (tree.get(n), free.rects.get(&n)) else {
                continue;
            };
            let pad = resolved_insets(tree, n, Prop::Pad);
            let inner = span(nb, axis) - pad_sum(pad, axis);
            for c in &node.children {
                let (Some(cn), Some(cb)) = (tree.get(*c), free.rects.get(c)) else {
                    continue;
                };
                if cn.kind.is_surface() {
                    continue;
                }
                let margin = resolved_insets(tree, *c, Prop::Margin);
                let outer = span(cb, axis) + pad_sum(margin, axis);
                let by = drawn.get(c).map(|w| w[axis] - span(cb, axis));
                let stepped = matches!((by, was_by), (Some(b), Some(w))
                    if b.abs() > 0.5 && (b - w).abs() < 0.5);
                // It fills its parent where the last frame drew them both
                // too: a box alone in one sized by its content spans it at
                // rest, but one that a leaving sibling held larger did not
                // (a stack crossfading to a smaller card).
                let was_fill = match (drawn.get(c), drawn.get(&n)) {
                    (Some(cw), Some(pw)) => {
                        (cw[axis] + pad_sum(margin, axis) - (pw[axis] - pad_sum(pad, axis))).abs()
                            < 0.5
                    }
                    _ => true,
                };
                let fill = !crate::layout::is_leaf(cn.kind)
                    && !own_size(cn, axis)
                    && (((outer - inner).abs() < 0.5 && was_fill) || stepped);
                let e = forced.entry(*c).or_default();
                if e.size[axis].is_some() {
                    // Its own size springs: it keeps to that.
                    continue;
                }
                if fill {
                    e.size[axis] = Some((span(cb, axis) + d).max(0.0));
                    chains[axis].insert(*c);
                    // A `scroll`, `list` or `pages` lays out its own.
                    if !matches!(cn.kind, NodeKind::Scroll | NodeKind::List | NodeKind::Pages) {
                        stack.push(*c);
                    }
                } else {
                    e.keep[axis] = Some(span(cb, axis));
                }
            }
        }
    }
    (forced, chains)
}

/// (growth) The boxes of a content-sized surface whose box springs over
/// a step, as `forced` laid them out at the size in flight, but on an
/// axis where what it holds keeps to the far edge (`fills.far`: a bottom
/// OSD's rows to its bottom edge, which the compositor keeps): there,
/// what does not follow the box lies as `free` laid it out at rest, moved
/// with the far edge of the box that follows nearest above it. So the
/// rows by that edge are drawn exactly where they rest, never by a flow
/// from the moving near edge that a glide has to cancel.
fn keep_far(
    tree: &SceneTree,
    root: NodeId,
    free: &Boxes,
    forced: Boxes,
    fills: &StepFills,
) -> Boxes {
    if !fills.far.iter().any(|f| *f) {
        return forced;
    }
    let mut out = forced;
    let far = |r: &LogicalRect, axis: usize| if axis == 0 { r.x + r.w } else { r.y + r.h };
    let mut stack = vec![(root, [0.0f32; 2])];
    while let Some((n, moved)) = stack.pop() {
        let Some(node) = tree.get(n) else { continue };
        let mut next = moved;
        let (Some(rest), Some(laid)) = (free.rects.get(&n).copied(), out.rects.get(&n).copied())
        else {
            continue;
        };
        let mut r = laid;
        for axis in 0..2 {
            if !fills.far[axis] {
                continue;
            }
            if fills.chains[axis].contains(&n) {
                next[axis] = far(&laid, axis) - far(&rest, axis);
                continue;
            }
            let res = free.residues.get(&n).copied();
            if axis == 0 {
                r.x = rest.x + moved[0];
            } else {
                r.y = rest.y + moved[1];
            }
            let e = out.residues.entry(n).or_insert([0.0; 4]);
            let fr = res.unwrap_or([0.0; 4]);
            e[axis] = fr[axis];
            e[axis + 2] = fr[axis + 2];
        }
        out.rects.insert(n, r);
        if out
            .residues
            .get(&n)
            .is_some_and(|v| v.iter().all(|e| *e == 0.0))
        {
            out.residues.remove(&n);
        }
        for c in &node.children {
            stack.push((*c, next));
        }
    }
    out
}

/// A box's length on `axis` (0 the width, 1 the height).
fn span(r: &strand_scene::LogicalRect, axis: usize) -> f32 {
    if axis == 0 { r.w } else { r.h }
}

/// The two sides of `i` along `axis`.
fn pad_sum(i: strand_scene::Insets, axis: usize) -> f32 {
    if axis == 0 {
        i.left + i.right
    } else {
        i.top + i.bottom
    }
}

/// `prop` (`pad`, `margin`) of `id`, its tokens resolved; none: zero.
fn resolved_insets(tree: &SceneTree, id: NodeId, prop: Prop) -> strand_scene::Insets {
    let Some(node) = tree.get(id) else {
        return strand_scene::Insets::default();
    };
    let tables = crate::flatten::scope_tables(tree, id);
    let scope = TokenScope::new(&tables);
    node.get(prop)
        .and_then(|v| scope.resolve(v))
        .and_then(|v| v.insets())
        .filter(|i| {
            [i.top, i.right, i.bottom, i.left]
                .iter()
                .all(|v| v.is_finite())
        })
        .unwrap_or_default()
}

/// Whether `node` has a size of its own on `axis`: a `width` (`height`)
/// or `size` other than `auto`.
fn own_size(node: &crate::tree::Node, axis: usize) -> bool {
    let auto = |v: Option<&strand_scene::PropValue>| match v {
        None => true,
        Some(strand_scene::PropValue::Keyword(k)) => k == "auto",
        Some(strand_scene::PropValue::Length(strand_scene::Length::Auto)) => true,
        Some(_) => false,
    };
    let prop = if axis == 0 { Prop::Width } else { Prop::Height };
    !(auto(node.get(prop)) && auto(node.get(Prop::Size)))
}

/// (launcher-flip-pass) The first pass after a paint replaced `old`,
/// the boxes that frame drew (laid out in a buffer of `size`): they stay
/// what a later pass before the next frame compares against.
fn keep_drawn(
    s: &mut super::SurfaceState,
    old: Option<Boxes>,
    size: Option<strand_scene::LogicalSize>,
) {
    if s.painted
        && s.drawn_boxes.is_none()
        && let Some(old) = old
    {
        s.drawn_boxes = Some((old.rects, size));
    }
}

/// `boxes` moved by `-shift` (into a buffer whose near edge moved
/// `shift` on screen), borrowed when nothing moves.
fn moved_by(
    boxes: &HashMap<NodeId, strand_scene::LogicalRect>,
    shift: (f32, f32),
) -> std::borrow::Cow<'_, HashMap<NodeId, strand_scene::LogicalRect>> {
    if shift == (0.0, 0.0) {
        return std::borrow::Cow::Borrowed(boxes);
    }
    std::borrow::Cow::Owned(
        boxes
            .iter()
            .map(|(k, r)| {
                let r = strand_scene::LogicalRect {
                    x: r.x - shift.0,
                    y: r.y - shift.1,
                    ..*r
                };
                (*k, r)
            })
            .collect(),
    )
}

/// (bottom-osd-shrink) How far a resize from a buffer of logical size
/// `was` to one of `now` moved the buffer's near edge on screen (`share`:
/// the part of a resize each axis's near edge takes). (growth) A growth
/// too: a content-sized surface's box is placed where the compositor
/// keeps it in the larger buffer (`RootSize::Placed`), and nothing glides
/// it by the compositor's move besides.
fn buffer_shift(
    was: Option<strand_scene::LogicalSize>,
    now: strand_scene::LogicalSize,
    share: Option<(f32, f32)>,
) -> (f32, f32) {
    match (was, share) {
        (Some(was), Some((fx, fy))) => {
            let shift = crate::layout::edge_shift;
            (shift(was.w, now.w, fx), shift(was.h, now.h, fy))
        }
        _ => (0.0, 0.0),
    }
}

/// (launcher-flip-pass) What the FLIP passes since the last frame gave
/// a node: the shift they added up to, and the axes they glided it on.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Glided {
    by: (f32, f32),
    axes: (bool, bool),
}

/// The boxes a FLIP compares: those the last frame drew (`old`, moved
/// into this buffer) and this pass's (`new`).
pub(super) struct Compare<'a> {
    pub old: &'a HashMap<NodeId, strand_scene::LogicalRect>,
    pub new: &'a HashMap<NodeId, strand_scene::LogicalRect>,
    /// (launcher-flip-pass) A pass that is no FLIP of its own, after one
    /// before the frame: it moves only what that pass glided, on the axes
    /// it glided it on where it now lies where it was drawn; what else it
    /// moves, the sizes in flight move.
    pub glided_only: bool,
}

/// FLIP: every node whose parent is in `parents` (all with `all`) and
/// whose box moved starts where it was and glides to its new place;
/// one never drawn (`unpainted`) has no place it was.
/// Visited top-down, so a node that moved with its parent adds nothing
/// to the parent's glide.
///
/// (clip-blink) A box that clips what it holds (`clip: true`, a
/// `scroll`, `list` or `pages`, a box whose size springs or that exits
/// to a size) carries it, glided or snapped: its content glides only by
/// how far it moved inside it. And a glide never starts a node where
/// the boxes that clip it hide what they showed of it (one they shrank
/// past): its start moves by as little as it takes to show as much of
/// it, on each axis, as showed before.
///
/// (launcher-flip-pass) Every pass laid out before a frame draws it
/// compares the boxes that frame's predecessor drew (`old`), so a node
/// starts where it was drawn however many passes a change takes (a size
/// spring's in-flight pass, a pass when a text arrives): each sets the
/// glide its node needs in all, replacing what the earlier ones gave it
/// (`shifts`, kept until the frame), and what one never drawn laid out
/// (a box squeezed, a text wrapped tall) counts for nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn glide_moved(
    tree: &SceneTree,
    anim: &mut Animator,
    root: NodeId,
    boxes: Compare<'_>,
    parents: &HashSet<NodeId>,
    all: bool,
    unpainted: &HashSet<NodeId>,
    shifts: &mut HashMap<NodeId, Glided>,
    curve: Curve,
) {
    let Compare {
        old,
        new,
        glided_only,
    } = boxes;
    // (growth) While `root`'s box springs over a step, the boxes that
    // follow it move with it, never glided on that axis; on an axis where
    // what they hold keeps to their far edge they do not carry it either
    // (it lies where it rests: `keep_far`).
    let fills = anim.step_fills(root).cloned();
    let follows =
        |c: &NodeId, axis: usize| fills.as_ref().is_some_and(|f| f.chains[axis].contains(c));
    let far = |axis: usize| fills.as_ref().is_some_and(|f| f.far[axis]);
    // Each entry: a node; where its children's glides are measured from
    // (the glides its ancestors are given in all, or, under a clip, the
    // clip's own move); how far it is drawn off its box on the glide's
    // first frame; and the boxes its clipping ancestors cut it to, before
    // and as drawn on that frame.
    let zero = (0.0f32, 0.0f32);
    let add = |a: (f32, f32), b: (f32, f32)| (a.0 + b.0, a.1 + b.1);
    let big = |d: (f32, f32)| d.0.abs() > 0.5 || d.1.abs() > 0.5;
    let mut stack = vec![(root, zero, zero, None::<(Span2, Span2)>)];
    while let Some((n, acc, real, clip)) = stack.pop() {
        let Some(node) = tree.get(n) else { continue };
        let scope = all || parents.contains(&n);
        for c in &node.children {
            let Some(cn) = tree.get(*c).filter(|k| !k.kind.is_surface()) else {
                continue;
            };
            let rects = (old.get(c), new.get(c));
            let had = shifts.get(c).copied();
            // What the passes before this one since the frame gave it.
            let given = had.map_or(zero, |g| g.by);
            let mut total = given;
            if !unpainted.contains(c)
                && let (Some(o), Some(w)) = rects
            {
                if glided_only {
                    // Where it lies where it was drawn, it did not move:
                    // what those passes glided it by there is taken back.
                    if let Some(g) = had {
                        if g.axes.0 && (o.x - w.x).abs() < 0.5 {
                            total.0 = o.x - w.x - acc.0;
                        }
                        if g.axes.1 && (o.y - w.y).abs() < 0.5 {
                            total.1 = o.y - w.y - acc.1;
                        }
                    }
                } else if scope {
                    let mut adj = (o.x - w.x - acc.0, o.y - w.y - acc.1);
                    if let Some((was, now)) = &clip {
                        let (old_box, start) = (Span2::of(o), Span2::of(w).shifted(real));
                        let shift = |i: usize| {
                            let d = [adj.0, adj.1][i];
                            keep_shown(old_box.0[i], was.0[i], start.0[i].shifted(d), now.0[i])
                        };
                        adj = (adj.0 + shift(0), adj.1 + shift(1));
                    }
                    if follows(c, 0) {
                        adj.0 = 0.0;
                    }
                    if follows(c, 1) {
                        adj.1 = 0.0;
                    }
                    total = if big(adj) { adj } else { zero };
                }
            }
            let delta = (total.0 - given.0, total.1 - given.1);
            let glided = if big(delta) {
                anim.glide(*c, [delta.0, delta.1], curve);
                let g = shifts.entry(*c).or_default();
                g.by = total;
                g.axes.0 |= total.0.abs() > 0.5;
                g.axes.1 |= total.1.abs() > 0.5;
                total
            } else {
                given
            };
            let (mut acc_c, real_c) = (add(acc, glided), add(real, glided));
            let mut clip_c = clip;
            if !glided_only
                && clips(tree, anim, cn)
                && !unpainted.contains(c)
                && let (Some(o), Some(w)) = rects
            {
                // What it holds glides from where it was inside it.
                acc_c = (o.x - w.x, o.y - w.y);
                if follows(c, 0) && far(0) {
                    acc_c.0 = acc.0 + glided.0;
                }
                if follows(c, 1) && far(1) {
                    acc_c.1 = acc.1 + glided.1;
                }
                let (was, now) = (Span2::of(o), Span2::of(w).shifted(real_c));
                clip_c = Some(match clip {
                    Some((a, b)) => (a.cut(was), b.cut(now)),
                    None => (was, now),
                });
            }
            stack.push((*c, acc_c, real_c, clip_c));
        }
    }
}

/// A box as its two spans, x then y.
#[derive(Clone, Copy, Debug)]
struct Span2([Span; 2]);

/// `[lo, hi)` on one axis.
#[derive(Clone, Copy, Debug)]
struct Span {
    lo: f32,
    hi: f32,
}

impl Span {
    fn len(self) -> f32 {
        (self.hi - self.lo).max(0.0)
    }
    fn shifted(self, d: f32) -> Span {
        Span {
            lo: self.lo + d,
            hi: self.hi + d,
        }
    }
    /// How much of `self` lies in `clip`.
    fn shown(self, clip: Span) -> f32 {
        (self.hi.min(clip.hi) - self.lo.max(clip.lo)).max(0.0)
    }
}

impl Span2 {
    fn of(r: &strand_scene::LogicalRect) -> Span2 {
        Span2([
            Span {
                lo: r.x,
                hi: r.x + r.w,
            },
            Span {
                lo: r.y,
                hi: r.y + r.h,
            },
        ])
    }
    fn shifted(self, d: (f32, f32)) -> Span2 {
        Span2([self.0[0].shifted(d.0), self.0[1].shifted(d.1)])
    }
    fn cut(self, o: Span2) -> Span2 {
        let c = |a: Span, b: Span| Span {
            lo: a.lo.max(b.lo),
            hi: a.hi.min(b.hi).max(a.lo.max(b.lo)),
        };
        Span2([c(self.0[0], o.0[0]), c(self.0[1], o.0[1])])
    }
}

/// (clip-blink) Where a glide starts a node on one axis: at `start`
/// unless its clip `now` would hide more of it than its clip `was` hid
/// of its last box `old`; then moved by as little as it takes to show as
/// much as it showed (no more than fits in the clip). Returns the
/// shift. A node its clip hid wholly is left where it was.
fn keep_shown(old: Span, was: Span, start: Span, now: Span) -> f32 {
    let want = old.shown(was).min(now.len()).min(start.len());
    if want <= 0.0 || start.shown(now) >= want - 0.5 {
        return 0.0;
    }
    // Showing `want` of it: its low end lies in [now.lo - (len - want),
    // now.hi - want].
    let (min, max) = (now.lo - (start.len() - want), now.hi - want);
    start.lo.clamp(min, max.max(min)) - start.lo
}

/// (clip-blink) Whether `node` clips what it holds, as flattening does
/// (`flatten/node.rs`, its children's clip).
fn clips(tree: &SceneTree, anim: &Animator, node: &crate::tree::Node) -> bool {
    matches!(
        node.get(Prop::Clip),
        Some(strand_scene::PropValue::Bool(true))
    ) || matches!(
        node.kind,
        strand_scene::NodeKind::Scroll
            | strand_scene::NodeKind::List
            | strand_scene::NodeKind::Pages
    ) || anim.sizing(tree, node)
}

/// The larger of `a` and `b` on each side.
fn max_insets(a: strand_scene::Insets, b: strand_scene::Insets) -> strand_scene::Insets {
    strand_scene::Insets {
        top: a.top.max(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
        left: a.left.max(b.left),
    }
}

//! The layout step of a frame: when a surface lays out again, size
//! springs and FLIP glides, layout facts for logic, and flattening.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::Curve;
use strand_scene::{LogicalRect, NodeId, NodeKind, Prop, Scale, SurfaceId, TokenScope};

use super::Renderer;
use super::specs::{centred_overhang, within_edges};
use super::text::TextSlot;
use crate::anim::{Animator, SizeMap};
use crate::flatten::{Flattened, Shaped, flatten, pick};
use crate::layout::{Boxes, RootSize, TextSizes, layout};
use crate::tree::SceneTree;

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

    /// (popup-shrink) How the surface node `root` is laid out in `frame`,
    /// its buffer's box. A content-sized panel, OSD or popup held at its
    /// larger size while something on it moves (it shrinks once nothing
    /// does, [`Renderer::release_holds`]) is laid out at its content's
    /// size and placed where the compositor will keep the smaller box
    /// ([`super::specs::resize_share`]): its content is drawn where it
    /// will be on screen, so nothing moves when the buffer shrinks, as
    /// [`Renderer::glide_origin`] keeps it in place when the buffer
    /// grows. Any other root fills its frame: a bar's or a lock's size is
    /// the output's, and a surface configured at another size than it
    /// asked for is drawn at the size it has.
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
        }
    }

    /// (bottom-osd-shrink) How a content-sized surface's buffer keeps
    /// its place when it shrinks ([`super::specs::resize_share`]); `None`
    /// for any other surface. Its growth is [`Renderer::glide_origin`]'s.
    fn shrink_share(&self, root: NodeId) -> Option<(f32, f32)> {
        let spec = self.specs.get(&root)?;
        if spec.kind == NodeKind::Bar || !self.content_sized.contains_key(&root) {
            return None;
        }
        self.resize_share_of(root)
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
        if !full
            && let Some(old) = &old
            && let Some(boxes) = self.relayout_sized(root, old, &rest, &info)
        {
            let moving = self.anim.sizes_moving(&self.tree, root);
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.boxes = Some(boxes);
                s.anim_layout = moving;
            }
            self.place_waiting_popups(root);
            self.report_facts(id);
            return;
        }
        let sized = self.anim.size_work(&self.tree, root);
        let mut sizes = rest.clone();
        // The shadows' reach where size springs that start here land.
        let mut rest_reach = None;
        let mut boxes = if sized && !full && !self.anim.size_pending(&self.tree, root) {
            // Only springs moved: their targets are the ones learnt when
            // they started (nothing layout reads changed since), so one
            // pass with the in-flight sizes is enough.
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            self.run_layout(root, root_size, &info, &sizes)
        } else if sized {
            let at_rest = self.run_layout(root, root_size, &info, &rest);
            rest_reach = Some(crate::fillet::grow(
                &self.tree,
                &at_rest,
                root,
                at_rest.overhang,
            ));
            let old_rects = old.as_ref().map(|b| &b.rects);
            self.anim
                .start_sizes(&self.tree, root, &at_rest.rects, old_rects, false);
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            if sizes == rest {
                at_rest
            } else {
                self.run_layout(root, root_size, &info, &sizes)
            }
        } else {
            self.run_layout(root, root_size, &info, &rest)
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
        if let Some(s) = self.surfaces.get_mut(&id) {
            let flip = std::mem::take(&mut s.flip);
            let all = std::mem::take(&mut s.flip_all);
            let text = std::mem::take(&mut s.text_arrived);
            // (eval-coverage) Laid out in a resized buffer: a content-
            // sized surface configured at the size a change asked for.
            // Before its first frame nothing was drawn to glide from.
            let resized = s.painted && s.laid_frame.is_some_and(|f| f != frame);
            // (bottom-osd-shrink) Where a shrink moved the buffer's near
            // edge on screen: boxes are compared where they were drawn
            // on screen, not in the buffer.
            let shift = match (resized, s.laid_size, share) {
                (true, Some(was), Some((fx, fy))) => {
                    let shrink = |from: f32, to: f32, f: f32| {
                        crate::layout::edge_shift(from, to, f).max(0.0)
                    };
                    (shrink(was.w, logical.w, fx), shrink(was.h, logical.h, fy))
                }
                _ => (0.0, 0.0),
            };
            s.laid_frame = Some(frame);
            s.laid_size = Some(logical);
            let fresh = all || !flip.is_empty();
            if fresh {
                let open = s.flip_open.get_or_insert_with(Default::default);
                open.0.extend(flip);
                open.1 |= all;
                s.flip_drawn = false;
            }
            let scope = s.flip_open.clone().filter(|_| fresh || text || resized);
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
                let moved;
                let old_rects = if shift == (0.0, 0.0) {
                    &old.rects
                } else {
                    moved = old
                        .rects
                        .iter()
                        .map(|(k, r)| {
                            let r = strand_scene::LogicalRect {
                                x: r.x - shift.0,
                                y: r.y - shift.1,
                                ..*r
                            };
                            (*k, r)
                        })
                        .collect::<HashMap<_, _>>();
                    &moved
                };
                let tables = [&self.tree.tokens];
                let curve = Curve::of(
                    &TokenScope::new(&tables)
                        .transition(&strand_scene::Transition::Default, Prop::X),
                );
                glide_moved(
                    &self.tree,
                    &mut self.anim,
                    root,
                    old_rects,
                    &boxes.rects,
                    &flip,
                    all,
                    &s.unpainted,
                    curve,
                );
            }
        }
        // Springs a clockless preview left to start lay out again on the
        // first frame painted.
        let moving =
            self.anim.sizes_moving(&self.tree, root) || self.anim.size_pending(&self.tree, root);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.boxes = Some(boxes);
            s.layout_dirty = false;
            s.anim_layout = moving;
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
#[allow(clippy::too_many_arguments)]
pub(super) fn glide_moved(
    tree: &SceneTree,
    anim: &mut Animator,
    root: NodeId,
    old: &HashMap<NodeId, strand_scene::LogicalRect>,
    new: &HashMap<NodeId, strand_scene::LogicalRect>,
    parents: &HashSet<NodeId>,
    all: bool,
    unpainted: &HashSet<NodeId>,
    curve: Curve,
) {
    // Each entry: a node; where its children's glides are measured from
    // (the glides its ancestors were given here, or, under a clip, the
    // clip's own move); how far it is drawn off its box on the glide's
    // first frame; and the boxes its clipping ancestors cut it to, before
    // and as drawn on that frame.
    let zero = (0.0f32, 0.0f32);
    let mut stack = vec![(root, zero, zero, None::<(Span2, Span2)>)];
    while let Some((n, acc, real, clip)) = stack.pop() {
        let Some(node) = tree.get(n) else { continue };
        let scope = all || parents.contains(&n);
        for c in &node.children {
            let Some(cn) = tree.get(*c).filter(|k| !k.kind.is_surface()) else {
                continue;
            };
            let (mut acc_c, mut real_c) = (acc, real);
            let rects = (old.get(c), new.get(c));
            if scope
                && !unpainted.contains(c)
                && let (Some(o), Some(w)) = rects
            {
                let mut adj = (o.x - w.x - acc.0, o.y - w.y - acc.1);
                if let Some((was, now)) = &clip {
                    let (old_box, start) = (Span2::of(o), Span2::of(w).shifted(real));
                    let shift = |i: usize| {
                        let d = [adj.0, adj.1][i];
                        keep_shown(old_box.0[i], was.0[i], start.0[i].shifted(d), now.0[i])
                    };
                    adj = (adj.0 + shift(0), adj.1 + shift(1));
                }
                if adj.0.abs() > 0.5 || adj.1.abs() > 0.5 {
                    anim.glide(*c, [adj.0, adj.1], curve);
                    acc_c = (acc.0 + adj.0, acc.1 + adj.1);
                    real_c = (real.0 + adj.0, real.1 + adj.1);
                }
            }
            let mut clip_c = clip;
            if clips(tree, anim, cn)
                && !unpainted.contains(c)
                && let (Some(o), Some(w)) = rects
            {
                // What it holds glides from where it was inside it.
                acc_c = (o.x - w.x, o.y - w.y);
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

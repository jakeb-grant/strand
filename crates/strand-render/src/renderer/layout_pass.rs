//! The layout step of a frame: when a surface lays out again, size
//! springs and FLIP glides, layout facts for logic, and flattening.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::Curve;
use strand_scene::{NodeId, Prop, PropValue, Scale, SurfaceId, TokenScope};

use super::Renderer;
use super::specs::centred_overhang;
use super::text::TextSlot;
use crate::anim::{Animator, SizeMap};
use crate::flatten::{Flattened, Shaped, flatten, pick};
use crate::layout::{Boxes, RootSize, TextSizes, layout};
use crate::tree::SceneTree;

/// Delivered text layouts as layout measures them.
pub(super) struct TextInfo<'a> {
    pub(super) shaped: &'a HashMap<NodeId, Vec<Shaped>>,
    pub(super) scale: Scale,
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

    /// Laid-out sizes that changed since the last call, `(node, width,
    /// height)` in logical pixels, of the nodes logic reads them of
    /// (`Prop::Watch`): what `self.width` and container queries read. A
    /// node shown on several surfaces reports the size of the last one
    /// laid out. A non-empty batch gets the next [`Renderer::layout_seq`].
    pub fn take_layout_facts(&mut self) -> Vec<(NodeId, f32, f32)> {
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
        for _ in 0..6 {
            let flat = self.flatten_now(id);
            let text = self.request_text(&flat.text);
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
            s.wanted = f
                .text
                .iter()
                .map(|(node, spec)| TextSlot::of(*node, spec))
                .collect();
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
        };
        // Replaced below on every path: taken, not copied.
        let old = self.surfaces.get_mut(&id).and_then(|s| s.boxes.take());
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
            self.report_facts(id);
            return;
        }
        let sized = self.anim.size_work(&self.tree, root);
        let mut sizes = rest.clone();
        let mut boxes = if sized && !full && !self.anim.size_pending(&self.tree, root) {
            // Only springs moved: their targets are the ones learnt when
            // they started (nothing layout reads changed since), so one
            // pass with the in-flight sizes is enough.
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            self.run_layout(root, RootSize::Fixed(frame), &info, &sizes)
        } else if sized {
            let at_rest = self.run_layout(root, RootSize::Fixed(frame), &info, &rest);
            let old_rects = old.as_ref().map(|b| &b.rects);
            self.anim
                .start_sizes(&self.tree, root, &at_rest.rects, old_rects, false);
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            if sizes == rest {
                at_rest
            } else {
                self.run_layout(root, RootSize::Fixed(frame), &info, &sizes)
            }
        } else {
            self.run_layout(root, RootSize::Fixed(frame), &info, &rest)
        };
        if !self.content_sized.contains(&root)
            && let Some(mut spec) = self.specs.get(&root).cloned()
            && spec.open
        {
            let grown = crate::fillet::grow(&self.tree, &boxes, root, boxes.overhang);
            let o = crate::fillet::flush(&self.tree, root, centred_overhang(&spec, grown));
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
        if let Some(s) = self.surfaces.get_mut(&id) {
            let flip = std::mem::take(&mut s.flip);
            let all = std::mem::take(&mut s.flip_all);
            if let Some(old) = &old
                && (all || !flip.is_empty())
                && !self.anim.reduced()
            {
                let tables = [&self.tree.tokens];
                let curve = Curve::of(
                    &TokenScope::new(&tables)
                        .transition(&strand_scene::Transition::Default, Prop::X),
                );
                glide_moved(
                    &self.tree,
                    &mut self.anim,
                    root,
                    &old.rects,
                    &boxes.rects,
                    &flip,
                    all,
                    curve,
                );
            }
        }
        let moving = self.anim.sizes_moving(&self.tree, root);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.boxes = Some(boxes);
            s.layout_dirty = false;
            s.anim_layout = moving;
        }
        self.report_facts(id);
    }

    /// Hands the laid-out sizes of watched nodes that changed to logic,
    /// holding the frame for a container query's answer.
    pub(super) fn report_facts(&mut self, id: SurfaceId) {
        let Some(boxes) = self.surfaces.get(&id).and_then(|s| s.boxes.as_ref()) else {
            return;
        };
        let mut query = false;
        let mut facts = Vec::new();
        for (node, r) in &boxes.rects {
            if self.tree.is_ghost(*node) {
                continue;
            }
            let Some(watch) = self.tree.get(*node).and_then(|n| n.get(Prop::Watch)) else {
                continue;
            };
            let size = (r.w, r.h);
            if self.facts_sent.get(&(id, *node)) != Some(&size) {
                facts.push((*node, size));
                query |= matches!(watch, PropValue::Keyword(k) if k == "query");
            }
        }
        for (node, size) in facts {
            self.facts_sent.insert((id, node), size);
            self.facts.push((node, size.0, size.1));
        }
        let (wait, window) = (self.query_wait, self.busy_window);
        let seq = self.facts_seq + 1;
        if let Some(s) = self.surfaces.get_mut(&id)
            && query
        {
            s.hold_for_query(seq, wait, window);
        }
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
                    if self.content_sized.contains(&root) {
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
                if let Some(node) = self.tree.get(n) {
                    stack.extend(node.children.iter().copied());
                }
            }
            for (k, v) in b.rects {
                if k != *a {
                    boxes.rects.insert(k, v);
                }
            }
        }
        Some(boxes)
    }
}

/// FLIP: every node whose parent is in `parents` (all with `all`) and
/// whose box moved starts where it was and glides to its new place.
/// Visited top-down, so a node that moved with its parent adds nothing
/// to the parent's glide.
#[allow(clippy::too_many_arguments)]
pub(super) fn glide_moved(
    tree: &SceneTree,
    anim: &mut Animator,
    root: NodeId,
    old: &HashMap<NodeId, strand_scene::LogicalRect>,
    new: &HashMap<NodeId, strand_scene::LogicalRect>,
    parents: &HashSet<NodeId>,
    all: bool,
    curve: Curve,
) {
    let mut stack = vec![(root, (0.0f32, 0.0f32))];
    while let Some((n, acc)) = stack.pop() {
        let Some(node) = tree.get(n) else { continue };
        let scope = all || parents.contains(&n);
        for c in &node.children {
            if tree.get(*c).is_none_or(|k| k.kind.is_surface()) {
                continue;
            }
            let mut acc_c = acc;
            if scope && let (Some(o), Some(w)) = (old.get(c), new.get(c)) {
                let adj = (o.x - w.x - acc.0, o.y - w.y - acc.1);
                if adj.0.abs() > 0.5 || adj.1.abs() > 0.5 {
                    anim.glide(*c, [adj.0, adj.1], curve);
                    acc_c = (acc.0 + adj.0, acc.1 + adj.1);
                }
            }
            stack.push((*c, acc_c));
        }
    }
}

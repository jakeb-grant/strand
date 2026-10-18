//! Surfaces attached to the renderer: attach, configure, detach, and
//! hit testing their last frame.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use strand_scene::Curve;
use strand_scene::{
    Damage, LogicalPoint, LogicalRect, NodeId, NodeKind, Prop, Scale, Size, SurfaceId, TokenScope,
};

use super::{Renderer, SurfaceState};

impl Renderer {
    /// `node`'s laid-out box on `surface`, in surface logical pixels
    /// (before paint offsets).
    pub fn node_rect(&self, surface: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        self.surfaces
            .get(&surface)?
            .boxes
            .as_ref()?
            .rects
            .get(&node)
            .copied()
    }

    /// (m6-owner-fixes) `at`, a point on `surface`, moved into `node`'s
    /// laid-out box ([`Renderer::node_rect`]): the paint offsets (`x`,
    /// `y`, a glide) and transforms (`scale`, `rotate`) the node was last
    /// drawn with, its own and its ancestors', are undone, so a slider or
    /// a `segmented` reads the pointer where it is drawn (spec.md 6.7:
    /// input follows the drawn shape). `at` unchanged when the node was
    /// not drawn there.
    pub fn to_laid(&self, surface: SurfaceId, node: NodeId, at: LogicalPoint) -> LogicalPoint {
        let Some(s) = self.surfaces.get(&surface) else {
            return at;
        };
        let (Some(laid), Some(h)) = (
            self.node_rect(surface, node),
            s.hits.iter().rev().find(|h| h.node == node),
        ) else {
            return at;
        };
        let k = s.scale.as_f64();
        let mut p = vello_cpu::kurbo::Point::new(at.x as f64 * k, at.y as f64 * k);
        if let Some(inv) = h.inverse {
            p = inv * p;
        }
        let x = laid.x as f64 + (p.x - (h.rect.x0 + h.grow)) / k;
        let y = laid.y as f64 + (p.y - (h.rect.y0 + h.grow)) / k;
        if !(x.is_finite() && y.is_finite()) {
            return at;
        }
        LogicalPoint::new(x as f32, y as f32)
    }

    /// (M4) `node` on `surface` drawn alone, at rest (not lifted, no
    /// glide), at the surface's scale and with what it inherits there:
    /// the icon that follows the pointer while the compositor carries a
    /// drag out of the surface. `None` when it is not laid out there or
    /// has no area on it.
    pub fn drag_image(
        &mut self,
        surface: SurfaceId,
        node: NodeId,
    ) -> Option<strand_scene::DragImage> {
        let rect = self.node_rect(surface, node)?;
        let s = self.surfaces.get(&surface)?;
        let (size, scale) = (s.size, s.scale);
        let time = s.painted_time.unwrap_or_default();
        let x0 = scale.round(rect.x).clamp(0, size.w as i64);
        let y0 = scale.round(rect.y).clamp(0, size.h as i64);
        let x1 = scale.round(rect.x + rect.w).clamp(0, size.w as i64);
        let y1 = scale.round(rect.y + rect.h).clamp(0, size.h as i64);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let box_ = strand_scene::Rect::from_edges(x0, y0, x1, y1);
        let layouts = self.shaped();
        let still = self.anim.hold_still(node);
        // A frame that commits nothing: no clock, play or glide moves.
        self.anim.begin(time, None, false);
        let f = match self.surfaces.get(&surface).and_then(|s| s.boxes.as_ref()) {
            Some(boxes) => crate::flatten::flatten(
                &self.tree,
                node,
                size,
                scale,
                &layouts,
                boxes,
                &mut self.anim,
                &self.extras,
            ),
            None => crate::flatten::Flattened::default(),
        };
        self.anim.release(node, still);
        // Drawn into a buffer of the box's size, the surface's pixels
        // from its corner on.
        let (w, h) = (box_.w as usize, box_.h as usize);
        let mut pixels = vec![0u8; w * h * 4];
        let target = strand_scene::PaintTarget::new(
            &mut pixels,
            Size::new(box_.w, box_.h),
            box_.w * 4,
            scale,
            0,
        )
        .ok()?;
        let mut target = target.at(time);
        self.raster.paint_window(
            &f.items,
            &Damage::from_rect(box_),
            &self.atlas,
            scale,
            &mut target,
            strand_scene::Rect::from_size(size),
            (x0, y0),
        );
        Some(strand_scene::DragImage {
            size: Size::new(box_.w, box_.h),
            scale,
            origin: LogicalPoint::new(scale.to_logical(x0 as f64), scale.to_logical(y0 as f64)),
            pixels,
        })
    }

    /// (M5, the inspector) The surfaces that show `node`: those attached at the nearest
    /// surface root at or above it (one per output for a bar on several;
    /// a popup's own surface, not its anchor's, for a node inside a
    /// popup). Empty when no attached surface shows it. Logic names
    /// nodes, never surfaces, so main resolves the inspector's highlight
    /// and layout requests through this (decisions.md m5-w0-harden).
    pub fn surfaces_of(&self, node: NodeId) -> Vec<SurfaceId> {
        let mut at = Some(node);
        while let Some(id) = at {
            let shown: Vec<SurfaceId> = self
                .surfaces
                .iter()
                .filter(|(_, s)| s.root == id)
                .map(|(sid, _)| *sid)
                .collect();
            if !shown.is_empty() {
                return shown;
            }
            at = self.tree.get(id).and_then(|n| n.parent);
        }
        Vec::new()
    }

    /// Paints the subtree under `root` (a surface node) into `surface`.
    pub fn attach_surface(&mut self, surface: SurfaceId, root: NodeId) {
        self.surfaces.insert(
            surface,
            SurfaceState {
                root,
                size: Size::default(),
                scale: Scale::ONE,
                prev_scale: None,
                records: BTreeMap::new(),
                history: VecDeque::new(),
                valid: false,
                painted: false,
                wait_until: None,
                wait_paused: Duration::ZERO,
                text_deadline: None,
                awaiting_text: false,
                new_text_until: None,
                painted_at: None,
                dirty: true,
                opaque: Damage::new(),
                time: Duration::ZERO,
                drawn: HashSet::new(),
                cache: None,
                posed: None,
                wanted: Vec::new(),
                boxes: None,
                layout_dirty: true,
                hits: Vec::new(),
                query_hold: None,
                query_held: false,
                size_hold: None,
                size_paused: Duration::ZERO,
                animating: false,
                anim_layout: false,
                flip: HashSet::new(),
                flip_all: false,
                flip_open: None,
                text_arrived: false,
                unpainted: HashSet::new(),
                painted_time: None,
                overhang: self
                    .specs
                    .get(&root)
                    .map(|s| s.overhang)
                    .unwrap_or_default(),
            },
        );
        self.raster.set_surfaces(self.surfaces.len());
        self.refresh_size_holds();
        // A surface attached while a swap springs: its `set { }` scopes
        // were not played through.
        self.check_new_scopes(&[root], Some(surface));
    }

    /// Forces a full repaint of `surface` on its next paint (for example
    /// after a painted buffer could not be committed).
    pub fn invalidate(&mut self, surface: SurfaceId) {
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.valid = false;
            s.dirty = true;
        }
    }

    /// Tells the renderer a surface's buffer size and scale before its first
    /// paint, so text can be shaped ahead of the first frame.
    ///
    /// Until its first paint, a surface whose text is still being shaped
    /// does not ask for a frame (no frame without its text at boot or on
    /// hotplug) for up to the first-frame wait; see
    /// [`Renderer::frame_deadline`].
    pub fn configure_surface(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let wait = self.first_frame_wait;
        self.glide_origin(surface, size, scale);
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.resize(size, scale);
            if !s.painted && s.wait_until.is_none() && size != Size::default() {
                s.wait_until = Some(Instant::now() + wait);
                s.wait_paused = Duration::ZERO;
            }
        }
        self.prune_scales();
        self.update();
    }

    /// A shown content-sized surface is about to grow: where its anchor
    /// moves its origin (centred: half the growth; anchored to the right
    /// or bottom edge: all of it; a popup as its positioner places it,
    /// [`super::specs::resize_share`]), its content is drawn where it was
    /// on screen and glides to its new place, instead of jumping as the
    /// compositor re-centres the buffer.
    pub(super) fn glide_origin(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let Some(s) = self.surfaces.get(&surface) else {
            return;
        };
        if !s.painted || s.scale != scale || s.size == size || s.size == Size::default() {
            return;
        }
        let root = s.root;
        let (old, new) = (s.scale.logical_size(s.size), scale.logical_size(size));
        let Some(spec) = self.specs.get(&root) else {
            return;
        };
        if self.anim.reduced()
            || spec.kind == NodeKind::Bar
            || !self.content_sized.contains_key(&root)
        {
            return;
        }
        let Some((fx, fy)) = self.resize_share_of(root) else {
            return;
        };
        // Growth only: content kept in place in a smaller buffer would be
        // cut off at its edge. A shrink comes once everything settled,
        // and the content is laid out where the smaller box will be
        // before it comes (`RootSize::Placed`, decisions.md popup-shrink).
        // As far as the compositor moves the larger box's edge the other
        // way, in whole pixels (`layout::edge_shift`).
        let grow = |from: f32, to: f32, f: f32| (-crate::layout::edge_shift(from, to, f)).max(0.0);
        let delta = [grow(old.w, new.w, fx), grow(old.h, new.h, fy)];
        if delta.iter().all(|d| d.abs() < 0.5) {
            return;
        }
        let tables = [&self.tree.tokens];
        let curve = Curve::of(
            &TokenScope::new(&tables).transition(&strand_scene::Transition::Default, Prop::X),
        );
        let kids: Vec<NodeId> = self
            .tree
            .get(root)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for k in kids {
            if self.tree.get(k).is_some_and(|n| !n.kind.is_surface()) {
                self.anim.glide(k, delta, curve);
            }
        }
    }

    pub fn detach_surface(&mut self, surface: SurfaceId) {
        if let Some(root) = self.surfaces.remove(&surface).map(|s| s.root)
            && !self.surfaces.values().any(|s| s.root == root)
        {
            self.extras.placed.remove(&root);
            self.resolve_origins();
            // Its morph boxes left the screen with its last frame.
            self.anim.shared_hidden(root);
        }
        self.forget_fade(surface);
        self.extras.images.forget(surface);
        self.reap_exits();
        self.bounds.remove(&surface);
        self.facts_sent.retain(|(s, _), _| *s != surface);
        self.last_damage.remove(&surface);
        #[cfg(feature = "eval-hooks")]
        self.probe.last.remove(&surface);
        self.frame_stats.forget(surface);
        self.inspect.forget(surface);
        self.raster.set_surfaces(self.surfaces.len());
        self.prune_scales();
        self.prune_texts();
    }

    /// The nodes under the logical `point` of `surface` in its last
    /// painted frame, innermost first, ending at the surface's root (just
    /// the root when nothing is there). A node is hit inside its laid-out
    /// rounded box, grown by `hit: grow(n)` and cut by the clip of a
    /// `scroll`, `list` or `clip: true` ancestor; the topmost in paint
    /// order wins (children over their parent, later siblings over
    /// earlier ones). Shadows are not part of a node's shape. Empty for an
    /// unknown surface.
    pub fn hit(&self, surface: SurfaceId, point: LogicalPoint) -> Vec<NodeId> {
        let Some(s) = self.surfaces.get(&surface) else {
            return Vec::new();
        };
        let k = s.scale.as_f64();
        let (x, y) = (point.x as f64 * k, point.y as f64 * k);
        let best = s
            .hits
            .iter()
            .rev()
            .find(|h| h.contains(x, y))
            .map(|h| h.node);
        let mut chain = Vec::new();
        let mut cur = best.unwrap_or(s.root);
        loop {
            chain.push(cur);
            if cur == s.root {
                break;
            }
            match self.tree.get(cur).and_then(|n| n.parent) {
                Some(p) => cur = p,
                None => break,
            }
        }
        chain
    }

    /// (M5, the inspector) `surface`'s root node, scale and the hit
    /// shapes of its last painted frame, in paint order (`crate::inspect`).
    pub(crate) fn painted_hits(
        &self,
        surface: SurfaceId,
    ) -> Option<(NodeId, strand_scene::Scale, &[crate::flatten::HitBox])> {
        let s = self.surfaces.get(&surface)?;
        Some((s.root, s.scale, &s.hits))
    }

    /// (M5, the inspector) Every surface paints again (the outline or
    /// the flash changed); what each frame flattened stays good.
    pub(crate) fn repaint_all(&mut self) {
        for s in self.surfaces.values_mut() {
            s.dirty = true;
        }
    }
}

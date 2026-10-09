//! Surfaces attached to the renderer: attach, configure, detach, and
//! hit testing their last frame.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use strand_scene::Curve;
use strand_scene::{
    Anchor, Damage, LogicalPoint, LogicalRect, NodeId, NodeKind, Prop, Scale, Size, SurfaceId,
    TokenScope,
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
                awaiting_text: false,
                new_text_until: None,
                painted_at: None,
                dirty: true,
                opaque: Damage::new(),
                time: Duration::ZERO,
                drawn: HashSet::new(),
                cache: None,
                wanted: Vec::new(),
                boxes: None,
                layout_dirty: true,
                hits: Vec::new(),
                query_hold: None,
                query_held: false,
                size_hold: None,
                animating: false,
                anim_layout: false,
                flip: HashSet::new(),
                flip_all: false,
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
            }
        }
        self.prune_scales();
        self.update();
    }

    /// A shown content-sized surface is about to grow: where its anchor
    /// moves its origin (centred: half the growth; anchored to the right
    /// or bottom edge: all of it), its content is drawn where it was on
    /// screen and glides to its new place, instead of jumping as the
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
        if self.anim.reduced() || spec.kind == NodeKind::Bar || !self.content_sized.contains(&root)
        {
            return;
        }
        let (fx, fy) = match spec.anchor {
            Anchor::Center => (0.5, 0.5),
            Anchor::Top => (0.5, 0.0),
            Anchor::Bottom => (0.5, 1.0),
            Anchor::Left => (0.0, 0.5),
            Anchor::Right => (1.0, 0.5),
            Anchor::TopLeft => (0.0, 0.0),
            Anchor::TopRight => (1.0, 0.0),
            Anchor::BottomLeft => (0.0, 1.0),
            Anchor::BottomRight => (1.0, 1.0),
        };
        // Growth only: content kept in place in a smaller buffer would be
        // cut off at its edge (a shrink comes once everything settled,
        // and steps; decisions.md, wave3-pixels fixer round 3).
        let delta = [(new.w - old.w).max(0.0) * fx, (new.h - old.h).max(0.0) * fy];
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
        self.surfaces.remove(&surface);
        self.forget_fade(surface);
        self.extras.images.forget(surface);
        self.reap_exits();
        self.bounds.remove(&surface);
        self.facts_sent.retain(|(s, _), _| *s != surface);
        self.last_damage.remove(&surface);
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
}

//! (M5, S-inspect) The inspector's render side (docs/architecture.md,
//! "M5 interfaces"): pick the node under a point, a node's layout, the
//! highlight outline and the repaint flash, on CPU and `GpuPresent`
//! surfaces alike. They are the only inspector UI the shell draws
//! (m5-owner Q9): the outline and the flash are display items appended
//! to a frame's list after flattening (`overlay.rs`), so the CPU raster
//! and the GPU's lowering draw them as they draw everything else.
//!
//! Pick mode is the host's: the [`Router`](crate::Router) passes pointer
//! input to no handler while it picks
//! ([`Router::set_picking`](crate::Router::set_picking)), and
//! [`Renderer::pick_input`] outlines the node under the pointer on
//! whichever surface it is over and answers a left press with the node
//! picked. Lock surfaces are never pickable.

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::input::button;
use strand_scene::{
    ButtonState, Damage, InputEvent, LogicalPoint, LogicalRect, NodeId, NodeKind, SurfaceId,
};

use crate::Renderer;

mod overlay;

/// How long a repaint stays tinted ([`Renderer::set_damage_flash`]).
pub const FLASH: Duration = Duration::from_millis(300);

/// A node's layout as the inspector shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutInfo {
    /// Its box on the surface as the last frame drew it, in logical
    /// pixels: its laid-out border box after paint offsets, scrolling
    /// and any `scale`/`rotate` (their bounding box then), without `hit:
    /// grow(n)` (`None`: not drawn there).
    pub rect: Option<LogicalRect>,
}

/// What the inspector asked render to draw, and what it drew.
#[derive(Debug, Default)]
pub(crate) struct Inspect {
    /// The node `set_highlight` outlines.
    pub(crate) highlight: Option<NodeId>,
    /// Pick mode's node under the pointer, outlined in place of
    /// `highlight` while it is set.
    pub(crate) hover: Option<NodeId>,
    pub(crate) flash: bool,
    /// Per surface, each recent frame's repainted area and its frame
    /// time, while the flash is on (empty and unallocated while it is
    /// off).
    pub(crate) flashes: HashMap<SurfaceId, Vec<(Damage, Duration)>>,
    /// Per surface, what the overlay drew in its last frame: where, and
    /// a signature of it. A frame that draws something else repaints
    /// both places; one that draws the same repaints neither.
    pub(crate) drawn: HashMap<SurfaceId, (Damage, u64)>,
}

impl Inspect {
    /// Nothing asked for and nothing left on screen: a frame skips the
    /// overlay at once.
    pub(crate) fn idle(&self) -> bool {
        self.highlight.is_none() && self.hover.is_none() && !self.flash && self.drawn.is_empty()
    }

    /// `surface` wants frames for the flash: its tints are still fading.
    pub(crate) fn wants(&self, surface: SurfaceId) -> bool {
        self.flash && self.flashes.contains_key(&surface)
    }

    /// `surface` is gone.
    pub(crate) fn forget(&mut self, surface: SurfaceId) {
        self.flashes.remove(&surface);
        self.drawn.remove(&surface);
    }
}

impl Renderer {
    /// The node a pick at `point` on `surface` chooses: the topmost node
    /// of its last painted frame under the point (the innermost of
    /// [`Renderer::hit`]'s chain); none off every shape, on a surface
    /// not painted yet, and on a lock surface (never pickable).
    pub fn pick(&self, surface: SurfaceId, point: LogicalPoint) -> Option<NodeId> {
        let (root, scale, hits) = self.painted_hits(surface)?;
        if self
            .tree()
            .get(root)
            .is_none_or(|n| n.kind == NodeKind::Lock)
        {
            return None;
        }
        let k = scale.as_f64();
        let (x, y) = (point.x as f64 * k, point.y as f64 * k);
        hits.iter().rev().find(|h| h.contains(x, y)).map(|h| h.node)
    }

    /// `node`'s layout on `surface`, as its last painted frame drew it.
    pub fn node_layout(&self, surface: SurfaceId, node: NodeId) -> LayoutInfo {
        let rect = self.painted_hits(surface).and_then(|(_, scale, hits)| {
            let h = hits.iter().rev().find(|h| h.node == node)?;
            let r = overlay::drawn_box(h);
            let r = match h.inverse {
                Some(inv) => inv.inverse().transform_rect_bbox(r),
                None => r,
            };
            let k = scale.as_f64();
            Some(LogicalRect::new(
                (r.x0 / k) as f32,
                (r.y0 / k) as f32,
                (r.width() / k) as f32,
                (r.height() / k) as f32,
            ))
        });
        LayoutInfo { rect }
    }

    /// Outline `node` on every surface that shows it
    /// ([`Renderer::surfaces_of`]), or nothing. Pick mode's node under
    /// the pointer is outlined in its place while picking.
    pub fn set_highlight(&mut self, target: Option<NodeId>) {
        if self.inspect.highlight != target {
            self.inspect.highlight = target;
            self.repaint_all();
        }
    }

    /// The outlined node, as last set.
    pub fn highlight(&self) -> Option<NodeId> {
        self.inspect.highlight
    }

    /// Tint each frame's damage for [`FLASH`] (300 ms), fading, or stop.
    /// A frame's own repaint of the tints and the outline is not tinted
    /// (no feedback); a frame without a presentation time (an offline
    /// paint at time zero) tints nothing. Off means no clock and no
    /// allocation: nothing is kept and no surface wants a frame for it.
    pub fn set_damage_flash(&mut self, on: bool) {
        if self.inspect.flash == on {
            return;
        }
        self.inspect.flash = on;
        if !on {
            // The tints still shown are repainted away.
            self.inspect.flashes = HashMap::new();
            self.repaint_all();
        }
    }

    /// Whether damage flashing is on, as last set.
    pub fn damage_flash(&self) -> bool {
        self.inspect.flash
    }

    /// Pick mode's handling of one input event the router passed to no
    /// handler ([`Router::set_picking`](crate::Router::set_picking)):
    /// the node under the pointer is outlined on whichever surface it is
    /// over ([`Renderer::pick`]), and a left press answers the surface
    /// and the node picked there (none: nothing pickable). Other input
    /// answers nothing.
    pub fn pick_input(&mut self, event: &InputEvent) -> Option<(SurfaceId, Option<NodeId>)> {
        match event {
            InputEvent::PointerEnter { surface, position }
            | InputEvent::PointerMotion {
                surface, position, ..
            } => {
                let node = self.pick(*surface, *position);
                self.set_hover(node);
                None
            }
            InputEvent::PointerLeave { .. } => {
                self.set_hover(None);
                None
            }
            InputEvent::PointerButton {
                surface,
                position,
                button: button::LEFT,
                state: ButtonState::Pressed,
                ..
            } => {
                let node = self.pick(*surface, *position);
                self.set_hover(node);
                Some((*surface, node))
            }
            _ => None,
        }
    }

    /// Pick mode ended: the node under the pointer is no longer outlined
    /// (the highlight, if any, is again).
    pub fn pick_ended(&mut self) {
        self.set_hover(None);
    }

    fn set_hover(&mut self, node: Option<NodeId>) {
        if self.inspect.hover != node {
            self.inspect.hover = node;
            self.repaint_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TextBackend;

    fn renderer() -> Renderer {
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(Vec::new()));
        Renderer::new(TextBackend::Inline(Box::new(engine)))
    }

    /// Before any frame nothing is picked or reported; the highlight and
    /// the flash read back as set.
    #[test]
    fn nothing_is_picked_before_a_frame() {
        let mut r = renderer();
        let (s, n) = (SurfaceId(1), NodeId::new(3, 0));
        assert_eq!(r.pick(s, LogicalPoint::new(1.0, 1.0)), None);
        assert_eq!(r.node_layout(s, n), LayoutInfo::default());
        assert_eq!(r.highlight(), None);
        r.set_highlight(Some(n));
        assert_eq!(r.highlight(), Some(n));
        r.set_highlight(None);
        assert!(!r.damage_flash());
        r.set_damage_flash(true);
        assert!(r.damage_flash());
        assert_eq!(r.frame_summary(s), None);
    }

    /// A node resolves to the surfaces that show it: every output's
    /// surface at a bar's root, a popup's own surface for a node inside
    /// it, and none for a node no attached surface shows.
    #[test]
    fn a_node_resolves_to_the_surfaces_that_show_it() {
        use strand_scene::SceneDiff;
        let mut r = renderer();
        let (bar, item, popup, inner, other) = (
            NodeId::new(0, 0),
            NodeId::new(1, 0),
            NodeId::new(2, 0),
            NodeId::new(3, 0),
            NodeId::new(4, 0),
        );
        let mut d = SceneDiff::new();
        d.create(bar, NodeKind::Bar, None, 0)
            .create(item, NodeKind::Box, Some(bar), 0)
            .create(popup, NodeKind::Popup, Some(item), 0)
            .create(inner, NodeKind::Box, Some(popup), 0)
            .create(other, NodeKind::Panel, None, 1);
        assert!(r.apply(d).is_empty());
        assert_eq!(r.surfaces_of(item), Vec::<SurfaceId>::new());
        r.attach_surface(SurfaceId(1), bar);
        r.attach_surface(SurfaceId(2), bar);
        r.attach_surface(SurfaceId(3), popup);
        assert_eq!(r.surfaces_of(bar), vec![SurfaceId(1), SurfaceId(2)]);
        assert_eq!(r.surfaces_of(item), vec![SurfaceId(1), SurfaceId(2)]);
        assert_eq!(r.surfaces_of(inner), vec![SurfaceId(3)]);
        assert_eq!(r.surfaces_of(other), Vec::<SurfaceId>::new());
        assert_eq!(r.surfaces_of(NodeId::new(9, 0)), Vec::<SurfaceId>::new());
    }
}

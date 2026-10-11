//! (M5, S-inspect) The inspector's render side (docs/architecture.md,
//! "M5 interfaces"): pick a node under a point, a node's layout, the
//! highlight outline and damage flashing, on CPU and `GpuPresent`
//! surfaces alike. Inert until S-inspect lands it: nothing is picked,
//! no layout is reported, and the highlight and flash are only kept.

use strand_scene::{LogicalPoint, LogicalRect, NodeId, SurfaceId};

use crate::Renderer;

/// A node's layout as the inspector shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutInfo {
    /// Its box on the surface, in logical pixels (`None`: not laid out
    /// there).
    pub rect: Option<LogicalRect>,
}

/// What the inspector asked render to draw.
#[derive(Debug, Default)]
pub(crate) struct Inspect {
    pub(crate) highlight: Option<NodeId>,
    pub(crate) flash: bool,
}

impl Renderer {
    /// The node a pick at `point` on `surface` chooses (none yet).
    pub fn pick(&self, surface: SurfaceId, point: LogicalPoint) -> Option<NodeId> {
        let _ = (surface, point);
        None
    }

    /// `node`'s layout on `surface` (not reported yet).
    pub fn node_layout(&self, surface: SurfaceId, node: NodeId) -> LayoutInfo {
        let _ = (surface, node);
        LayoutInfo::default()
    }

    /// Outline `node` on every surface that shows it
    /// ([`Renderer::surfaces_of`]), or nothing (not drawn yet).
    pub fn set_highlight(&mut self, target: Option<NodeId>) {
        self.inspect.highlight = target;
    }

    /// The outlined node, as last set.
    pub fn highlight(&self) -> Option<NodeId> {
        self.inspect.highlight
    }

    /// Tint each frame's damage for 300 ms, or stop (not drawn yet; off
    /// means no clock and no allocation).
    pub fn set_damage_flash(&mut self, on: bool) {
        self.inspect.flash = on;
    }

    /// Whether damage flashing is on, as last set.
    pub fn damage_flash(&self) -> bool {
        self.inspect.flash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TextBackend;

    /// (0b) Inert: nothing is picked or reported, and setting the
    /// highlight or the flash changes nothing but what reads them back.
    #[test]
    fn the_inspector_stubs_are_inert() {
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(Vec::new()));
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
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
        use strand_scene::{NodeKind, SceneDiff};
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(Vec::new()));
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
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

//! Scrolling `scroll` and `list` nodes.

use strand_scene::{LogicalPoint, NodeId, NodeKind, Prop, PropValue, SurfaceId};

use super::Renderer;

impl Renderer {
    /// Scrolls the innermost `scroll` or `list` under `point` of
    /// `surface` (in its last frame) that can move by `dy` logical
    /// pixels: one already at its end passes the scroll outward. Returns
    /// the node scrolled, if any moved.
    pub fn scroll(&mut self, surface: SurfaceId, point: LogicalPoint, dy: f32) -> Option<NodeId> {
        let chain = self.hit(surface, point);
        // The innermost that can still move that way: one at its end
        // hands the wheel to the one around it.
        let id = chain.into_iter().find(|n| {
            self.tree
                .get(*n)
                .is_some_and(|n| matches!(n.kind, NodeKind::Scroll | NodeKind::List))
                && self.scrolls.entry(*n).or_default().scroll_by(dy)
        })?;
        let root = self.tree.root_of(id);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_layout();
            }
        }
        Some(id)
    }

    /// Scrolls list or scroll `id` so that its child `row` is in view.
    pub fn scroll_into_view(&mut self, id: NodeId, row: NodeId) {
        let Some(node) = self.tree.get(id) else {
            return;
        };
        let st = self.scrolls.entry(id).or_default();
        let est = if st.heights.is_empty() {
            crate::layout::LIST_ROW_ESTIMATE
        } else {
            st.heights.values().sum::<f32>() / st.heights.len() as f32
        };
        let gap = node
            .get(Prop::Gap)
            .and_then(PropValue::as_number)
            .unwrap_or(0.0)
            .max(0.0);
        let mut y = 0.0;
        for c in &node.children {
            let h = st.heights.get(c).copied().unwrap_or(est);
            if *c == row {
                let before = st.offset;
                if y < st.offset {
                    st.offset = y;
                } else if y + h > st.offset + st.viewport {
                    st.offset = y + h - st.viewport;
                }
                if st.offset != before {
                    let root = self.tree.root_of(id);
                    for s in self.surfaces.values_mut() {
                        if Some(s.root) == root {
                            s.mark_layout();
                        }
                    }
                }
                return;
            }
            y += h + gap;
        }
    }
}

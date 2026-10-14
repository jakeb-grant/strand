//! Layout facts for logic: the laid-out boxes of the nodes logic reads
//! them of (`Prop::Watch`), what `self.width`, container queries and
//! (M6) `id.box` read (spec.md 6.3).
//!
//! A fact is a node's box in the coordinates of its surface's box: from
//! that box's top-left corner (the buffer's shadow overhang lies outside
//! it), in logical pixels, as laid out (paint offsets, transforms and
//! poses not applied). A surface node's own box is where its box sits in
//! its parent surface's coordinates (a popup's, from where the
//! compositor placed both), or at 0, 0 for a surface with no parent (a
//! bar, panel, OSD or lock is the frame its content is measured in).
//! Facts go out only when a box changes: an idle shell sends none.

use strand_scene::{LogicalRect, NodeId, PropValue, SurfaceId};

use super::Renderer;
use strand_scene::Prop;

/// What logic reads of a watched node: its box (`id.box`) or only its
/// size; and whether a container query reads it (its answer holds a
/// frame).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Watch {
    pub(super) boxed: bool,
    pub(super) query: bool,
}

impl Watch {
    /// From a node's `Prop::Watch` (`size`, `query`, `box`, `box_query`).
    pub(super) fn of(v: &PropValue) -> Watch {
        match v {
            PropValue::Keyword(k) => Watch {
                boxed: k.starts_with("box"),
                query: k == "query" || k == "box_query",
            },
            _ => Watch::default(),
        }
    }

    /// Whether `now` differs from `sent` in what is read.
    fn changed(self, sent: Option<&LogicalRect>, now: &LogicalRect) -> bool {
        match sent {
            None => true,
            Some(s) if self.boxed => s != now,
            Some(s) => (s.w, s.h) != (now.w, now.h),
        }
    }
}

impl Renderer {
    /// Node `node`'s box on surface `id` as a fact (see the module doc),
    /// if it is laid out there.
    pub(super) fn fact_box(&self, id: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        let s = self.surfaces.get(&id)?;
        let boxes = s.boxes.as_ref()?;
        let r = *boxes.rects.get(&node)?;
        let root = boxes.rects.get(&s.root).copied().unwrap_or(r);
        if node != s.root {
            return Some(LogicalRect::new(r.x - root.x, r.y - root.y, r.w, r.h));
        }
        Some(match self.placed_in_parent(s.root) {
            Some((x, y)) => LogicalRect::new(x, y, r.w, r.h),
            None => LogicalRect::new(0.0, 0.0, r.w, r.h),
        })
    }

    /// Where surface node `root`'s box sits in its parent surface's box,
    /// from where the surface manager placed both (`None`: no parent
    /// surface, or not placed yet).
    fn placed_in_parent(&self, root: NodeId) -> Option<(f32, f32)> {
        let parent = self.tree.get(root)?.parent?;
        let parent = self.tree.root_of(parent)?;
        let (_, at) = self.extras.placed.get(&root)?;
        let (_, pat) = self.extras.placed.get(&parent)?;
        let o = self
            .specs
            .get(&root)
            .map(|s| s.overhang)
            .unwrap_or_default();
        let po = self
            .specs
            .get(&parent)
            .map(|s| s.overhang)
            .unwrap_or_default();
        Some((
            (at.x + o.left) - (pat.x + po.left),
            (at.y + o.top) - (pat.y + po.top),
        ))
    }

    /// Hands the boxes of watched nodes on surface `id` that changed to
    /// logic, holding the frame for a container query's answer, and for
    /// the first box of a node `id.box` reads (so a binding on it shows
    /// its value from the first frame, not a frame later).
    pub(super) fn report_facts(&mut self, id: SurfaceId) {
        let Some(boxes) = self.surfaces.get(&id).and_then(|s| s.boxes.as_ref()) else {
            return;
        };
        let mut hold = false;
        let mut facts = Vec::new();
        for node in boxes.rects.keys() {
            if self.tree.is_ghost(*node) {
                continue;
            }
            let Some(watch) = self.tree.get(*node).and_then(|n| n.get(Prop::Watch)) else {
                continue;
            };
            let watch = Watch::of(watch);
            let Some(rect) = self.fact_box(id, *node) else {
                continue;
            };
            let sent = self.facts_sent.get(&(id, *node));
            if watch.changed(sent, &rect) {
                hold |= watch.query || (watch.boxed && sent.is_none());
                facts.push((*node, rect));
            }
        }
        self.send_facts(id, facts, hold);
    }

    /// Newly watched nodes: their box as laid out now, if it is (on the
    /// first surface showing it). A read that starts holds that
    /// surface's next frame for the answer, as a change would.
    pub(super) fn report_watched(&mut self, watched: Vec<NodeId>) {
        for node in watched {
            self.facts_sent.retain(|(_, n), _| *n != node);
            let Some(watch) = self.tree.get(node).and_then(|n| n.get(Prop::Watch)) else {
                continue;
            };
            let watch = Watch::of(watch);
            let mut ids: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
            ids.sort();
            let mut first = true;
            for sid in ids {
                let Some(rect) = self.fact_box(sid, node) else {
                    continue;
                };
                if first {
                    first = false;
                    self.send_facts(sid, vec![(node, rect)], watch.query || watch.boxed);
                } else {
                    self.facts_sent.insert((sid, node), rect);
                }
            }
        }
    }

    /// (M6) A surface was placed or moved: the boxes of watched surface
    /// nodes (a popup's place in its parent) that changed go to logic.
    pub(super) fn report_placed(&mut self) {
        let ids: Vec<(SurfaceId, NodeId)> = self
            .surfaces
            .iter()
            .map(|(sid, s)| (*sid, s.root))
            .collect();
        for (sid, root) in ids {
            let Some(watch) = self.tree.get(root).and_then(|n| n.get(Prop::Watch)) else {
                continue;
            };
            let watch = Watch::of(watch);
            if !watch.boxed {
                continue;
            }
            let Some(rect) = self.fact_box(sid, root) else {
                continue;
            };
            if watch.changed(self.facts_sent.get(&(sid, root)), &rect) {
                self.send_facts(sid, vec![(root, rect)], false);
            }
        }
    }

    fn send_facts(&mut self, id: SurfaceId, facts: Vec<(NodeId, LogicalRect)>, hold: bool) {
        if facts.is_empty() {
            return;
        }
        for (node, rect) in facts {
            self.facts_sent.insert((id, node), rect);
            self.facts.push((node, rect));
        }
        let (wait, window) = (self.query_wait, self.busy_window);
        let seq = self.facts_seq + 1;
        if let Some(s) = self.surfaces.get_mut(&id)
            && hold
        {
            s.hold_for_query(seq, wait, window);
        }
    }
}

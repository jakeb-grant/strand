//! Applying logic's scene diffs, and the motions each op starts.

use std::time::Instant;

use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneDiff, SceneOp, TokenScope};

use super::{MAX_GHOSTS_PER_PARENT, Renderer};
use crate::anim::{ExitKind, exit_pose, is_pose};
use crate::flatten::scope_tables;
use crate::tree::SceneError;

/// Layout lengths that snap to a new value while the boxes they move
/// glide there (design.md, snap rules); `width`, `height` and `size`
/// spring instead.
pub(super) fn snaps_and_glides(prop: Prop) -> bool {
    use Prop::*;
    matches!(
        prop,
        MinWidth
            | MaxWidth
            | MinHeight
            | MaxHeight
            | Pad
            | Margin
            | Gap
            | Grow
            | Shrink
            | Align
            | Justify
            | Place
            | Columns
    )
}

/// True for props layout reads: a change to one relayouts its surface;
/// any other prop (colours, `x`/`y`, opacity, shadows) only repaints.
pub fn affects_layout(prop: Prop) -> bool {
    use Prop::*;
    matches!(
        prop,
        Width
            | Height
            | Size
            | MinWidth
            | MaxWidth
            | MinHeight
            | MaxHeight
            | Pad
            | Margin
            | Gap
            | Grow
            | Shrink
            | Align
            | Justify
            | Place
            | Columns
            | Edge
            | Text
            | Font
            | Weight
            | Ellipsis
            | MaxLines
            | Markup
            | Marks
            | Tokens
    )
}

impl Renderer {
    /// Applies one tick's diff. Failed ops are returned; the rest apply.
    pub fn apply(&mut self, diff: SceneDiff) -> Vec<SceneError> {
        // A `tooltip` (or the tokens it reads) changing while one waits
        // or shows: its text follows.
        let tooltips = diff.ops.iter().any(|op| {
            matches!(
                op,
                SceneOp::SetProp {
                    prop: Prop::Tooltip,
                    ..
                } | SceneOp::SetTokens { .. }
            )
        });
        let errors = self.apply_ops(diff);
        // Widget state of nodes logic removed goes with them, and a
        // tooltip with its node.
        let tree = &self.tree;
        self.extras.widgets.retain(|n| tree.contains_live(n));
        if self
            .tooltip
            .as_ref()
            .is_some_and(|t| !self.tree.contains_live(t.target))
        {
            self.hide_tooltip();
        }
        if tooltips && !self.extras.widgets.hovered.is_empty() {
            self.refresh_tooltip();
        }
        // An exit this diff started is woken for even if its output
        // sends no frame.
        self.arm_timer();
        errors
    }

    pub(super) fn apply_ops(&mut self, diff: SceneDiff) -> Vec<SceneError> {
        let mut errors = Vec::new();
        // Surface roots whose subtree an op touches; `None` means all.
        // `relayout`: those whose layout it may change (anything but a
        // paint-only prop).
        let mut touched: Option<Vec<NodeId>> = Some(Vec::new());
        let mut relayout: Option<Vec<NodeId>> = Some(Vec::new());
        if let Some(on) = diff.reduced_motion {
            self.set_reduced_motion(on);
        }
        // Logic took in the facts a held frame waits for: it answered.
        if let Some(seen) = diff.layout_seen {
            for s in self.surfaces.values_mut() {
                if s.query_hold.is_some_and(|(seq, _)| seq <= seen) {
                    s.query_hold = None;
                }
            }
        }
        // Surfaces this diff closes: content it removes from them may be
        // kept for their closing pose (see `closing_with_pose`).
        self.closing_now = diff
            .ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::Open,
                    value: PropValue::Bool(false),
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        // A theme swap is planned against the table and frames on screen.
        let swap = self.plan_swap(&diff);
        let mut watched = Vec::new();
        // Nodes whose `set { }` scopes may be new to a swap in flight.
        let mut scoped = Vec::new();
        for op in diff.ops {
            match &op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::Watch,
                    ..
                } => watched.push(*id),
                SceneOp::SetProp {
                    id,
                    prop: Prop::Tokens,
                    ..
                }
                | SceneOp::Move { id, .. } => scoped.push(*id),
                _ => {}
            }
            // Where the node painted before the op, and the node whose
            // root to look up after it.
            let (before, after, shapes) = match &op {
                SceneOp::Create { id, .. } => (None, Some(*id), true),
                SceneOp::Remove { id, .. } => (self.tree.root_of(*id), None, true),
                SceneOp::SetProp { id, prop, .. } => {
                    (self.tree.root_of(*id), None, affects_layout(*prop))
                }
                SceneOp::Move { id, .. } => (self.tree.root_of(*id), Some(*id), true),
                SceneOp::SetTokens { .. } => {
                    touched = None;
                    relayout = None;
                    (None, None, true)
                }
            };
            // `open` sizes a content-sized surface when it opens.
            let open = matches!(
                &op,
                SceneOp::SetProp {
                    prop: Prop::Open,
                    ..
                }
            );
            // A shadow, or the coordinates of a `place: absolute` node
            // casting one, changes only the overhang a surface asks for
            // (from its layout pass, so it lays out again). A flow node's
            // paint offset never does: the overhang is taken at rest, so
            // an enter or exit animation never resizes the buffer.
            let shadow = match &op {
                SceneOp::SetProp {
                    prop: Prop::Shadow, ..
                } => true,
                SceneOp::SetProp {
                    id,
                    prop: Prop::X | Prop::Y,
                    ..
                } => self.tree.get(*id).is_some_and(|n| {
                    n.get(Prop::Shadow).is_some()
                        && matches!(n.get(Prop::Place), Some(PropValue::Keyword(k)) if k == "absolute")
                }),
                _ => false,
            };
            let result = if self.animate_op(&op) {
                Ok(())
            } else {
                self.tree.apply_op(op)
            };
            match result {
                Ok(()) => {
                    let roots: Vec<NodeId> = before
                        .into_iter()
                        .chain(after.and_then(|id| self.tree.root_of(id)))
                        .collect();
                    if let Some(t) = &mut touched {
                        t.extend(roots.iter().copied());
                    }
                    if (shapes || shadow)
                        && let Some(t) = &mut relayout
                    {
                        t.extend(roots.iter().copied());
                    }
                    if shapes || shadow || open {
                        self.spec_dirty.extend(roots);
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        if let Some(plan) = swap {
            self.install_swap(plan);
        }
        // Scopes the swap's plan never saw (created or changed by this
        // diff), while its roots spring.
        self.check_new_scopes(&scoped, None);
        if relayout.is_none() {
            self.spec_dirty.extend(self.tree.surface_nodes());
            self.refresh_reduced();
        }
        let tree = &self.tree;
        self.anim.retain(|id| tree.contains(id));
        self.extras.rasters.retain(|id| tree.contains(id));
        self.closed.retain(|id| tree.contains(*id));
        self.reap_exits();
        // Drop text state of nodes that are gone or no longer show text.
        let tree = &self.tree;
        let text = &self.text;
        let before = self.texts.len();
        self.texts.retain(|slot, t| {
            let keep = tree.get(slot.node).is_some_and(|n| match n.kind {
                NodeKind::Text | NodeKind::Button => n.get(Prop::Text).is_some(),
                NodeKind::Input => {
                    n.get(Prop::Text).is_some() || n.get(Prop::Placeholder).is_some()
                }
                _ => false,
            });
            if !keep && let Some((k, _)) = t.requested.take() {
                text.cancel(k);
            }
            keep
        });
        if self.texts.len() != before {
            // Dropped layouts may have freed atlas pages.
            self.refresh_retries();
        }
        let texts = &self.texts;
        self.pending.retain(|_, slot| texts.contains_key(slot));
        let tree = &self.tree;
        self.facts_sent.retain(|(_, n), _| tree.contains(*n));
        // Newly watched nodes: their size as laid out now, if it is (on
        // the first surface showing it). A query that starts reading one
        // holds that surface's next frame for the answer, as a size
        // change would.
        let (wait, window) = (self.query_wait, self.busy_window);
        for id in watched {
            self.facts_sent.retain(|(_, n), _| *n != id);
            let query = match self.tree.get(id).and_then(|n| n.get(Prop::Watch)) {
                None => continue,
                Some(w) => matches!(w, PropValue::Keyword(k) if k == "query"),
            };
            let seq = self.facts_seq + 1;
            let mut first = true;
            for (sid, s) in self.surfaces.iter_mut() {
                let Some(r) = s.boxes.as_ref().and_then(|b| b.rects.get(&id)).copied() else {
                    continue;
                };
                self.facts_sent.insert((*sid, id), (r.w, r.h));
                if !first {
                    continue;
                }
                first = false;
                if query {
                    s.hold_for_query(seq, wait, window);
                }
                self.facts.push((id, r.w, r.h));
            }
        }
        self.scrolls.retain(|n, _| tree.contains(*n));
        // A surface nested in a touched one (a popup in a bar) inherits
        // from it, so it is touched too; `update` clears it again if
        // nothing it draws changed.
        // A surface whose root went with an ancestor (a popup in a removed
        // bar) is touched too: its stale frame clears until it is
        // detached.
        let tree = &self.tree;
        for s in self.surfaces.values_mut() {
            let hit = |t: &Option<Vec<NodeId>>| {
                t.as_ref()
                    .is_none_or(|t| t.iter().any(|r| tree.is_ancestor(*r, s.root)))
            };
            if !tree.contains(s.root) || hit(&relayout) {
                s.mark_layout();
            } else if hit(&touched) {
                s.mark_dirty();
            }
        }
        // `update` refreshes the specs (after collecting delivered text,
        // so nothing asked for here can already be answered).
        self.update();
        self.born.clear();
        self.closing_now.clear();
        errors
    }

    /// What an op starts moving, before it applies: logic's new value of
    /// an animatable prop springs from the old one, a node created on a
    /// shown surface enters, a removed one with an exit pose becomes a
    /// ghost (returns true: the removal is done), and structural changes
    /// glide their siblings. Nothing animates on a surface not shown
    /// with a clock, or under `reduced_motion`.
    pub(super) fn animate_op(&mut self, op: &SceneOp) -> bool {
        let reduced = self.anim.reduced();
        match op {
            // (M4) S-lists makes a list window's rows (`window: true`)
            // play no enter, exit or FLIP; until then they animate like
            // any node.
            SceneOp::Create {
                id,
                parent,
                kind: _,
                index: _,
                window: _,
            } => {
                // Logic reused the id of a ghost: the tree unmounts the
                // ghost, and its motions must not carry over.
                if self.tree.is_ghost(*id) {
                    let old = self.tree.get(*id).and_then(|n| n.parent);
                    self.anim.forget(*id);
                    self.flip(old);
                }
                let root = parent.and_then(|p| self.tree.root_of(p));
                if parent.is_some() && !reduced {
                    // On screen, or opening (its first frame still to
                    // come): it enters.
                    let opening = root.is_some_and(|r| self.opening.contains(&r));
                    if opening || self.shown(root) {
                        self.anim.enter(*id);
                    } else {
                        self.born.push(*id);
                    }
                }
                self.flip(*parent);
            }
            SceneOp::Remove { id, window: _ } => {
                let Some(node) = self.tree.get(*id).filter(|_| self.tree.contains_live(*id)) else {
                    return false;
                };
                let parent = node.parent;
                let surface = node.kind.is_surface();
                let root = self.tree.root_of(*id);
                let laid = self.surfaces.values().any(|s| {
                    Some(s.root) == root
                        && s.boxes.as_ref().is_some_and(|b| b.rects.contains_key(id))
                });
                if !node.kind.is_surface()
                    && !reduced
                    && is_pose(exit_pose(node))
                    && laid
                    && self.shown(root)
                    && self.tree.ghost(*id).is_ok()
                {
                    // At most a few ghosts per parent: rows removed faster
                    // than they leave (or with no frames to leave in) end
                    // the oldest exit.
                    if let Some(p) = parent.and_then(|p| self.tree.get(p)) {
                        let mut ghosts: Vec<(Instant, NodeId)> = p
                            .children
                            .iter()
                            .filter(|c| self.anim.exiting(**c) == Some(ExitKind::Ghost))
                            .filter_map(|c| Some((self.anim.exit_started(*c)?, *c)))
                            .collect();
                        ghosts.sort();
                        let over = (ghosts.len() + 1).saturating_sub(MAX_GHOSTS_PER_PARENT);
                        for (_, g) in ghosts.into_iter().take(over) {
                            self.anim.finish_now(g);
                        }
                    }
                    self.anim.exit(*id, ExitKind::Ghost);
                    return true;
                }
                // Content removed as its surface closes with a pose (a
                // popup's, unmounted on close): drawn at rest until the
                // pose ends, so the surface does not leave empty.
                if let Some(r) = root
                    && !surface
                    && !reduced
                    && self.closing_with_pose(r)
                    && self.tree.ghost(*id).is_ok()
                {
                    self.closing_content.entry(r).or_default().push(*id);
                    return true;
                }
                self.flip(parent);
            }
            SceneOp::Move { id, parent, .. } => {
                let old = self.tree.get(*id).and_then(|n| n.parent);
                self.flip(old);
                self.flip(*parent);
            }
            SceneOp::SetProp { id, prop, .. } => {
                if !self.tree.contains_live(*id) {
                    return false;
                }
                let root = self.tree.root_of(*id);
                let animates = !reduced && self.shown(root);
                if animates && crate::anim::ANIMATED.contains(prop) {
                    let tables = scope_tables(&self.tree, *id);
                    let scope = TokenScope::new(&tables);
                    let old = self
                        .tree
                        .get(*id)
                        .and_then(|n| n.get(*prop))
                        .and_then(|v| scope.resolve(v))
                        .map(std::borrow::Cow::into_owned);
                    self.anim.touch(*id, *prop, old);
                }
                if animates && crate::anim::is_size(*prop) {
                    self.anim.touch_size(*id);
                }
                // Layout lengths snap; the boxes they move glide.
                if snaps_and_glides(*prop) {
                    let parent = self.tree.get(*id).and_then(|n| n.parent);
                    self.flip(Some(*id));
                    self.flip(parent);
                }
            }
            SceneOp::SetTokens { .. } => {
                for s in self.surfaces.values_mut() {
                    s.flip_all = true;
                }
            }
        }
        false
    }
}

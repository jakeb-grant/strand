//! (lock-int) Strand's own surfaces settle before a lock's stills are
//! taken (design.md, "Lock screen"; docs/architecture.md, "Session
//! lock"): a lock's `thumbnail screen` shows the desktop at rest, never
//! a panel caught leaving or a fade halfway.
//!
//! [`Renderer::settle_for_lock`] ends every running `exit` outside the
//! lock (a closing panel closes now, its surface going with the next
//! surface changes; a removed row unmounts) and gives each other surface
//! with a motion toward an end one settled frame: its next painted frame
//! snaps every spring, glide, size, pose, finite `play`, morph, roll,
//! mask and jelly to its end, as `reduced_motion` does, while what
//! follows time (clocks, loops, a caret's blink, a graph, media) runs on
//! (`Animator::set_settle`). A lock's own surfaces, and everything under
//! a lock, are never touched. With nothing moving it does nothing, and
//! the lock is asked for at once.

use std::collections::BTreeSet;

use strand_scene::{NodeId, NodeKind, SurfaceId};

use super::Renderer;
use crate::anim::ExitKind;

/// What [`Renderer::settle_for_lock`] settled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockSettle {
    /// Exits ended: removed nodes unmounted, closing surfaces closed.
    pub exits: usize,
    /// The surface nodes among them that closed: their surfaces go.
    pub closed: Vec<NodeId>,
    /// Surfaces that owe a settled frame.
    pub settled: Vec<SurfaceId>,
    /// Surfaces of the nodes that closed, which go.
    pub going: Vec<SurfaceId>,
}

impl LockSettle {
    /// Nothing moved: nothing to wait for.
    pub fn is_empty(&self) -> bool {
        self.exits == 0 && self.settled.is_empty() && self.going.is_empty()
    }

    /// Every surface the lock's stills wait for: those that owe a
    /// settled frame and those that go.
    pub fn surfaces(&self) -> Vec<SurfaceId> {
        let mut out = self.settled.clone();
        out.extend(self.going.iter().copied());
        out
    }
}

impl Renderer {
    /// (lock-int) A lock is about to take its stills: Strand's own
    /// surfaces settle (module docs). Every running `exit` not under a
    /// lock ends now (a surface it closes reports its spec closed in the
    /// surface changes, [`Renderer::take_surface_changes`]); every other
    /// surface not under a lock with a motion toward an end
    /// (`Animator::settles`) is marked to draw one settled frame, its
    /// next painted one. Returns what it did; empty when nothing moved.
    pub fn settle_for_lock(&mut self) -> LockSettle {
        let mut out = LockSettle::default();
        let roots: Vec<(SurfaceId, NodeId)> =
            self.surfaces.iter().map(|(id, s)| (*id, s.root)).collect();
        let moving: Vec<(SurfaceId, NodeId)> = roots
            .iter()
            .copied()
            .filter(|(_, root)| !self.under_lock(*root) && self.anim.settles(&self.tree, *root))
            .collect();
        let exits: Vec<(NodeId, ExitKind)> = self
            .anim
            .exits()
            .filter(|(id, _)| self.tree.root_of(*id).is_some_and(|r| !self.under_lock(r)))
            .collect();
        for (id, kind) in &exits {
            self.anim.finish_now(*id);
            if *kind == ExitKind::Close {
                out.closed.push(*id);
            }
        }
        out.exits = exits.len();
        if !exits.is_empty() {
            // Unmounted and closed now: a closing surface's spec reports
            // it closed, and the manager destroys it with the changes.
            self.process_finished();
            self.refresh_specs();
        }
        let closed: BTreeSet<NodeId> = out.closed.iter().copied().collect();
        for (id, root) in moving {
            if closed.contains(&root) {
                continue;
            }
            if let Some(s) = self.surfaces.get_mut(&id) {
                // Sizes in flight snap only in a layout pass.
                s.mark_layout();
            }
            self.lock_settle.insert(id);
            out.settled.push(id);
        }
        out.going = roots
            .iter()
            .filter(|(_, root)| closed.contains(root))
            .map(|(id, _)| *id)
            .collect();
        out
    }

    /// (lock-int) Whether `surface` still owes what the lock's stills
    /// wait for since [`Renderer::settle_for_lock`]: its settled frame
    /// is not painted yet, or it shows a node that closed and has not
    /// gone. False for a surface render does not know.
    pub fn lock_settle_owed(&self, surface: SurfaceId) -> bool {
        self.lock_settle.contains(&surface)
            || self.surfaces.get(&surface).is_some_and(|s| {
                self.specs
                    .get(&s.root)
                    .is_some_and(|spec| !spec.open && spec.kind != NodeKind::Lock)
            })
    }

    /// (lock-int) `surface` paints a frame: whether it is the settled one
    /// [`Renderer::settle_for_lock`] marked it for (then no longer owed).
    pub(super) fn take_lock_settle(&mut self, surface: SurfaceId) -> bool {
        self.lock_settle.remove(&surface)
    }

    /// (lock-int) Surface node `root` is a lock or nested in one (a
    /// popup of the lock's content).
    fn under_lock(&self, root: NodeId) -> bool {
        let mut at = Some(root);
        while let Some(id) = at {
            let Some(n) = self.tree.get(id) else {
                return false;
            };
            if n.kind == NodeKind::Lock {
                return true;
            }
            at = n.parent;
        }
        false
    }
}

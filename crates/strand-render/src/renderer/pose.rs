//! Exit poses: ghosts of removed nodes and surfaces playing their
//! closing pose, until they finish or stall; and (M4) the surface poses
//! the compositor applies ([`crate::pose`]).

use std::collections::HashSet;
use std::time::Duration;
use strand_scene::time::Instant;

use strand_scene::{NodeId, SurfaceId};

use super::Renderer;
use crate::anim::{ExitKind, exit_pose, is_pose};

impl Renderer {
    /// (M4) The compositor applies surface poses (it offers
    /// `wp_alpha_modifier_v1` and the viewporter,
    /// `CompositorCaps::delegates_poses`): a surface root's opacity, scale
    /// and offset are reported through [`Renderer::delegated_pose`]
    /// (`Painter::surface_pose`) where its placement allows, and its
    /// content paints at rest.
    pub fn set_compositor_poses(&mut self, on: bool) {
        if self.extras.compositor_poses != on {
            self.extras.compositor_poses = on;
            for s in self.surfaces.values_mut() {
                s.mark_dirty();
            }
        }
    }

    /// (M4) Whether a delegated pose may scale (on by default): off for
    /// a compositor that draws a layer surface stretched to the box it
    /// arranged for its requested size whatever its viewport's
    /// destination (Hyprland 0.56), where a root's scale is painted while
    /// its fade and offset are still delegated.
    pub fn set_compositor_pose_scale(&mut self, on: bool) {
        if self.extras.compositor_pose_scale_off == on {
            self.extras.compositor_pose_scale_off = !on;
            for s in self.surfaces.values_mut() {
                s.mark_dirty();
            }
        }
    }

    /// (m4-surface-w2) Whether a delegated pose may move the surface by
    /// its margins (on by default): off for a compositor that arranges
    /// every margin change of a layer surface with its own animation
    /// (Hyprland), where a root's `x`/`y` (and so its scale) are painted
    /// and only its fade is delegated. The specs refresh with it: a panel
    /// or OSD keeps room in its buffer for its own enter and exit moves
    /// while they are painted ([`Renderer::reserve_reach`]).
    pub fn set_compositor_pose_offset(&mut self, on: bool) {
        if self.extras.compositor_pose_offset_off == on {
            self.extras.compositor_pose_offset_off = !on;
            for s in self.surfaces.values_mut() {
                s.mark_dirty();
            }
            self.spec_dirty.extend(self.specs.keys().copied());
            self.refresh_specs();
        }
    }

    /// (M4) The pose the compositor should apply to `surface` with its
    /// next commit: the one its last painted frame took out of the root
    /// ([`crate::pose::delegate`]), even when what ended that frame
    /// (a closing pose finishing) has the next frame flatten anew;
    /// `None` when nothing is delegated.
    pub fn delegated_pose(&self, surface: SurfaceId) -> Option<strand_scene::SurfacePose> {
        // (M4) A surface the GPU thread presents paints its poses.
        #[cfg(feature = "gpu")]
        if self.backend(surface) == strand_scene::Backend::GpuPresent {
            return None;
        }
        let s = self.surfaces.get(&surface)?;
        if !self.extras.compositor_poses {
            return None;
        }
        s.posed
    }

    /// Ghosts under surface node `root` still play their exit.
    pub(super) fn ghosts_under(&self, root: NodeId) -> bool {
        self.anim
            .exits()
            .any(|(id, k)| k == ExitKind::Ghost && self.tree.root_of(id) == Some(root))
    }

    /// True if surface node `root` is shown and closes with a pose: its
    /// closing pose plays now, or the diff being applied closes it and
    /// it has one (decided in `update`, after the diff's removals).
    pub(super) fn closing_with_pose(&self, root: NodeId) -> bool {
        let exiting = self.anim.exiting(root) == Some(ExitKind::Close);
        let closes = self.closing_now.contains(&root)
            && self.specs.get(&root).is_some_and(|s| s.open)
            && !self.closed.contains(&root)
            && self.tree.get(root).is_some_and(|n| is_pose(exit_pose(n)));
        (exiting || closes) && self.shown(Some(root))
    }

    /// (lock-polish) True if lock node `root` is shown and leaving: the
    /// diff being applied closes it, or its exit plays. Its content
    /// leaves with it (logic unmounts a closed lock's content, as a
    /// popup's): what has an `exit` pose plays it, and the rest stays
    /// drawn as it was until those end, so the lock plays its way out as
    /// one screen and the session unlocks once it has (design.md, "Lock
    /// screen").
    pub(super) fn lock_closing(&self, root: NodeId) -> bool {
        let Some(spec) = self.specs.get(&root) else {
            return false;
        };
        let leaving = spec.closing
            || (self.closing_now.contains(&root) && spec.open && !self.closed.contains(&root));
        spec.kind == strand_scene::NodeKind::Lock && leaving && self.shown(Some(root))
    }

    /// (lock-int) `surface` shows a lock that settled on its exit's end
    /// (`Renderer::lock_settled`) and has drawn that frame: it paints
    /// nothing more and wants no frame (a new size, which needs one, is
    /// drawn) until the lock opens again or the surface goes.
    pub(super) fn lock_frozen(&self, surface: SurfaceId) -> bool {
        self.surfaces.get(&surface).is_some_and(|s| {
            s.valid
                && self
                    .lock_settled
                    .get(&s.root)
                    .is_some_and(|drawn| drawn.contains(&surface))
        })
    }

    /// (lock-int) Whether lock content `surface` has drawn its lock's
    /// settled frame, the exit's end, since the lock closed after its
    /// exit; true for a surface render does not know or whose lock did
    /// not settle (nothing of it is left to draw). strand-surface's
    /// unlock after an exit waits for it on every content surface
    /// (`SurfaceHost::lock_end_drawn`).
    pub fn lock_end_drawn(&self, surface: SurfaceId) -> bool {
        let Some(s) = self.surfaces.get(&surface) else {
            return true;
        };
        self.lock_settled
            .get(&s.root)
            .is_none_or(|drawn| drawn.contains(&surface))
    }

    /// (lock-int) The session lock ended (unlocked, or the compositor
    /// ended it): every lock that settled lets its exit's end go, what it
    /// kept for it unmounting, so a lock shown later on the same nodes
    /// (the one asked for after `finished`) draws what a new lock does.
    /// Until then a settled lock keeps it across its surfaces going and
    /// coming (a renewal).
    pub fn lock_session_ended(&mut self) {
        let roots: Vec<NodeId> = std::mem::take(&mut self.lock_settled).into_keys().collect();
        for root in roots {
            self.drop_closing_content(root);
            for s in self.surfaces.values_mut() {
                if s.root == root {
                    s.mark_dirty();
                }
            }
        }
    }

    /// (lock-int) `surface` drew a frame: one of a settled lock drew its
    /// settled frame, and is frozen from now on.
    pub(super) fn lock_drew(&mut self, surface: SurfaceId) {
        let Some(root) = self.surfaces.get(&surface).map(|s| s.root) else {
            return;
        };
        if let Some(drawn) = self.lock_settled.get_mut(&root) {
            drawn.insert(surface);
        }
    }

    /// Unmounts the content kept for surface node `root`'s closing pose
    /// (it closed, or opened again and logic sent its content anew).
    pub(super) fn drop_closing_content(&mut self, root: NodeId) {
        let Some(ghosts) = self.closing_content.remove(&root) else {
            return;
        };
        for g in ghosts {
            let parent = self.tree.get(g).and_then(|n| n.parent);
            self.tree.drop_ghost(g);
            self.flip(parent);
        }
        self.mark_layout_of(root);
    }

    /// Unmounts ghosts whose exit finished and closes surfaces whose
    /// closing pose did.
    pub(super) fn process_finished(&mut self) {
        for (id, kind) in self.anim.take_finished() {
            match kind {
                ExitKind::Ghost => {
                    let parent = self.tree.get(id).and_then(|n| n.parent);
                    let root = self.tree.root_of(id);
                    self.tree.drop_ghost(id);
                    self.flip(parent);
                    for s in self.surfaces.values_mut() {
                        if Some(s.root) == root {
                            s.mark_layout();
                        }
                    }
                    self.spec_dirty.extend(root);
                }
                ExitKind::Close => {
                    self.closed.insert(id);
                    self.spec_dirty.insert(id);
                    // (lock-int) A lock's goes when it no longer settles
                    // (`refresh_specs`).
                    if self
                        .specs
                        .get(&id)
                        .is_none_or(|s| s.kind != strand_scene::NodeKind::Lock)
                    {
                        self.drop_closing_content(id);
                    }
                }
            }
        }
        let tree = &self.tree;
        self.anim.retain(|id| tree.contains(id));
    }

    /// How long an exit waits for frames before it ends at once (see
    /// [`EXIT_STALL`]; tests shorten it).
    pub fn set_exit_stall(&mut self, stall: Duration) {
        self.exit_stall = stall;
    }

    /// Ends exits no frame samples: older than the stall limit on a
    /// surface that painted nothing for as long, or older than any motion
    /// may run.
    pub(super) fn expire_exits(&mut self) {
        let now = Instant::now();
        let stall = self.exit_stall;
        for (id, started) in self.anim.exit_times() {
            let age = now.saturating_duration_since(started);
            let root = self.tree.root_of(id);
            let painting = self.surfaces.values().any(|s| {
                Some(s.root) == root
                    && s.painted_at
                        .is_some_and(|t| now.saturating_duration_since(t) < stall)
            });
            if age >= strand_scene::motion::MAX_MOTION + stall || (age >= stall && !painting) {
                self.anim.finish_now(id);
            }
        }
    }

    /// Ends exits nobody can see any more (their surface went or never
    /// showed, or it gets no frames).
    pub(super) fn reap_exits(&mut self) {
        self.expire_exits();
        let mut shown: HashSet<NodeId> = HashSet::new();
        for s in self.surfaces.values() {
            if s.painted {
                shown.insert(s.root);
            }
        }
        let tree = &self.tree;
        self.anim
            .finish_undrawn(|id| tree.root_of(id).is_none_or(|r| !shown.contains(&r)));
        self.process_finished();
    }
}

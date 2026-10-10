//! Exit poses: ghosts of removed nodes and surfaces playing their
//! closing pose, until they finish or stall; and (M4) the surface poses
//! the compositor applies ([`crate::pose`]).

use std::collections::HashSet;
use std::time::{Duration, Instant};

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

    /// (M4) The pose the compositor should apply to `surface` with its
    /// next commit: the one its last flattened frame took out of the
    /// root ([`crate::pose::delegate`]); `None` when nothing is
    /// delegated.
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
        s.cache.as_ref().and_then(|f| f.pose)
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
                    self.drop_closing_content(id);
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

//! Surface specs: resolving every surface node's parameters, content
//! sizing, overhang, and holding frames for the configure at a new size.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use strand_scene::{
    Anchor, Curve, Edge, Insets, LogicalRect, LogicalSize, NodeId, NodeKind, Prop, Scale, Size,
    SurfaceChange, SurfaceId, SurfaceSpec, TokenScope,
};

use super::Renderer;
use super::layout_pass::TextInfo;
use super::text::TextSlot;
use crate::anim::{ExitKind, exit_pose, is_pose};
use crate::flatten::{natural_texts, scope_tables};
use crate::layout::{MAX_CONTENT_SIZE, RootSize, layout};

/// The overhang a surface asks for: on an axis its anchor leaves centred
/// (both axes for `center`, the cross axis for an edge, none for a bar
/// or a corner), the larger side on both sides, since the compositor
/// centres the whole buffer and an uneven overhang would move the box off
/// centre.
pub(super) fn centred_overhang(spec: &SurfaceSpec, o: Insets) -> Insets {
    // A popup's box is its window geometry, which the compositor places:
    // its overhang may be uneven.
    if matches!(spec.kind, NodeKind::Bar | NodeKind::Popup) {
        return o;
    }
    let (h, v) = match spec.anchor {
        Anchor::Center => (true, true),
        Anchor::Top | Anchor::Bottom => (true, false),
        Anchor::Left | Anchor::Right => (false, true),
        _ => (false, false),
    };
    let mut o = o;
    if h {
        let m = o.left.max(o.right);
        (o.left, o.right) = (m, m);
    }
    if v {
        let m = o.top.max(o.bottom);
        (o.top, o.bottom) = (m, m);
    }
    o
}

/// The overhang a layer surface asks for, kept inside the edges it is
/// anchored to: on each anchored side no more than its margin there, so
/// the buffer never reaches past the output's edge (a shadow there would
/// be off screen anyway). The buffer's origin then lies on the surface's
/// own output. Hyprland constrains a popup to the monitor at its layer
/// surface's origin, or to the nearest monitor when none holds it
/// (`CPopup::reposition`, `CMonitorQueryCore::closestTo`): a floating,
/// shadowed bar's origin lay above and left of its output, nearer the
/// neighbour on its left, so its hover islands slid onto that monitor
/// (decisions.md m6-w2-surface). A popup's box is placed by the compositor,
/// so its overhang is left alone.
pub(super) fn within_edges(spec: &SurfaceSpec, o: Insets) -> Insets {
    let (top, right, bottom, left) = match spec.kind {
        NodeKind::Bar => match spec.edge.unwrap_or(Edge::Top) {
            Edge::Top => (true, true, false, true),
            Edge::Bottom => (false, true, true, true),
            Edge::Left => (true, false, true, true),
            Edge::Right => (true, true, true, false),
        },
        NodeKind::Panel | NodeKind::Osd => {
            let (mut t, mut r, mut b, mut l) = match spec.anchor {
                Anchor::Center => (false, false, false, false),
                Anchor::Top => (true, false, false, false),
                Anchor::Bottom => (false, false, true, false),
                Anchor::Left => (false, false, false, true),
                Anchor::Right => (false, true, false, false),
                Anchor::TopLeft => (true, false, false, true),
                Anchor::TopRight => (true, true, false, false),
                Anchor::BottomLeft => (false, false, true, true),
                Anchor::BottomRight => (false, true, true, false),
            };
            // `attach:` anchors that edge (its overhang is none already,
            // `fillet::flush`).
            match spec.attach.filter(|_| spec.kind == NodeKind::Panel) {
                Some(Edge::Top) => (t, b) = (true, false),
                Some(Edge::Bottom) => (t, b) = (false, true),
                Some(Edge::Left) => (l, r) = (true, false),
                Some(Edge::Right) => (l, r) = (false, true),
                None => {}
            }
            (t, r, b, l)
        }
        _ => return o,
    };
    let m = spec.margin;
    let cap = |v: f32, anchored: bool, margin: f32| {
        if anchored { v.min(margin.max(0.0)) } else { v }
    };
    Insets {
        top: cap(o.top, top, m.top),
        right: cap(o.right, right, m.right),
        bottom: cap(o.bottom, bottom, m.bottom),
        left: cap(o.left, left, m.left),
    }
}

/// (popup-shrink) Where the compositor keeps the box of the surface
/// `spec` describes as its size changes, as a share of the change on
/// each axis: 0 keeps its start edge (left or top), 1 its end edge, 0.5
/// its centre. `parent` is a popup's parent surface spec. A layer
/// surface keeps the edges it is anchored to (`anchor:`, and a panel's
/// `attach:`), as strand-surface's `placement::layer_config` anchors it.
/// A popup keeps its side that faces its anchor and stays centred on
/// the anchor across, or level with its row's top as a submenu beside
/// it, as `placement::popup_config` and `popup_request` place it. The
/// compositor's slide or flip of a popup that does not fit is not
/// followed.
pub fn resize_share(spec: &SurfaceSpec, parent: Option<&SurfaceSpec>) -> (f32, f32) {
    if spec.kind == NodeKind::Popup {
        let bar_edge = parent
            .filter(|p| p.kind == NodeKind::Bar)
            .map(|p| p.edge.unwrap_or(Edge::Top));
        let parent_popup = parent.is_some_and(|p| p.kind == NodeKind::Popup);
        // The popup's edge that faces its anchor: it stays.
        let facing = match spec.attach.or(bar_edge) {
            _ if spec.tooltip => Edge::Top,
            Some(e) => e,
            None if parent_popup => match spec.anchor {
                Anchor::Left | Anchor::TopLeft | Anchor::BottomLeft => Edge::Right,
                Anchor::Top => Edge::Bottom,
                Anchor::Bottom => Edge::Top,
                _ => Edge::Left,
            },
            None => Edge::Top,
        };
        // A submenu beside its row starts level with it.
        let aligned = parent_popup && spec.attach.is_none() && !spec.tooltip;
        let across = if aligned { 0.0 } else { 0.5 };
        return match facing {
            Edge::Top => (0.5, 0.0),
            Edge::Bottom => (0.5, 1.0),
            Edge::Left => (0.0, across),
            Edge::Right => (1.0, across),
        };
    }
    let (mut fx, mut fy) = match spec.anchor {
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
    // `attach:` anchors a panel to that edge; the other axis stays.
    match spec.attach.filter(|_| spec.kind == NodeKind::Panel) {
        Some(Edge::Top) => fy = 0.0,
        Some(Edge::Bottom) => fy = 1.0,
        Some(Edge::Left) => fx = 0.0,
        Some(Edge::Right) => fx = 1.0,
        None => {}
    }
    (fx, fy)
}

impl Renderer {
    /// (growth) Whether the box of `root`, a shown content-sized panel,
    /// OSD or popup described by `spec` and laid out last at `drawn`,
    /// springs down over a step of its content's size to `size`: on an
    /// axis it sizes by its content where `drawn` is larger, and whose
    /// curve is not `instant` (`width: auto ~ instant`); never under
    /// reduced motion.
    fn steps_down(
        &self,
        root: NodeId,
        spec: &SurfaceSpec,
        drawn: LogicalRect,
        size: LogicalSize,
    ) -> bool {
        if self.anim.reduced()
            || !matches!(spec.kind, NodeKind::Panel | NodeKind::Osd | NodeKind::Popup)
        {
            return false;
        }
        let Some(&(cw, ch)) = self.content_sized.get(&root) else {
            return false;
        };
        let springs = |sized: bool, axis: usize, was: f32, now: f32| {
            sized
                && was - now > super::layout_pass::STEP_MIN
                && self.step_curve(root, axis) != Curve::Instant
        };
        springs(cw, 0, drawn.w, size.w) || springs(ch, 1, drawn.h, size.h)
    }

    /// (popup-shrink) [`resize_share`] of the surface node `root`.
    pub(super) fn resize_share_of(&self, root: NodeId) -> Option<(f32, f32)> {
        let spec = self.specs.get(&root)?;
        let parent = spec.parent.and_then(|p| self.specs.get(&p));
        Some(resize_share(spec, parent))
    }

    /// Content-sized surfaces held at a larger size while things moved
    /// ask for their own size once nothing does, and any spec left dirty
    /// (an exit finished, a surface closed) is refreshed, so the host
    /// sees the change ([`Renderer::has_surface_changes`]) without
    /// waiting for an unrelated event.
    pub(super) fn release_holds(&mut self) {
        let ready: Vec<NodeId> = self
            .held
            .iter()
            .copied()
            .filter(|id| !self.moving(*id))
            .collect();
        for id in ready {
            self.held.remove(&id);
            self.spec_dirty.insert(id);
        }
        // (shadow-buffer) Shadows that settled: the overhang shrinks to
        // their reach at rest, asked for now (a content-sized surface's by
        // its spec, any other's by its layout pass), so the host has the
        // new size before the next frame is drawn at it.
        let settled: Vec<NodeId> = self
            .shadow_held
            .iter()
            .copied()
            .filter(|id| !self.anim.shadows_moving(&self.tree, *id))
            .collect();
        for id in settled {
            self.shadow_held.remove(&id);
            if self.content_sized.contains_key(&id) {
                self.spec_dirty.insert(id);
                continue;
            }
            let shown: Vec<SurfaceId> = self
                .surfaces
                .iter()
                .filter(|(_, s)| s.root == id && s.boxes.is_some())
                .map(|(sid, _)| *sid)
                .collect();
            for sid in shown {
                if let Some(s) = self.surfaces.get_mut(&sid) {
                    s.mark_layout();
                }
                let layouts = self.shaped();
                self.lay_out(sid, &layouts);
            }
        }
        if !self.spec_dirty.is_empty() {
            self.refresh_specs();
        }
    }

    /// (shadow-buffer) The overhang surface node `root` asks for, given
    /// `grown`, the reach of its shadows at rest (design.md: "Shadows
    /// enlarge the buffer"). A shadow in flight draws between where it
    /// was and where it rests, so while one springs the overhang is
    /// never less than the one asked for now: a shadow springing back
    /// (a hover's lift ending) keeps the room it is drawn in, and the
    /// surface shrinks once its shadows settle ([`Renderer::release_holds`]).
    /// Growing, it takes its new reach at once. What an overshoot carries
    /// past both is clipped, as with a size spring's (owner-testing.md
    /// bug 9).
    pub(super) fn hold_shadow_reach(&mut self, root: NodeId, grown: Insets) -> Insets {
        if !self.anim.shadows_moving(&self.tree, root) {
            return grown;
        }
        let Some(old) = self.specs.get(&root).map(|s| s.overhang) else {
            return grown;
        };
        let held = Insets {
            top: old.top.max(grown.top),
            right: old.right.max(grown.right),
            bottom: old.bottom.max(grown.bottom),
            left: old.left.max(grown.left),
        };
        if held != grown {
            self.shadow_held.insert(root);
        }
        held
    }

    /// How long a content-sized surface whose size changed holds its
    /// frame for the compositor's configure at the new size (see
    /// [`RESIZE_WAIT`]); zero, the default, never holds (offline
    /// rendering has no compositor to wait for).
    pub fn set_resize_wait(&mut self, wait: Duration) {
        self.resize_wait = wait;
        self.refresh_size_holds();
    }

    /// The logical size of the output `surface` is on: a content-sized
    /// surface is laid out no larger than it, less its margins (the
    /// surface manager clamps the layer size the same way), so it waits
    /// for no configure the compositor would never send.
    pub fn set_surface_bounds(&mut self, surface: SurfaceId, size: Option<LogicalSize>) {
        match size {
            Some(b) => self.bounds.insert(surface, b),
            None => self.bounds.remove(&surface),
        };
        self.refresh_size_holds();
    }

    /// The buffer size, logical pixels, the spec of `root` asks a
    /// surface of it for, on the content-sized axes (`None`: not sized by
    /// its content on that axis).
    pub(super) fn wanted_size(
        &self,
        surface: SurfaceId,
        root: NodeId,
    ) -> Option<(Option<f32>, Option<f32>)> {
        if !self.content_sized.contains_key(&root) {
            return None;
        }
        let spec = self.specs.get(&root)?;
        let (o, m) = (spec.overhang, spec.margin);
        let b = self.bounds.get(&surface);
        let fit = |v: Option<f32>, bound: Option<f32>, margins: f32| {
            v.map(|v| match bound {
                Some(b) => v.min((b - margins).max(1.0)),
                None => v,
            })
        };
        let w = fit(spec.width, b.map(|b| b.w), m.left + m.right).map(|w| w + o.left + o.right);
        let h = fit(spec.height, b.map(|b| b.h), m.top + m.bottom).map(|h| h + o.top + o.bottom);
        Some(match (spec.kind, spec.edge) {
            (NodeKind::Bar, Some(Edge::Left | Edge::Right)) => (w, None),
            (NodeKind::Bar, _) => (None, h),
            _ => (w, h),
        })
    }

    /// Holds the frames of content-sized surfaces configured at another
    /// size than their spec asks for, until the configure (or
    /// [`RESIZE_WAIT`]): whatever the order of the spec change and the
    /// configure (a spec that changed before the surface's first
    /// configure arrived holds its first frame too). Each size asked for
    /// holds at most once, so a compositor that configures another size
    /// costs one wait, not a stall.
    pub(super) fn refresh_size_holds(&mut self) {
        let ids: Vec<(SurfaceId, NodeId)> =
            self.surfaces.iter().map(|(i, s)| (*i, s.root)).collect();
        let now = Instant::now();
        for (id, root) in ids {
            let wanted = self.wanted_size(id, root);
            // (gpu-first review 2) A surface the GPU thread presents is
            // held too: its new size is pending surface state that only a
            // present commits, so the thread presents its last frame
            // again for it (`gpu_commit_holds`, at the hand-over of the
            // surface changes) and the configure comes as a CPU
            // surface's. A hold that runs out wakes the loop
            // (`GpuHost::wait` in the binary) and the frame is drawn at
            // the size the surface has.
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            // (growth) A larger buffer asked for, its configure to come.
            let have = s.scale.logical_size(s.size);
            let up = |want: Option<f32>, have: f32| want.is_some_and(|v| v.round() - have > 0.5);
            let larger = wanted
                .filter(|(w, h)| s.size != Size::default() && (up(*w, have.w) || up(*h, have.h)))
                .map(|(w, h)| LogicalSize::new(w.unwrap_or(have.w), h.unwrap_or(have.h)));
            match larger {
                Some(t) if s.awaited != Some(t) => {
                    s.awaits = true;
                    s.awaited = Some(t);
                }
                Some(_) => {}
                None => {
                    s.awaits = false;
                    s.awaited = None;
                }
            }
            let holds = !self.resize_wait.is_zero() && s.size != Size::default();
            let Some((w, h)) = wanted.filter(|_| holds) else {
                s.size_hold = None;
                continue;
            };
            let have = s.scale.logical_size(s.size);
            let off =
                |want: Option<f32>, have: f32| want.is_some_and(|v| (v.round() - have).abs() > 1.0);
            if !(off(w, have.w) || off(h, have.h)) {
                // Configured at what it asked for.
                s.size_hold = None;
                continue;
            }
            let target = LogicalSize::new(w.unwrap_or(have.w), h.unwrap_or(have.h));
            if s.size_hold.is_none_or(|(t, _)| t != target) {
                // A first frame waits as long as it would for its text:
                // nothing shows meanwhile, and a busy main thread (other
                // surfaces painting at boot) can delay the configure
                // past one round trip.
                let wait = if s.painted {
                    self.resize_wait
                } else {
                    self.resize_wait.max(self.first_frame_wait)
                };
                s.size_hold = Some((target, now + wait));
                s.size_paused = Duration::ZERO;
            }
        }
    }

    /// The resolved surface parameters of a surface-kind node (as of the
    /// last [`Renderer::apply`]): what `strand-surface` creates its layer
    /// surface from.
    pub fn surface_spec(&self, node: NodeId) -> Option<&SurfaceSpec> {
        self.specs.get(&node)
    }

    /// Surface-kind nodes created, changed or removed since the last call,
    /// in order. Token changes that move a resolved value (`margin:
    /// $space.2`) are reported as updates.
    pub fn take_surface_changes(&mut self) -> Vec<(NodeId, SurfaceChange)> {
        // (gpu-first review 2) The manager sets what these change on the
        // surfaces the GPU thread presents without a commit: the thread
        // is asked for one (sent after them, at the host's next pump).
        #[cfg(feature = "gpu")]
        self.gpu_commit_holds();
        std::mem::take(&mut self.surface_changes)
    }

    /// True if surface changes are waiting for
    /// [`Renderer::take_surface_changes`]: a host that called in from the
    /// surface manager (a configure, a paint) wakes its loop to hand
    /// them over at once, as a resized surface's frame is held for them.
    pub fn has_surface_changes(&self) -> bool {
        !self.surface_changes.is_empty()
    }

    /// Re-resolves every surface spec and records what changed. A
    /// surface with no size of its own (a panel, OSD or popup without
    /// `width`/`height`, a bar without a thickness) is sized by its
    /// content, laid out on its own; every surface reports how far its
    /// shadows reach past its box (`SurfaceSpec::overhang`).
    ///
    /// The content pass runs only where it decides something: never for
    /// a closed surface (it is sized when it opens), and for a surface of
    /// a fixed size only until it is shown (then the overhang comes from
    /// the pass that lays it out for painting, see `flatten_now`).
    pub(super) fn refresh_specs(&mut self) {
        let dirty = std::mem::take(&mut self.spec_dirty);
        let layouts = (!dirty.is_empty()).then(|| self.shaped());
        let ids: Vec<NodeId> = self.tree.surface_nodes().collect();
        let mut live = BTreeSet::new();
        let mut requests = Vec::new();
        for id in ids {
            let Some(node) = self.tree.get(id) else {
                continue;
            };
            let tables = scope_tables(&self.tree, id);
            let scope = TokenScope::new(&tables);
            let mut spec =
                SurfaceSpec::resolve(node.kind, |p| node.get(p).and_then(|v| scope.resolve(v)));
            live.insert(id);
            if node.kind == NodeKind::Popup {
                // Nested in the surface of the element it is declared in,
                // anchored to that element's laid-out box there; shown only
                // while that surface is.
                let anchor = node.parent;
                let parent = anchor.and_then(|a| self.tree.root_of(a));
                spec.parent = parent;
                spec.tooltip = self.tree.is_overlay(id);
                // (flex) `anchor: pointer`: where the press that opened
                // it went down (at its element when no press did).
                let at_pointer = spec
                    .at_pointer
                    .then(|| self.pointing.anchor(id, anchor, parent, spec.open))
                    .flatten();
                spec.anchor_rect = at_pointer
                    .or_else(|| anchor.zip(parent).and_then(|(a, p)| self.anchor_box(a, p)));
                if parent.is_some_and(|p| self.specs.get(&p).is_some_and(|ps| !ps.open)) {
                    spec.open = false;
                }
            }
            // Surface poses: `enter` plays when it opens, `exit` before it
            // closes (it stays open until the pose settles).
            let reported = self.specs.get(&id).map(|s| s.open);
            let reduced = self.anim.reduced();
            if spec.open {
                self.closed.remove(&id);
                if self.anim.exiting(id) == Some(ExitKind::Close) {
                    self.anim.cancel_exit(id);
                    // Its pose may have sized it: the next layout lets go.
                    self.mark_layout_of(id);
                } else if reported != Some(true) && !reduced && is_pose(node.get(Prop::Enter)) {
                    self.anim.enter(id);
                }
                // Opened by the diff that created these nodes: they
                // enter with it (a surface that was never reported, at
                // boot, shows at rest).
                if reported == Some(false) && !reduced {
                    if !self.shown(Some(id)) {
                        self.opening.insert(id);
                        // (cross-surface-morph) What it holds comes on
                        // screen with its first frame: a `morph` name
                        // there starts from its partner's box.
                        self.anim.surface_opened(id);
                        // (launcher-reopen) Opened again, what it kept
                        // while closed (a surface's body is mounted when
                        // it is first shown and frozen while hidden, not
                        // unmounted) comes on screen as it first came,
                        // mounted by the opening diff: each node there
                        // with an enter pose plays it. Its first opening
                        // plays only those born with it (below).
                        let mut walk: Vec<NodeId> = if self.was_shown.contains(&id) {
                            self.tree
                                .get(id)
                                .map(|n| n.children.clone())
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        while let Some(n) = walk.pop() {
                            let Some(c) = self.tree.get(n) else { continue };
                            if c.kind.is_surface() || !self.tree.contains_live(n) {
                                continue;
                            }
                            if is_pose(c.get(Prop::Enter)) {
                                self.anim.enter(n);
                            }
                            walk.extend(c.children.iter().copied());
                        }
                    }
                    for n in &self.born {
                        if self.tree.root_of(*n) == Some(id)
                            && self.tree.contains_live(*n)
                            && self
                                .tree
                                .get(*n)
                                .is_some_and(|c| is_pose(c.get(Prop::Enter)))
                        {
                            self.anim.enter(*n);
                        }
                    }
                }
            } else if reported == Some(true) && !self.closed.contains(&id) {
                if self.anim.exiting(id) == Some(ExitKind::Close) {
                    spec.open = true;
                } else if !reduced && is_pose(exit_pose(node)) && self.shown(Some(id)) {
                    self.anim.exit(id, ExitKind::Close);
                    self.mark_layout_of(id);
                    spec.open = true;
                } else if !reduced && self.ghosts_under(id) && self.shown(Some(id)) {
                    // Rows still leaving keep it open; it closes when the
                    // last ghost unmounts (which refreshes the specs).
                    spec.open = true;
                }
            }
            if !spec.open && reported == Some(true) {
                // (popup-reopen) It closes now (its closing pose, if it
                // had one, has ended): opened again, it enters from its
                // pose, not from what its exit held. A preview flattened
                // before the surface goes would otherwise snap that to
                // rest (the exit no longer holds it), and the next
                // opening would show at rest at once.
                let tree = &self.tree;
                self.anim.surface_closed(|n| tree.root_of(n) == Some(id));
            }
            if !spec.open || self.shown(Some(id)) {
                self.opening.remove(&id);
            }
            // Content kept for a closing pose that is not playing (it
            // opened again, or closes at once after all) goes now.
            if self.closing_content.contains_key(&id)
                && self.anim.exiting(id) != Some(ExitKind::Close)
            {
                self.drop_closing_content(id);
            }
            let bar = spec.kind == NodeKind::Bar;
            let vertical = matches!(spec.edge, Some(Edge::Left | Edge::Right));
            let axes = match (bar, vertical) {
                (true, true) => (spec.width.is_none(), false),
                (true, false) => (false, spec.height.is_none()),
                _ => (spec.width.is_none(), spec.height.is_none()),
            };
            let content_sized = axes.0 || axes.1;
            if content_sized {
                self.content_sized.insert(id, axes);
            } else {
                self.content_sized.remove(&id);
            }
            // A surface already showing this node: its scale, and for a bar
            // the length the compositor gave it.
            let shown = self.surfaces.values().find(|s| s.root == id);
            let scale = shown.map_or(Scale::ONE, |s| s.scale);
            let old = self.specs.get(&id);
            let laid_out = shown.is_some_and(|s| s.boxes.is_some());
            let pass = spec.open && (content_sized || !laid_out);
            if !spec.open {
                // Nothing to shape for a surface nobody sees.
                self.spec_wanted.remove(&id);
            }
            let (size, overhang) = match (&layouts, old) {
                (Some(layouts), _) if dirty.contains(&id) && pass => {
                    let along = shown.map(|s| {
                        let l = s.scale.logical_size(s.size);
                        let o = s.overhang;
                        if vertical {
                            l.h - o.top - o.bottom
                        } else {
                            l.w - o.left - o.right
                        }
                    });
                    let (w, h) = match (bar, vertical) {
                        (true, false) => (along, spec.height),
                        (true, true) => (spec.width, along),
                        _ => (spec.width, spec.height),
                    };
                    // Sized by its texts' estimates until they are
                    // shaped (`TextSizes::estimates`, late-text-dip).
                    let info = TextInfo {
                        shaped: layouts,
                        scale,
                        estimate: true,
                    };
                    let root_size = RootSize::Content {
                        width: w,
                        height: h,
                    };
                    // At rest: exiting nodes take their exit pose's size,
                    // and one an exit squeezes keeps its layout at rest,
                    // where it was last laid out (bar-blank-on-exit: as
                    // the painted pass does, or its text would wrap into
                    // a column and the surface ask for its height).
                    if let Some(b) = shown.and_then(|s| s.boxes.as_ref()) {
                        self.anim.hold_exits(&self.tree, id, &b.rects);
                    }
                    let rest = self.anim.rest_sizes(&self.tree, id);
                    let mut b = layout(&self.tree, id, root_size, &info, &mut self.scrolls, &rest);
                    self.layout_passes += 1;
                    if b.unsettled {
                        // A list measured rows it had only estimated: its
                        // size (and so the surface's) comes from the
                        // second pass, as the painted one does.
                        b = layout(&self.tree, id, root_size, &info, &mut self.scrolls, &rest);
                        self.layout_passes += 1;
                    }
                    // Never smaller while something on it moves (a toast
                    // collapsing, its siblings sliding up), or while its
                    // box springs down over a step of its content (growth:
                    // a row gone with no pose, a text shorter): it shrinks
                    // once everything settles.
                    let mut held = false;
                    // Drawn at the size it asked for: one the compositor
                    // gave another size fills it, and never springs.
                    let near = |a: Option<f32>, b: f32| a.is_none_or(|a| (a - b).abs() < 1.0);
                    let drawn = shown
                        .filter(|s| s.painted)
                        .and_then(|s| s.boxes.as_ref()?.rects.get(&id).copied())
                        .filter(|r| old.is_some_and(|o| near(o.width, r.w) && near(o.height, r.h)));
                    let steps = drawn.is_some_and(|r| self.steps_down(id, &spec, r, b.size));
                    if let Some(old) = old.filter(|_| steps || self.moving(id)) {
                        let (w, h) = (old.width.unwrap_or(0.0), old.height.unwrap_or(0.0));
                        held = b.size.w.ceil() < w || b.size.h.ceil() < h;
                        b.size.w = b.size.w.max(w);
                        b.size.h = b.size.h.max(h);
                    }
                    if held {
                        self.held.insert(id);
                    } else {
                        self.held.remove(&id);
                    }
                    if content_sized {
                        let r = natural_texts(&self.tree, id, scale, &b.rects);
                        self.spec_wanted
                            .insert(id, r.iter().map(|(n, t)| TextSlot::of(*n, t)).collect());
                        requests.extend(r);
                    } else {
                        self.spec_wanted.remove(&id);
                    }
                    // `attach:` fillets reach past the box too.
                    (b.size, crate::fillet::grow(&self.tree, &b, id, b.overhang))
                }
                (_, Some(old)) => (
                    LogicalSize::new(old.width.unwrap_or(0.0), old.height.unwrap_or(0.0)),
                    old.overhang,
                ),
                _ => (LogicalSize::default(), Insets::default()),
            };
            let overhang = self.hold_shadow_reach(id, overhang);
            spec.overhang = crate::fillet::flush(
                &self.tree,
                id,
                within_edges(&spec, centred_overhang(&spec, overhang)),
            );
            if content_sized {
                // Capped: content taller than any output (a list with no
                // `max_height`, a long body) never asks for a buffer of
                // its full size; the output's own size caps it further
                // where the surface is placed.
                let cap = |v: f32| v.ceil().clamp(1.0, MAX_CONTENT_SIZE);
                let (w, h) = (cap(size.w), cap(size.h));
                match (bar, vertical) {
                    (true, false) => spec.height = Some(h),
                    (true, true) => spec.width = Some(w),
                    _ => {
                        spec.width.get_or_insert(w);
                        spec.height.get_or_insert(h);
                    }
                }
            }
            self.record_spec(id, spec);
        }
        let gone: Vec<NodeId> = self
            .specs
            .keys()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.specs.remove(&id);
            self.surface_changes.push((id, SurfaceChange::Removed));
        }
        self.content_sized.retain(|id, _| live.contains(id));
        self.held.retain(|id| live.contains(id));
        self.shadow_held.retain(|id| live.contains(id));
        self.held_reach.retain(|id, _| live.contains(id));
        self.opening.retain(|id| live.contains(id));
        self.closing_content.retain(|id, _| live.contains(id));
        self.spec_wanted.retain(|id, _| live.contains(id));
        if !requests.is_empty() && self.request_text(&requests) {
            // Shaped inline: size the surfaces with it at once.
            self.spec_dirty
                .extend(requests.iter().filter_map(|(n, _)| self.tree.root_of(*n)));
            self.refresh_specs();
        }
        self.refresh_size_holds();
    }

    /// The laid-out box of `anchor`, a popup's anchor element, on a
    /// surface showing `root`, the surface node it is in: the popup's
    /// `anchor_rect`.
    fn anchor_box(&self, anchor: NodeId, root: NodeId) -> Option<LogicalRect> {
        self.surfaces
            .values()
            .filter(|s| s.root == root)
            .find_map(|s| s.boxes.as_ref()?.rects.get(&anchor).copied())
    }

    /// (popup-boot-wake) `root` was laid out: a popup anchored in it
    /// that waits for its anchor's box (`anchor_rect` none, so the
    /// surface manager has not made it) and now has one is re-resolved
    /// by the next spec refresh, which the frame or wake that laid it
    /// out runs (`release_holds`), so the host makes the popup at once
    /// rather than at an unrelated wake (a bar with no text has none
    /// at boot). An anchor that moves keeps its popup where it was until
    /// the next refresh, as before: following it here would reposition
    /// (or, below `xdg_wm_base` 3, make again) the popup every frame
    /// it springs.
    ///
    /// (growth) Once `root` rests (nothing on it springs, nor its box over
    /// a step, and it is laid out in the buffer it asked for), a popup
    /// whose anchor's box is not the one it was placed at is re-resolved
    /// too: its anchor was last read from a pass laid out in a buffer the
    /// surface was leaving (a preview pass before its configure squeezes
    /// or moves what the old buffer holds), or while it moved.
    pub(super) fn place_waiting_popups(&mut self, root: NodeId) {
        let rest = !self.anim.sizes_moving(&self.tree, root)
            && !self.anim.size_pending(&self.tree, root)
            && !self.held.contains(&root)
            && self
                .surfaces
                .values()
                .filter(|s| s.root == root)
                .all(|s| !s.awaits && s.boxes.is_some());
        let moved = |a: LogicalRect, b: LogicalRect| {
            [(a.x, b.x), (a.y, b.y), (a.w, b.w), (a.h, b.h)]
                .iter()
                .any(|(p, q)| (p - q).abs() > 0.5)
        };
        let waiting: Vec<(NodeId, Option<LogicalRect>)> = self
            .specs
            .iter()
            .filter(|(_, s)| {
                s.kind == NodeKind::Popup
                    && s.parent == Some(root)
                    && (s.anchor_rect.is_none() || rest)
            })
            .map(|(id, s)| (*id, s.anchor_rect))
            .collect();
        for (id, placed) in waiting {
            let anchor = self.tree.get(id).and_then(|n| n.parent);
            let Some(now) = self
                .pointing
                .held_anchor(id)
                .or_else(|| anchor.and_then(|a| self.anchor_box(a, root)))
            else {
                continue;
            };
            if placed.is_none_or(|p| moved(p, now)) {
                self.spec_dirty.insert(id);
            }
        }
    }

    /// Records `spec` as the spec of surface node `id`, reporting a
    /// change; shown surfaces lay out again inside a new overhang.
    pub(super) fn record_spec(&mut self, id: NodeId, spec: SurfaceSpec) {
        let change = match self.specs.get(&id) {
            None => SurfaceChange::Created(spec.clone()),
            Some(old) if *old != spec => SurfaceChange::Updated {
                recreate: old.needs_recreate(&spec),
                spec: spec.clone(),
            },
            Some(_) => return,
        };
        for s in self.surfaces.values_mut() {
            if s.root == id && s.overhang != spec.overhang {
                s.overhang = spec.overhang;
                s.mark_layout();
            }
        }
        self.surface_changes.push((id, change));
        self.specs.insert(id, spec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: NodeKind, props: &[(Prop, &str)]) -> SurfaceSpec {
        SurfaceSpec::resolve(kind, |p| {
            props
                .iter()
                .find(|(q, _)| *q == p)
                .map(|(_, v)| strand_scene::PropValue::Keyword((*v).into()))
        })
    }

    /// (popup-shrink) Where a resized surface's box stays, as the surface
    /// manager places it (strand-surface placement.rs `layer_config`,
    /// `popup_config`, `popup_request`): a layer surface by its anchor
    /// and a panel's `attach:`; a popup by the side facing its anchor,
    /// centred across it unless it is a submenu beside its row.
    #[test]
    fn a_resized_box_stays_where_the_surface_manager_keeps_it() {
        let bar = spec(NodeKind::Bar, &[]);
        let bottom_bar = spec(NodeKind::Bar, &[(Prop::Edge, "bottom")]);
        let panel = spec(NodeKind::Panel, &[]);
        let popup = spec(NodeKind::Popup, &[]);
        let cases: [(SurfaceSpec, Option<&SurfaceSpec>, (f32, f32)); 9] = [
            (panel.clone(), None, (0.5, 0.5)),
            (
                spec(NodeKind::Osd, &[(Prop::Anchor, "bottom_right")]),
                None,
                (1.0, 1.0),
            ),
            (
                spec(NodeKind::Panel, &[(Prop::Attach, "top")]),
                None,
                (0.5, 0.0),
            ),
            // Below a top bar: its top stays, centred across.
            (popup.clone(), Some(&bar), (0.5, 0.0)),
            (popup.clone(), Some(&bottom_bar), (0.5, 1.0)),
            (popup.clone(), Some(&panel), (0.5, 0.0)),
            // A submenu: beside its row, level with its top.
            (popup.clone(), Some(&popup), (0.0, 0.0)),
            (
                spec(NodeKind::Popup, &[(Prop::Anchor, "left")]),
                Some(&popup),
                (1.0, 0.0),
            ),
            (
                spec(NodeKind::Popup, &[(Prop::Attach, "left")]),
                Some(&panel),
                (0.0, 0.5),
            ),
        ];
        for (s, parent, want) in cases {
            assert_eq!(resize_share(&s, parent), want, "{s:?} in {parent:?}");
        }
    }

    /// A box keeps its share of a resize in whole pixels, a centred one
    /// at half its size taken down as the compositor places it.
    #[test]
    fn a_resized_box_moves_by_whole_pixels() {
        use crate::layout::edge_shift;
        for (from, to, share, want) in [
            (221.0, 150.0, 0.5, 35.0),
            (150.0, 221.0, 0.5, -35.0),
            (220.0, 160.0, 0.5, 30.0),
            (101.0, 40.0, 0.5, 30.0),
            // A buffer's logical size at scale 1.5 (332 px for 221).
            (221.333, 150.0, 0.5, 35.0),
            (221.0, 150.0, 1.0, 71.0),
            (150.0, 221.0, 1.0, -71.0),
            (221.0, 150.0, 0.0, 0.0),
        ] {
            assert_eq!(
                edge_shift(from, to, share),
                want,
                "{from} -> {to} at {share}"
            );
        }
    }
}

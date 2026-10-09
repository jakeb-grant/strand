//! Surface specs: resolving every surface node's parameters, content
//! sizing, overhang, and holding frames for the configure at a new size.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use strand_scene::{
    Anchor, Edge, Insets, LogicalSize, NodeId, NodeKind, Prop, Scale, Size, SurfaceChange,
    SurfaceId, SurfaceSpec, TokenScope,
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

impl Renderer {
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
        if !self.spec_dirty.is_empty() {
            self.refresh_specs();
        }
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
        if !self.content_sized.contains(&root) {
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
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
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
                spec.anchor_rect = anchor.zip(parent).and_then(|(a, p)| {
                    self.surfaces
                        .values()
                        .filter(|s| s.root == p)
                        .find_map(|s| s.boxes.as_ref()?.rects.get(&a).copied())
                });
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
            let content_sized = if bar {
                if vertical {
                    spec.width.is_none()
                } else {
                    spec.height.is_none()
                }
            } else {
                spec.width.is_none() || spec.height.is_none()
            };
            if content_sized {
                self.content_sized.insert(id);
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
                    let info = TextInfo {
                        shaped: layouts,
                        scale,
                    };
                    let root_size = RootSize::Content {
                        width: w,
                        height: h,
                    };
                    // At rest: exiting nodes take their exit pose's size.
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
                    // collapsing, its siblings sliding up): it shrinks
                    // once everything settles.
                    let mut held = false;
                    if let Some(old) = old.filter(|_| self.moving(id)) {
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
            spec.overhang = crate::fillet::flush(&self.tree, id, centred_overhang(&spec, overhang));
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
        self.content_sized.retain(|id| live.contains(id));
        self.held.retain(|id| live.contains(id));
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

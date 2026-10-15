//! xdg popups: nesting, positioners, grabs and the keyboard they hold,
//! and dismissal.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    // ---- popups -------------------------------------------------------------

    /// The surface a popup of `spec` nests in: one showing its parent
    /// node, mapped (the last one a button was pressed on, when the parent
    /// shows on several).
    pub(super) fn popup_parent(&self, spec: &SurfaceSpec) -> Option<SurfaceId> {
        let parent = spec.parent?;
        let mut candidates: Vec<&Surface> = self
            .surfaces
            .values()
            .filter(|s| s.node == parent && s.mapped() && s.configured)
            .collect();
        candidates.sort_by_key(|s| std::cmp::Reverse(Some(s.id) == self.last_pressed));
        candidates.first().map(|s| s.id)
    }

    /// Creates or destroys `node`'s popup so it shows exactly while open
    /// and its parent is mapped.
    pub(super) fn reconcile_popup(&mut self, node: NodeId, spec: &SurfaceSpec) {
        let existing = self.surfaces_of(node);
        if !spec.open {
            self.dismissed.remove(&node);
        }
        let parent = (spec.open && !self.dismissed.contains(&node))
            .then(|| self.popup_parent(spec))
            .flatten();
        let parent_spec = spec.parent.and_then(|p| self.specs.get(&p)).cloned();
        let config = match (&parent, &parent_spec) {
            (Some(_), Some(ps)) => popup_config(spec, ps).ok(),
            _ => None,
        };
        log::trace!(
            "{}: popup open {} parent {:?} config {:?} (spec parent {:?}, anchor {:?}, size {:?}×{:?})",
            spec.namespace(),
            spec.open,
            parent,
            config,
            spec.parent,
            spec.anchor_rect,
            spec.width,
            spec.height
        );
        let Some((parent, config)) = parent.zip(config) else {
            for id in existing {
                self.destroy_surface(id);
            }
            return;
        };
        if existing.is_empty() {
            self.create_popup(node, spec, parent, config);
        }
    }

    /// The popups nested in surfaces of `node` (now mapped or gone) are
    /// reconciled.
    pub(super) fn reconcile_children(&mut self, node: NodeId) {
        let kids: Vec<NodeId> = self
            .specs
            .iter()
            .filter(|(_, s)| s.kind == NodeKind::Popup && s.parent == Some(node))
            .map(|(n, _)| *n)
            .collect();
        for k in kids {
            self.reconcile(k);
        }
    }

    /// A popup spec changed: a new size or anchor repositions it (or, on
    /// an `xdg_wm_base` older than version 3, makes it again).
    pub(super) fn reconfigure_popups(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        let parent_spec = spec.parent.and_then(|p| self.specs.get(&p)).cloned();
        let new = parent_spec.and_then(|ps| popup_config(&spec, &ps).ok());
        for id in self.surfaces_of(node) {
            let Some(new) = new.clone() else {
                self.destroy_surface(id);
                continue;
            };
            let Some((positioner, ask)) = self
                .popup_parent_of(id)
                .and_then(|parent| self.positioner(&new, parent))
            else {
                continue;
            };
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let Role::Popup {
                popup,
                config,
                asked,
                ..
            } = &mut s.role
            else {
                continue;
            };
            if *config == new {
                continue;
            }
            if new.grab != config.grab || new.namespace != config.namespace {
                self.destroy_surface(id);
                continue;
            }
            if popup.xdg_popup().version() >= 3 {
                log::debug!(
                    "{}: popup repositioned: {}×{} at {:?} (was {}×{} at {:?}, overhang {:?} was {:?})",
                    new.namespace,
                    new.width,
                    new.height,
                    new.anchor_rect,
                    config.width,
                    config.height,
                    config.anchor_rect,
                    new.overhang,
                    config.overhang
                );
                popup.reposition(&positioner, 0);
                *config = new.clone();
                *asked = Some(ask);
                s.config = new.as_layer();
                s.geometry_dirty = true;
                if !self.gpu.has(id) {
                    popup.wl_surface().commit();
                    s.stats.bare_commits += 1;
                    self.stats.bare_commits += 1;
                }
            } else {
                log::debug!(
                    "{}: popup made again (xdg_wm_base version {})",
                    new.namespace,
                    popup.xdg_popup().version()
                );
                self.destroy_surface(id);
            }
        }
        for id in self.surfaces_of(node) {
            self.sync_popup_scrim(id, &spec);
        }
        self.reconcile(node);
    }

    /// The parent surface of popup `id`.
    fn popup_parent_of(&self, id: SurfaceId) -> Option<SurfaceId> {
        self.surfaces.get(&id).and_then(|s| s.role.popup_parent())
    }

    /// (owner-testing.md bug 6) Surface `parent`'s output, relative to its
    /// window geometry (a layer surface's is its buffer, a popup's its
    /// box), from where the manager placed it (`origin.rs`) and the
    /// output's logical place; `None` while either is unknown.
    fn output_in_parent(&self, parent: SurfaceId) -> Option<(i32, i32, i32, i32)> {
        let p = self.surfaces.get(&parent)?;
        let (ox, oy) = p.origin?;
        let (gx, gy) = match &p.role {
            Role::Popup { config, .. } => (config.overhang[3], config.overhang[0]),
            _ => (0, 0),
        };
        let root = self.surfaces.get(&self.root_layer(parent)?)?;
        let m = root.monitor.as_ref().and_then(|m| self.monitors.get(m))?;
        let (mx, my) = m.position?;
        let (mw, mh) = m.logical_size.filter(|(w, h)| *w > 0 && *h > 0)?;
        Some((mx - ox - gx, my - oy - gy, mw, mh))
    }

    /// A positioner for `c` in surface `parent`, and the box position it
    /// asks for (relative to the parent's window geometry). The anchor
    /// rect is where the parent's content at rest is drawn now: a pose
    /// scales its coordinates (a popup opened from a panel held at a
    /// static `scale`). (owner-testing.md bug 6) The box is moved inside
    /// the parent's output along its anchor's edge first
    /// ([`popup_shift`]), so the compositor need not slide it; a slide
    /// is asked for only on an axis where the box does not fit its
    /// output, or while the manager cannot know (a posed parent, an
    /// output not yet described) ([`popup_slides`]).
    pub(super) fn positioner(
        &self,
        c: &PopupConfig,
        parent: SurfaceId,
    ) -> Option<(XdgPositioner, (i32, i32))> {
        let factor = self
            .surfaces
            .get(&parent)
            .map_or((1.0, 1.0), |p| p.pose_factor());
        let shell = self.xdg_shell.as_ref()?;
        let p = XdgPositioner::new(shell).ok()?;
        p.set_size(c.width.max(1) as i32, c.height.max(1) as i32);
        let (x, y, w, h) = c.anchor_rect;
        let (x, y, w, h) = super::pose::posed_rect(
            (i64::from(x), i64::from(y), i64::from(w), i64::from(h)),
            factor,
            false,
        )
        .unwrap_or_else(|| {
            // Shrunk to nothing: a pixel where it went.
            let at = |v: i32, f: f64| (f64::from(v) * f).round() as i32;
            (at(x, factor.0), at(y, factor.1), 1, 1)
        });
        let rect = (x, y, w.max(1), h.max(1));
        p.set_anchor_rect(rect.0, rect.1, rect.2, rect.3);
        let within = self
            .output_in_parent(parent)
            .filter(|_| factor == (1.0, 1.0));
        let shift = within.map_or((0, 0), |within| popup_shift(c, rect, within));
        let (slide_x, slide_y) = popup_slides(c, rect, shift, within);
        let (ax, ay, _, _) = popup_request(c, rect, shift);
        use xdg_positioner::{Anchor, ConstraintAdjustment as Adj, Gravity};
        let (anchor, gravity, offset, flip) = match (c.side, c.aligned) {
            (PopupSide::Below, _) => (Anchor::Bottom, Gravity::Bottom, (0, c.gap), Adj::FlipY),
            (PopupSide::Above, _) => (Anchor::Top, Gravity::Top, (0, -c.gap), Adj::FlipY),
            (PopupSide::Right, false) => (Anchor::Right, Gravity::Right, (c.gap, 0), Adj::FlipX),
            (PopupSide::Left, false) => (Anchor::Left, Gravity::Left, (-c.gap, 0), Adj::FlipX),
            // A submenu: level with its row's top, growing down.
            (PopupSide::Right, true) => (
                Anchor::TopRight,
                Gravity::BottomRight,
                (c.gap, 0),
                Adj::FlipX,
            ),
            (PopupSide::Left, true) => (
                Anchor::TopLeft,
                Gravity::BottomLeft,
                (-c.gap, 0),
                Adj::FlipX,
            ),
        };
        let offset = (offset.0 + shift.0, offset.1 + shift.1);
        p.set_anchor(anchor);
        p.set_gravity(gravity);
        p.set_offset(offset.0, offset.1);
        let mut adj = flip;
        if slide_x {
            adj |= Adj::SlideX;
        }
        if slide_y {
            adj |= Adj::SlideY;
        }
        p.set_constraint_adjustment(adj);
        Some((p, (ax, ay)))
    }

    /// Creates `node`'s popup in surface `parent`, grabbing with the
    /// press that opened it ([`State::set_popup_cause`]) when that press
    /// is the seat's last and came within [`GRAB_WINDOW`] (never for a
    /// tooltip or (M6) a `trigger: hover` popup, nor in a parent popup
    /// that has no grab).
    pub(super) fn create_popup(
        &mut self,
        node: NodeId,
        spec: &SurfaceSpec,
        parent: SurfaceId,
        config: PopupConfig,
    ) {
        if self.xdg_shell.is_none() {
            log::debug!("{}: no xdg_wm_base, popups are not shown", spec.namespace());
            return;
        }
        let Some((positioner, ask)) = self.positioner(&config, parent) else {
            return;
        };
        // It grabs only with the serial of the press that opened it, and
        // only while that press is fresh: a popup a hover, a timer, `on
        // change` or IPC opened has no grab, so it neither takes the
        // keyboard nor closes other grabbing popups. A popup in a parent
        // popup without a grab has none either: xdg-shell lets a
        // grabbing popup's parent be only a toplevel or another grabbing
        // popup. (M6) The pointer opens a `trigger: hover` popup: it
        // never grabs (spec.md 6.3), though it takes input.
        let cause = self.popup_cause.remove(&node);
        let parent_ok = self
            .surfaces
            .get(&parent)
            .is_none_or(|p| !matches!(p.role, Role::Popup { .. }) || p.role.grabbed());
        let grab = match &self.last_action {
            Some(a)
                if config.grab
                    && !spec.hover_trigger
                    && parent_ok
                    && cause == Some(a.serial)
                    && a.at.elapsed() <= GRAB_WINDOW =>
            {
                Some((a.seat.clone(), a.serial))
            }
            _ => None,
        };
        if grab.is_some() {
            self.dismiss_other_grabs(parent);
            // The layer surface takes the keyboard before the grab starts:
            // once it has, the compositor moves keyboard focus no more.
            if let Some(layer) = self.root_layer(parent) {
                self.set_grab_keyboard(layer, true);
            }
        }
        let Some(shell) = self.xdg_shell.as_ref() else {
            return;
        };
        let Some(ps) = self.surfaces.get(&parent) else {
            self.sync_popup_keyboard();
            return;
        };
        let parent_xdg = match &ps.role {
            Role::Popup { popup, .. } => Some(popup.xdg_surface().clone()),
            Role::Layer(_) => None,
            // A lock surface cannot parent an xdg_popup.
            Role::Lock(_) => {
                log::debug!("{}: popups do not open on a lock screen", spec.namespace());
                self.sync_popup_keyboard();
                return;
            }
        };
        let wl = self.compositor.create_surface(&self.qh);
        let popup = match Popup::from_surface(
            parent_xdg.as_ref(),
            &positioner,
            &self.qh,
            wl.clone(),
            shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("{}: no popup: {e}", spec.namespace());
                self.sync_popup_keyboard();
                return;
            }
        };
        if let Role::Layer(layer) = &ps.role {
            layer.get_popup(popup.xdg_popup());
        }
        let (monitor, output, scale_src) = (ps.monitor.clone(), ps.output, ps.output);
        if let Some((seat, serial)) = &grab {
            popup.xdg_popup().grab(seat, *serial);
            self.stats.grabs += 1;
        }
        let key = (node, Placement::Focused);
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        // A viewport whenever the viewporter is there: the fractional
        // path sizes the surface with it, and a pose's scale (M4) sets
        // its destination on either path.
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(id)));
        let fractional = match (&viewport, &self.fractional_manager) {
            (Some(_), Some(fm)) => Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            _ => None,
        };
        let (scale, integer_scale) = self.initial_scale(scale_src, fractional.is_some());
        let layer_like = config.as_layer();
        let click_through = layer_like.click_through;
        if click_through {
            match Region::new(&self.compositor) {
                Ok(region) => wl.set_input_region(Some(region.wl_region())),
                Err(e) => log::warn!("{}: no input region: {e}", config.namespace),
            }
        }
        wl.commit();
        self.by_wl.insert(wl.id(), id);
        let surface = Surface {
            id,
            generation,
            node,
            kind: spec.kind,
            placement: Placement::Focused,
            monitor: monitor.clone(),
            output,
            requested_output: output,
            role: Role::Popup {
                popup,
                parent,
                config,
                grabbed: grab.is_some(),
                at: None,
                asked: Some(ask),
            },
            config: layer_like,
            viewport,
            fractional,
            configured: false,
            logical: (0, 0),
            scale,
            reported_scale: None,
            integer_scale,
            buffers: ShmBuffers::new(id, self.max_buffers),
            geometry_dirty: true,
            callback_pending: false,
            commit_seq: 0,
            in_flight: None,
            throttled_at: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            blur: None,
            blur_sent: Some(Vec::new()),
            pose: strand_scene::SurfacePose::IDENTITY,
            alpha: None,
            origin: None,
            zone_on: None,
            last_damage: Vec::new(),
            click_through,
            input_region: click_through.then_some(None),
            painted_input: None,
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        self.sync_popup_scrim(id, spec);
        let monitor = monitor.and_then(|m| self.monitors.get(&m)).cloned();
        self.host.surface_attached(id, node, monitor.as_ref());
        self.sync_popup_keyboard();
    }

    /// Gives popup `id` the scrim `spec` asks for (or takes it away, or
    /// recolours it): a scrim-only catcher on its root layer surface's
    /// output, on the layer [`popup_scrim_layer`] picks, which takes no
    /// clicks (the popup's grab closes it).
    pub(super) fn sync_popup_scrim(&mut self, id: SurfaceId, spec: &SurfaceSpec) {
        let want = wants_scrim(spec);
        match (want, self.under_of(id)) {
            (None, None) => {}
            (None, Some(_)) => self.destroy_catcher(id),
            (Some(c), Some(_)) => self.recolor_scrim(id, Some(c)),
            (Some(c), None) => {
                let Some(s) = self.surfaces.get(&id) else {
                    return;
                };
                let Some(root) = self.root_layer(id).and_then(|r| self.surfaces.get(&r)) else {
                    return;
                };
                let under = Under {
                    layer: popup_scrim_layer(&root.config),
                    namespace: s.config.namespace.clone(),
                    clicks: false,
                    scrim: Some(c),
                };
                let (node, output) = (s.node, root.output);
                self.create_catcher(id, node, &under, output);
            }
        }
    }

    /// The layer surface popup `id` is nested in (itself for a layer
    /// surface).
    pub(super) fn root_layer(&self, mut id: SurfaceId) -> Option<SurfaceId> {
        for _ in 0..64 {
            match &self.surfaces.get(&id)?.role {
                Role::Layer(_) | Role::Lock(_) => return Some(id),
                Role::Popup { parent, .. } => id = *parent,
            }
        }
        None
    }

    /// True if popup `id` is nested, at any depth, in surface `ancestor`.
    pub(super) fn is_nested_in(&self, mut id: SurfaceId, ancestor: SurfaceId) -> bool {
        for _ in 0..64 {
            match self.surfaces.get(&id).and_then(|s| s.role.popup_parent()) {
                Some(p) if p == ancestor => return true,
                Some(p) => id = p,
                None => return false,
            }
        }
        false
    }

    /// Makes layer surface `id` `exclusive` for a popup grab (`on`), or
    /// gives it back its own keyboard interactivity.
    pub(super) fn set_grab_keyboard(&mut self, id: SurfaceId, on: bool) {
        if on == self.grab_keyboard.contains(&id) {
            return;
        }
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        let Role::Layer(layer) = &s.role else {
            return;
        };
        log::trace!("{id:?}: grab keyboard {on}");
        let k = if on {
            self.grab_keyboard.insert(id);
            Keyboard::Exclusive
        } else {
            self.grab_keyboard.remove(&id);
            if s.config.keyboard == Keyboard::None && self.keyboard_focus == Some(id) {
                self.releasing.insert(id);
            }
            s.config.keyboard
        };
        layer.set_keyboard_interactivity(match k {
            Keyboard::None => KeyboardInteractivity::None,
            Keyboard::OnDemand => KeyboardInteractivity::OnDemand,
            Keyboard::Exclusive => KeyboardInteractivity::Exclusive,
        });
        // (M4) A handed-off surface's next present applies it: the GPU
        // thread is its only committer.
        if !self.gpu.has(id) {
            layer.commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
        }
    }

    /// The topmost grabbing popup nested in layer surface `layer`.
    pub(super) fn topmost_grab(&self, layer: SurfaceId) -> Option<SurfaceId> {
        let grabbing = |s: &Surface| s.role.grabbed();
        self.surfaces
            .values()
            .filter(|s| grabbing(s) && self.root_layer(s.id) == Some(layer))
            .find(|s| {
                !self
                    .surfaces
                    .values()
                    .any(|c| grabbing(c) && c.role.popup_parent() == Some(s.id))
            })
            .map(|s| s.id)
    }

    /// Keeps the keyboard with the grabbing popups. An `xdg_popup` grab
    /// asks for the keyboard, but compositors give it to the popup only
    /// through its parent's focus, and a layer surface with `keyboard:
    /// none` (a bar) never has focus: so while a grabbing popup is open
    /// its layer surface is `exclusive` (as an app's menu holds the
    /// keyboard), and keys arriving on it go to the topmost grabbing
    /// popup, which is told a `KeyboardEnter` of its own. Escape then
    /// closes the calendar of a `keyboard: none` bar.
    pub(super) fn sync_popup_keyboard(&mut self) {
        let layers: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| matches!(s.role, Role::Layer(_)))
            .map(|s| s.id)
            .collect();
        for id in layers {
            let want = self.topmost_grab(id).is_some();
            self.set_grab_keyboard(id, want);
        }
        self.grab_keyboard
            .retain(|id| self.surfaces.contains_key(id));
        let want = self
            .keyboard_focus
            .and_then(|f| self.root_layer(f))
            .and_then(|l| self.topmost_grab(l));
        if want == self.grab_focus {
            return;
        }
        // Keys move from the old target to the new one: each is told,
        // as the compositor would tell surfaces of their own focus (the
        // surface that has it already was told by the compositor). A
        // popup whose nested popup takes the keys is not told it lost
        // them: a leave closes an `open: <->` popup, and the nested one
        // with it.
        let old = std::mem::replace(&mut self.grab_focus, want);
        if let Some(o) = old
            && self.surfaces.contains_key(&o)
            && Some(o) != self.keyboard_focus
            && !want.is_some_and(|w| self.is_nested_in(w, o))
        {
            self.send_input(InputEvent::KeyboardLeave { surface: o });
        }
        match want {
            Some(p) => {
                if Some(p) != self.keyboard_focus {
                    self.send_input(InputEvent::KeyboardEnter { surface: p });
                }
            }
            // The grab ended: keys go back to the surface with focus.
            None => {
                if let Some(f) = self.keyboard_focus
                    && old != Some(f)
                    && self.surfaces.contains_key(&f)
                {
                    self.send_input(InputEvent::KeyboardEnter { surface: f });
                }
            }
        }
    }
}

impl<H: SurfaceHost + 'static> PopupHandler for State<H> {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        config: PopupConfigure,
    ) {
        let Some(id) = self.surface_for(popup.wl_surface()) else {
            return;
        };
        let Some(Role::Popup {
            config: c, asked, ..
        }) = self.surfaces.get(&id).map(|s| &s.role)
        else {
            return;
        };
        // (owner-testing.md bug 6) The compositor put it elsewhere than
        // the positioner asked (it slid or flipped it): a pixel of
        // rounding aside, something the manager did not know.
        let moved = asked.is_some_and(|(x, y)| {
            (config.position.0 - x).abs() > 1 || (config.position.1 - y).abs() > 1
        });
        // The compositor sizes the box (the window geometry); the buffer
        // adds the shadow overhang.
        let [t, r, b, l] = c.overhang;
        let w = if config.width > 0 {
            config.width as u32
        } else {
            c.width
        };
        let h = if config.height > 0 {
            config.height as u32
        } else {
            c.height
        };
        let w = w.saturating_add((l + r).max(0) as u32);
        let h = h.saturating_add((t + b).max(0) as u32);
        log::debug!(
            "{}: popup configured at {:?}, {}×{}",
            c.namespace,
            config.position,
            config.width,
            config.height
        );
        if moved {
            self.stats.popups_moved += 1;
        }
        self.configured(id, (w, h));
        self.place_popup(id, config.position);
        // sctk acked it already; the next commit makes it take effect.
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.geometry_dirty = true;
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        // Escape or a click away (the grab ended): its `open` goes false
        // through the router, and the surface goes now (the compositor
        // unmapped it already).
        let Some(id) = self.surface_for(popup.wl_surface()) else {
            return;
        };
        self.dismiss_popup(id);
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Ends popup `id` as a click away does: told to the host while the
    /// surface is still known (it routes by surface), then destroyed with
    /// the popups nested in it; not made again until its spec has closed.
    pub(super) fn dismiss_popup(&mut self, id: SurfaceId) {
        self.stats.closed += 1;
        self.send_input(InputEvent::ClickAway { surface: id });
        if let Some(node) = self.surfaces.get(&id).map(|s| s.node) {
            self.dismissed.insert(node);
        }
        self.destroy_surface(id);
    }

    /// Before a grabbing popup opens in `parent`: the grabbing popups
    /// that are not `parent` or one of its ancestors are dismissed first.
    /// xdg-shell wants a grabbing popup to be the topmost one, nested in
    /// the topmost grabbing popup or in a toplevel/layer surface; opening
    /// the volume menu while the calendar is shown closes the calendar,
    /// as a click away would.
    pub(super) fn dismiss_other_grabs(&mut self, parent: SurfaceId) {
        let mut chain = BTreeSet::new();
        let mut at = Some(parent);
        while let Some(id) = at {
            if !chain.insert(id) {
                break;
            }
            at = self.surfaces.get(&id).and_then(|s| s.role.popup_parent());
        }
        // Outermost first: dismissing one takes the popups nested in it.
        let others: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| !chain.contains(&s.id))
            .filter(|s| s.role.grabbed())
            .filter(|s| {
                s.role
                    .popup_parent()
                    .and_then(|p| self.surfaces.get(&p))
                    .is_none_or(|p| !p.role.grabbed())
            })
            .map(|s| s.id)
            .collect();
        for id in others {
            if self.surfaces.contains_key(&id) {
                self.dismiss_popup(id);
            }
        }
    }
}

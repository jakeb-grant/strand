//! Frames: geometry, paint and commit, frame callbacks, deadlines and
//! presentation feedback.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    // ---- geometry ---------------------------------------------------------

    /// Re-derives the buffer size from the latest configure and scale, right
    /// before a paint, so all the events of one wakeup (a configure plus a
    /// `preferred_scale`) make one `surface_configured` and one resize.
    /// Returns false if the surface is gone or not configured.
    pub(super) fn update_geometry(&mut self, id: SurfaceId) -> bool {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return false;
        };
        if !s.configured {
            return false;
        }
        let size = s.buffer_size();
        let scale = s.effective_scale();
        s.scale = scale;
        if size == s.buffers.size() && s.reported_scale == Some(scale) {
            return true;
        }
        s.reported_scale = Some(scale);
        s.buffers.resize(size);
        s.geometry_dirty = true;
        s.repaint = true;
        self.host.surface_configured(id, size, scale);
        true
    }

    // ---- painting ----------------------------------------------------------

    pub(super) fn draw(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !s.configured {
            return;
        }
        if s.throttled() {
            if s.geometry_changed() {
                // A new size or scale supersedes the frame in flight: paint
                // now, so a surface the compositor does not present (an
                // occluded bar, an output in DPMS off) still applies its
                // configure. The old frame's feedback comes back discarded
                // and no longer matches `in_flight`; a late frame callback
                // only marks the surface again.
                s.in_flight = None;
                s.callback_pending = false;
            } else {
                // The frame in flight's callback or presentation marks it
                // again; whatever changed meanwhile is painted then, once.
                // A same-size configure's ack is committed then too (a
                // bare commit now would only discard the frame's
                // presentation feedback).
                s.stats.throttled += 1;
                self.stats.throttled += 1;
                return;
            }
        }
        if !self.update_geometry(id) {
            return;
        }
        let wants = self.host.wants_frame(id);
        if !wants && let Some(at) = self.host.frame_deadline(id) {
            // The painter holds this frame (text it would show is still
            // being shaped): ask again at its deadline, or sooner
            // when new content marks the surface. The paint stays owed.
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.repaint = true;
            }
            self.commit_ack(id);
            self.arm_deadline(id, at);
            return;
        }
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !(s.repaint || wants) {
            // A configure that needs no new frame (margins, exclusive zone)
            // still takes effect only with a commit.
            self.commit_ack(id);
            return;
        }
        // Painting now: a deadline armed for an earlier, held or empty
        // paint would only wake the loop for nothing.
        self.cancel_deadline(id);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        let acquired = match s.buffers.acquire(&self.shm, &self.qh) {
            Ok(Some(a)) => a,
            Ok(None) => {
                // Every buffer is with the compositor: the next release
                // brings us back.
                s.repaint = true;
                return;
            }
            Err(e) => {
                log::error!("{}: {e}", s.config.namespace);
                s.repaint = false;
                return;
            }
        };
        s.repaint = false;
        let size = s.buffers.size();
        let stride = s.buffers.stride();
        let scale = s.scale;
        let time = self.clock.predict(id);
        let Some(pixels) = s.buffers.pixels(acquired.index) else {
            log::error!(
                "{}: buffer {} has no memory",
                s.config.namespace,
                acquired.index
            );
            return;
        };
        let damage = match PaintTarget::new(pixels, size, stride, scale, acquired.age) {
            Ok(target) => {
                let mut target = target.at(time);
                self.host.paint(id, &mut target)
            }
            Err(e) => {
                log::error!("{}: {e}", s.config.namespace);
                return;
            }
        };
        s.stats.paints += 1;
        self.stats.paints += 1;
        let wants_more = self.host.wants_frame(id);
        // What this frame asks the compositor to blur, sent with its
        // commit when it changed (the blur ladder's first rung).
        let blur = self
            .blurs()
            .then(|| crate::blur::region_rects(&self.host.blur_region(id), scale));
        // A compositor pose (M4) rides this frame's commit, or a bare one
        // when it drew nothing.
        let posed = self.sync_pose(id);
        // So does a new opaque region: a delegated fade that settles
        // claims its region with the bare commit that ends it.
        let opaqued = self.sync_opaque(id, scale);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        let wl = s.wl().clone();
        if damage.is_empty() {
            if posed || opaqued {
                s.ack_pending = true;
            }
            // Nothing drawn, nothing recorded: the buffer keeps its age.
            s.stats.empty_paints += 1;
            self.stats.empty_paints += 1;
            let mapped = s.mapped();
            if !mapped {
                // No buffer yet: the first frame is still owed.
                s.repaint = true;
            }
            if let Some(at) = self.host.frame_deadline(id) {
                // The painter held the frame after all (what it collected
                // while painting asks for a configure first): ask again
                // at its deadline.
                s.repaint = true;
                self.commit_ack(id);
                self.arm_deadline(id, at);
                return;
            }
            if !wants_more {
                self.commit_ack(id);
                return;
            }
            if !mapped {
                // The compositor sends no frame callbacks to an unmapped
                // surface: one would never come and would block every
                // later paint. Ask again after about a frame instead.
                self.commit_ack(id);
                self.arm_deadline(id, Instant::now() + UNMAPPED_RETRY);
            } else {
                wl.frame(&self.qh, FrameCallbackData(wl.clone()));
                s.callback_pending = true;
                s.ack_pending = false;
                wl.commit();
                s.stats.frame_requests += 1;
                s.stats.bare_commits += 1;
                self.stats.frame_requests += 1;
                self.stats.bare_commits += 1;
            }
            return;
        }
        let Some(buffer) = s.buffers.buffer(acquired.index).cloned() else {
            s.buffers.slots.invalidate(acquired.index);
            s.repaint = true;
            self.host.frame_dropped(id);
            return;
        };
        if s.geometry_dirty {
            s.geometry_dirty = false;
            if s.is_fractional() {
                wl.set_buffer_scale(1);
            } else {
                wl.set_buffer_scale(s.integer_scale.max(1));
            }
            // The logical size, times a pose's scale (`pose.rs`).
            s.send_destination();
            // A popup's window geometry is its box: the compositor
            // positions that, and its shadow reaches past it.
            if let Role::Popup { popup, config, .. } = &s.role {
                let [t, r, b, l] = config.overhang;
                let (w, h) = s.logical;
                popup.xdg_surface().set_window_geometry(
                    l,
                    t,
                    (w as i32 - l - r).max(1),
                    (h as i32 - t - b).max(1),
                );
            }
        }
        // A lock surface acks its configure only with the buffer at that
        // size (`session_lock.rs`).
        if let Role::Lock(l) = &mut s.role {
            l.ack();
        }
        wl.attach(Some(&buffer), 0, 0);
        if wl.version() >= 4 {
            for r in damage.rects() {
                wl.damage_buffer(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
            }
            s.last_damage = damage.rects().to_vec();
        } else {
            wl.damage(0, 0, i32::MAX, i32::MAX);
            s.last_damage = vec![Rect::new(0, 0, size.w, size.h)];
        }
        if let Some(rects) = blur {
            self.set_blur(id, rects);
        }
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        s.commit_seq += 1;
        if let Some(p) = &self.presentation {
            let tag = FeedbackTag {
                surface: id,
                generation: s.generation,
                seq: s.commit_seq,
            };
            p.feedback(&wl, &self.qh, tag);
        }
        // Throttle to the refresh rate: paint again only after this frame's
        // callback, or (when nothing is animating) its presentation, which
        // is requested anyway and costs no extra wakeup when idle.
        if wants_more || self.presentation.is_none() {
            wl.frame(&self.qh, FrameCallbackData(wl.clone()));
            s.callback_pending = true;
            s.stats.frame_requests += 1;
            self.stats.frame_requests += 1;
        } else {
            s.in_flight = Some(s.commit_seq);
        }
        s.ack_pending = false;
        wl.commit();
        s.buffers.slots.commit(acquired.index);
        s.stats.commits += 1;
        self.stats.commits += 1;
        if s.commit_seq == 1 {
            // Mapped: popups waiting for it as their parent come now.
            let node = s.node;
            self.reconcile_children(node);
        }
    }

    /// Sets `id`'s opaque region as pending state when the host's
    /// changed; true if one was sent (it takes effect with the next
    /// commit, with a buffer or bare).
    pub(super) fn sync_opaque(&mut self, id: SurfaceId, scale: Scale) -> bool {
        let opaque = scale.inner_logical_region(&self.host.opaque_region(id));
        let Some(s) = self.surfaces.get_mut(&id) else {
            return false;
        };
        if opaque == s.opaque {
            return false;
        }
        let wl = s.wl().clone();
        let sent = if opaque.is_empty() {
            wl.set_opaque_region(None);
            true
        } else {
            match Region::new(&self.compositor) {
                Ok(region) => {
                    for r in &opaque {
                        region.add(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
                    }
                    wl.set_opaque_region(Some(region.wl_region()));
                    true
                }
                Err(e) => {
                    log::warn!("{}: no opaque region: {e}", s.config.namespace);
                    false
                }
            }
        };
        // Unsent regions are retried with the next frame.
        if sent {
            s.opaque = opaque;
            s.stats.opaque_updates += 1;
            self.stats.opaque_updates += 1;
        }
        sent
    }

    /// Sends a bare commit if a configure was acked and nothing has
    /// committed since, so the ack takes effect.
    pub(super) fn commit_ack(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        // A lock surface takes no bare commit: its configure is acked
        // with its next buffer instead (`session_lock.rs`).
        if matches!(s.role, Role::Lock(_)) {
            return;
        }
        if s.ack_pending {
            s.ack_pending = false;
            s.wl().commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
        }
    }

    pub(super) fn cancel_deadline(&mut self, id: SurfaceId) {
        if let Some(t) = self.deadline_timers.remove(&id) {
            self.handle.remove(t);
        }
    }

    /// Marks `id` again at `at` (one timer per surface; re-arming replaces
    /// it).
    pub(super) fn arm_deadline(&mut self, id: SurfaceId, at: Instant) {
        self.cancel_deadline(id);
        let token = self.handle.insert_source(
            Timer::from_deadline(at),
            move |_, _, state: &mut State<H>| {
                state.deadline_timers.remove(&id);
                state.mark(id);
                TimeoutAction::Drop
            },
        );
        match token {
            Ok(t) => {
                self.deadline_timers.insert(id, t);
            }
            Err(e) => log::warn!("cannot arm a frame deadline: {}", e.error),
        }
    }

    /// Presentation feedback for commit `seq` of `surface` arrived (or was
    /// discarded): the frame is no longer in flight.
    pub(super) fn frame_settled(&mut self, tag: &FeedbackTag) -> bool {
        let Some(s) = self.surfaces.get_mut(&tag.surface) else {
            return false;
        };
        if s.generation != tag.generation {
            return false;
        }
        if s.in_flight == Some(tag.seq) {
            s.in_flight = None;
            self.mark(tag.surface);
        }
        true
    }
}

impl<H: SurfaceHost + 'static> CompositorHandler for State<H> {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.integer_scale = new_factor.max(1);
        }
        // Resolved with the next paint (see `update_geometry`).
        self.mark(id);
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        // Buffers stay untransformed; the compositor rotates them.
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        self.stats.frames_done += 1;
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.callback_pending = false;
            s.stats.frames_done += 1;
        }
        self.mark(id);
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // A `screens: focused` surface learns where the compositor put it.
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        let Some(global) = self.output_globals.get(&output.id()).copied() else {
            return;
        };
        let Some(monitor) = self
            .monitors
            .id_of(global)
            .and_then(|m| self.monitors.get(m))
            .cloned()
        else {
            return;
        };
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.placement != Placement::Focused || s.monitor.as_ref() == Some(&monitor.id) {
            return;
        }
        s.monitor = Some(monitor.id.clone());
        s.output = Some(global);
        let (node, config) = (s.node, s.config.clone());
        // Its catcher went where the compositor put it too: the other
        // outputs get theirs now.
        if let Some(list) = self.catchers.get_mut(&id)
            && list.len() == 1
            && list[0].output.is_none()
        {
            list[0].output = Some(global);
            let under = Under {
                layer: config.layer,
                namespace: config.namespace.clone(),
                clicks: list[0].clicks,
                scrim: None,
            };
            self.add_secondary_catchers(id, node, &under, global);
        }
        self.host.surface_entered(id, &monitor);
        self.place_layer(id);
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for BufferData {
    fn event(
        &self,
        state: &mut State<H>,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.stats.releases += 1;
            let Some(s) = state.surfaces.get_mut(&self.surface) else {
                return;
            };
            s.stats.releases += 1;
            if s.buffers.release(self) && s.repaint {
                state.mark(self.surface);
            }
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpPresentationFeedback, State<H>> for FeedbackTag {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                flags: _,
            } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let presentation = Presentation {
                    time: Duration::new(secs, tv_nsec.min(999_999_999)),
                    refresh: (refresh > 0).then(|| Duration::from_nanos(u64::from(refresh))),
                    seq: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                };
                state.stats.presented += 1;
                if state.frame_settled(self) {
                    if let Some(s) = state.surfaces.get_mut(&self.surface) {
                        s.stats.presented += 1;
                    }
                    state.clock.presented(self.surface, presentation);
                }
            }
            wp_presentation_feedback::Event::Discarded => {
                state.stats.discarded += 1;
                if state.frame_settled(self) {
                    if let Some(s) = state.surfaces.get_mut(&self.surface) {
                        s.stats.discarded += 1;
                    }
                    state.clock.discarded(self.surface);
                }
            }
            _ => {}
        }
    }
}

//! (M4) The binary's side of the GPU (docs/architecture.md, "`strand-gpu`"):
//! the `Gpu` started when render asks for it and dropped when render says
//! so, its replies handed to render, promoted surfaces attached, handed
//! off to the WSI and taken back, presented surfaces' frames painted and
//! sent, and the status logic hears.
//!
//! [`GpuHost::pump`] runs on the main thread after every dispatch of the
//! surface manager's loop: render's requests and changes are made while
//! the manager calls in (a paint, `update`), and the GPU thread's replies
//! wake the loop through a ping. Nothing here waits on the GPU thread.

use strand_render::Renderer;
use strand_scene::GpuStatus;

use super::ToLogic;

/// Tells logic the GPU status when it changes while a frame shows
/// something only a GPU draws (logic logs each reason once and makes it
/// a `strand watch` notice), and once the GPU is turned off after
/// `LOST_CAP` lost devices, whatever the frame shows (m4-close-gpucap).
#[derive(Debug, Default)]
pub(crate) struct StatusForward {
    told: Option<GpuStatus>,
}

impl StatusForward {
    /// The status to send now, if any.
    pub(crate) fn check(&mut self, renderer: &Renderer) -> Option<GpuStatus> {
        if !renderer.gpu_in_demand() && !renderer.gpu_off() {
            return None;
        }
        let status = renderer.gpu_status();
        if self.told.as_ref() == Some(&status) {
            return None;
        }
        self.told = Some(status.clone());
        Some(status)
    }

    pub(crate) fn send(&mut self, renderer: &Renderer, tx: &calloop::channel::Sender<ToLogic>) {
        if let Some(s) = self.check(renderer) {
            let _ = tx.send(ToLogic::GpuStatus(s));
        }
    }
}

#[cfg(feature = "gpu")]
pub(crate) use host::GpuHost;

/// (gpu-first) The renderer asks for the device as the shell starts
/// (decisions.md gpu-first): the first pump starts the `Gpu` thread
/// (`BackendChange::Start`), which creates the device and builds its
/// pipelines off the main thread, and it is kept for the process, so an
/// animation is drawn on the GPU from its first large frame.
///
/// (gpu-first review) Where no device can exist (no render node, and no
/// software adapter accepted: `strand_gpu::device_possible`) nothing is
/// started or ever probed: the shell maps no Vulkan library and costs
/// what it did before gpu-first.
#[cfg(feature = "gpu")]
pub(crate) fn configure(renderer: &mut Renderer) {
    if strand_gpu::device_possible(&strand_gpu::GpuOptions::from_env()) {
        renderer.warm_gpu();
    } else {
        renderer.gpu_absent("no GPU render node (/dev/dri/renderD*)");
    }
}

#[cfg(feature = "gpu")]
mod host {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use strand_gpu::{Gpu, GpuMode, GpuOptions, GpuReply, GpuRequest};
    use strand_render::Renderer;
    use strand_scene::{BackendChange, Damage, Scale, Size, SurfaceId};
    use strand_surface::State;

    use crate::demo::host::Host;

    /// How often an ending GPU thread is checked for having ended.
    const JOIN_POLL: Duration = Duration::from_millis(50);

    /// How often [`GpuHost::shutdown`] looks again.
    const SHUTDOWN_POLL: Duration = Duration::from_millis(5);

    /// A surface the GPU thread presents.
    #[derive(Debug)]
    struct Presented {
        size: Size,
        scale: Scale,
        /// A frame was sent and its `Presented` has not come.
        in_flight: bool,
        /// The sent frame's size and scale, for its `damage` log line
        /// once presented.
        sent: Option<(Size, Scale)>,
        /// When a paint last sent nothing (decisions.md presented-quiet).
        drew_nothing: Option<std::time::Instant>,
    }

    /// A surface attached (asked or answered).
    #[derive(Debug, Default)]
    struct Attachment {
        /// How it draws, once answered.
        mode: Option<GpuMode>,
        /// Its `Attach` carried the manager's handles
        /// (`State::raw_handles`): the manager keeps its `wl_surface`
        /// alive until `State::take_back`.
        lent: bool,
        /// `Release` was sent: no frame goes after it (the thread would
        /// answer it for a surface no longer attached), and an `Attached`
        /// still to come hands nothing off.
        releasing: bool,
    }

    /// The `Gpu` and what the main thread knows of the surfaces it draws.
    pub(crate) struct GpuHost {
        gpu: Option<Gpu>,
        /// Dropped `Gpu`s whose threads are still dropping their device:
        /// joined once they have ended, never waited for; the surfaces
        /// whose handles they held are taken back then (their swapchains
        /// are gone).
        exiting: Vec<(Gpu, Vec<SurfaceId>)>,
        /// Requests for a `Gpu` not started yet: one starts only once
        /// every ending thread has ended (one device per process).
        queued: Vec<GpuRequest>,
        /// A warm start asked for while a thread was ending.
        start_queued: bool,
        opts: GpuOptions,
        /// Wakes the main loop after every reply.
        ping: calloop::ping::Ping,
        attached: BTreeMap<SurfaceId, Attachment>,
        presented: BTreeMap<SurfaceId, Presented>,
    }

    impl std::fmt::Debug for GpuHost {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GpuHost")
                .field("running", &self.gpu.is_some())
                .field("exiting", &self.exiting.len())
                .field("queued", &self.queued.len())
                .field("attached", &self.attached)
                .field("presented", &self.presented)
                .finish()
        }
    }

    impl GpuHost {
        /// `ping` is the main loop's GPU ping (its source does nothing:
        /// the loop pumps after every dispatch).
        pub(crate) fn new(ping: calloop::ping::Ping) -> Self {
            Self {
                gpu: None,
                exiting: Vec::new(),
                queued: Vec::new(),
                start_queued: false,
                opts: GpuOptions::from_env(),
                ping,
                attached: BTreeMap::new(),
                presented: BTreeMap::new(),
            }
        }

        /// Sends `r` to the running `Gpu`, or starts one, or, while a
        /// dropped thread is still ending, queues it for the next.
        fn send(&mut self, r: GpuRequest) {
            match send_route(self.gpu.is_some(), !self.exiting.is_empty()) {
                SendRoute::Send => {
                    if let Some(g) = &self.gpu {
                        g.send(r);
                    }
                }
                SendRoute::Spawn => self.spawn().send(r),
                SendRoute::Queue => self.queued.push(r),
            }
        }

        fn spawn(&mut self) -> &Gpu {
            let ping = self.ping.clone();
            self.gpu
                .insert(Gpu::spawn(Box::new(move || ping.ping()), self.opts))
        }

        /// Starts a `Gpu` with nothing to send it (the warm start), unless
        /// one runs; while a thread is still ending, once it has ended.
        fn start(&mut self) {
            match send_route(self.gpu.is_some(), !self.exiting.is_empty()) {
                SendRoute::Send => {}
                SendRoute::Spawn => {
                    self.spawn();
                }
                SendRoute::Queue => self.start_queued = true,
            }
        }

        /// A thread runs or will once the ending ones have ended.
        fn running(&self) -> bool {
            self.gpu.is_some() || !self.queued.is_empty() || self.start_queued
        }

        /// Asks the thread to let go of `id` (once).
        fn release(&mut self, id: SurfaceId) {
            if let Some(a) = self.attached.get_mut(&id) {
                if a.releasing {
                    return;
                }
                a.releasing = true;
            }
            self.send(GpuRequest::Release(id));
        }

        /// Everything owed since the last dispatch: replies to render,
        /// render's changes and requests to the GPU thread, frames of
        /// presented surfaces.
        pub(crate) fn pump(&mut self, state: &mut State<Host>) {
            // Replies first: they can attach, hand off and take back.
            if self.replies(state) {
                // A pass's pixels (or a readback's, a failure) dirtied a
                // node: render runs its update and its surfaces are asked
                // for frames, as after the text worker's layouts. The
                // GPU ping itself does nothing, so without this a CPU
                // surface waiting on a pass repainted only when the
                // renderer's timer happened to fire.
                crate::demo::text_ready(state);
            }
            for id in std::mem::take(&mut state.host_mut().gpu_released) {
                let held = self.exiting.iter().map(|(_, held)| held.as_slice());
                match release_route(id, self.running(), held) {
                    Release::Send => self.release(id),
                    // Taken back (and so destroyed) once that thread has
                    // ended: its swapchain goes first.
                    Release::Defer => {}
                    Release::TakeBack => state.take_back(id),
                }
            }
            for change in state.host_mut().renderer.take_backend_changes() {
                self.change(state, change);
            }
            for r in state.host_mut().renderer.take_gpu_requests() {
                self.send(r);
            }
            self.present(state);
            let mut back = Vec::new();
            let mut ended = false;
            self.exiting.retain_mut(|(g, surfaces)| {
                let done = g.try_join();
                ended |= done;
                if done {
                    log::info!("GPU: the thread ended");
                    back.append(surfaces);
                }
                !done
            });
            for id in back {
                state.take_back(id);
            }
            if ended {
                release_freed();
            }
            // The last ending thread has ended: the next one starts.
            if self.gpu.is_none() && self.exiting.is_empty() {
                if std::mem::take(&mut self.start_queued) {
                    self.start();
                }
                for r in std::mem::take(&mut self.queued) {
                    self.send(r);
                }
            }
        }

        /// Ends every GPU thread, the running one and those still ending,
        /// waiting at most `within`; true once all have ended. Called
        /// before the surface manager drops: a presenting thread holds
        /// the display and the `wl_surface`s it was lent
        /// (`strand_gpu::RawHandles`), and drops its swapchains on them
        /// as it ends, so none may outlive the connection.
        pub(crate) fn shutdown(&mut self, within: Duration) -> bool {
            self.queued.clear();
            self.start_queued = false;
            if let Some(g) = self.gpu.take() {
                self.exiting.push((g, Vec::new()));
            }
            let until = std::time::Instant::now() + within;
            loop {
                self.exiting.retain_mut(|(g, _)| !g.try_join());
                if self.exiting.is_empty() {
                    return true;
                }
                let now = std::time::Instant::now();
                if now >= until {
                    return false;
                }
                std::thread::sleep(SHUTDOWN_POLL.min(until - now));
            }
        }

        /// How soon the loop must pump again with nothing else waking
        /// it: a thread that is ending is polled until it has ended (its
        /// last ping can come before it has); a presented surface whose
        /// next frame is held (a size or query hold, new text, a pass in
        /// flight: `Renderer::frame_deadline`) is painted at the hold's
        /// end.
        ///
        /// (gpu-first review) The surface manager arms that deadline for
        /// the surfaces it paints, never for one handed off to the GPU
        /// thread, so a held frame of a presented surface waited for
        /// whatever woke the loop next: the launcher's mode switches that
        /// change its size froze for about 0.5 s (the allocator's trim
        /// timer) on the GPU, with every frame of the morph after it late.
        pub(crate) fn wait(&self, renderer: &Renderer) -> Option<Duration> {
            let poll = (!self.exiting.is_empty()).then_some(JOIN_POLL);
            let now = std::time::Instant::now();
            let held = self
                .presented
                .iter()
                .filter(|(_, p)| !p.in_flight)
                .filter_map(|(id, _)| renderer.frame_deadline(*id))
                .min()
                .map(|t| t.saturating_duration_since(now));
            [poll, held].into_iter().flatten().min()
        }

        /// Delivers the thread's replies to render; true when one may
        /// have changed what a surface shows (anything but a
        /// `Presented`).
        fn replies(&mut self, state: &mut State<Host>) -> bool {
            let mut changed = false;
            while let Some(reply) = self.gpu.as_mut().and_then(Gpu::try_recv) {
                match &reply {
                    GpuReply::Attached { surface, mode } => {
                        let id = *surface;
                        let presenting = *mode == GpuMode::Present;
                        match on_attached(self.attached.get(&id), presenting) {
                            OnAttached::Release => {
                                // Not asked for by this host: let go of it.
                                if let Some(g) = &self.gpu {
                                    g.send(GpuRequest::Release(id));
                                }
                                continue;
                            }
                            // Its `Released` follows and takes it back.
                            OnAttached::Wait => continue,
                            OnAttached::Unlend => {
                                // Read back: the thread made no swapchain,
                                // so the manager need not keep the surface
                                // for it (one destroyed meanwhile goes).
                                state.take_back(id);
                            }
                            OnAttached::HandOff => {
                                if !state.hand_off(id) {
                                    // Destroyed meanwhile (kept for the
                                    // swapchain: `Released` destroys it).
                                    self.release(id);
                                    continue;
                                }
                            }
                            OnAttached::Keep => {}
                        }
                        if let Some(a) = self.attached.get_mut(&id) {
                            a.mode = Some(*mode);
                            a.lent &= presenting;
                        }
                        log::info!("GPU: surface {} attached ({mode:?})", id.0);
                        if presenting {
                            let (size, scale) = geometry(state, id);
                            self.presented.insert(
                                id,
                                Presented {
                                    size,
                                    scale,
                                    in_flight: false,
                                    sent: None,
                                    drew_nothing: None,
                                },
                            );
                        }
                    }
                    GpuReply::Ready(info) => {
                        log::info!("GPU: the device is up ({}, {})", info.name, info.driver);
                    }
                    GpuReply::Released(id) => {
                        let id = *id;
                        log::info!("GPU: surface {} released", id.0);
                        self.attached.remove(&id);
                        self.presented.remove(&id);
                        state.take_back(id);
                    }
                    GpuReply::Presented { surface, .. } => {
                        if let Some(p) = self.presented.get_mut(surface) {
                            p.in_flight = false;
                            // Shown now: its `damage` line, as a CPU
                            // frame's at its commit (a presented frame
                            // is whole).
                            if let Some((size, scale)) = p.sent.take() {
                                state.host().log_frame(
                                    *surface,
                                    &Damage::full(size),
                                    size,
                                    scale,
                                    0,
                                );
                            }
                        }
                    }
                    GpuReply::Failed {
                        surface: Some(surface),
                        ..
                    } => {
                        // A frame that failed is answered too: render
                        // takes the surface back if it cannot be drawn.
                        if let Some(p) = self.presented.get_mut(surface) {
                            p.in_flight = false;
                            p.sent = None;
                        }
                    }
                    GpuReply::Exited => {
                        // Every swapchain went with the thread: the
                        // manager commits its surfaces again and no
                        // longer keeps the lent ones for it.
                        self.presented.clear();
                        for id in lent(&std::mem::take(&mut self.attached)) {
                            state.take_back(id);
                        }
                        if let Some(g) = self.gpu.take() {
                            self.exiting.push((g, Vec::new()));
                        }
                    }
                    _ => {}
                }
                changed |= !matches!(reply, GpuReply::Presented { .. });
                state.host_mut().renderer.deliver_gpu(reply);
                state.host_mut().log_gpu_frames();
            }
            changed
        }

        fn change(&mut self, state: &mut State<Host>, change: BackendChange) {
            match change {
                BackendChange::Promote(id) => {
                    if self.attached.contains_key(&id) {
                        return;
                    }
                    // A surface an ending thread may still present
                    // gets no second swapchain: it is read back.
                    let ending = self.exiting.iter().any(|(_, held)| held.contains(&id));
                    let handles =
                        state
                            .raw_handles(id)
                            .filter(|_| !ending)
                            .map(|h| strand_gpu::RawHandles {
                                display: h.display,
                                window: h.window,
                            });
                    let (size, scale) = geometry(state, id);
                    self.attached.insert(
                        id,
                        Attachment {
                            lent: handles.is_some(),
                            ..Attachment::default()
                        },
                    );
                    self.send(GpuRequest::Attach {
                        surface: id,
                        handles,
                        size,
                        scale,
                        opaque: false,
                    });
                }
                BackendChange::Demote(id) => {
                    if self.attached.contains_key(&id) && self.running() {
                        // Its swapchain goes first; `Released` takes the
                        // surface back.
                        self.release(id);
                    }
                }
                BackendChange::Start => self.start(),
            }
        }

        /// Frames for presented surfaces that want one and have none in
        /// flight; a new size first.
        fn present(&mut self, state: &mut State<Host>) {
            let Some(gpu) = &self.gpu else {
                return;
            };
            let now = monotonic();
            for (id, p) in &mut self.presented {
                let releasing = self.attached.get(id).is_none_or(|a| a.releasing);
                let (size, scale) = geometry(state, *id);
                if (size, scale) != (p.size, p.scale) {
                    p.size = size;
                    p.scale = scale;
                    gpu.send(GpuRequest::Resize {
                        surface: *id,
                        size,
                        scale,
                    });
                }
                if p.in_flight || releasing {
                    continue;
                }
                let r: &mut Renderer = &mut state.host_mut().renderer;
                if r.presented_due(*id).is_none() {
                    continue;
                }
                if let Some(frame) = r.paint_gpu(*id, now) {
                    let (size, scale) = (frame.size, frame.scale);
                    // What follows a CPU frame follows this one too:
                    // its regions, as pending state the present commits
                    // (decisions.md gpu-regions), set before the thread
                    // can present it; layout facts and list windows to
                    // logic, fed nodes (its `damage` line waits for
                    // `Presented`).
                    state.sync_presented(*id);
                    gpu.send(GpuRequest::Frame(frame));
                    p.in_flight = true;
                    p.sent = Some((size, scale));
                    p.drew_nothing = None;
                    state.host_mut().painted(*id, &Damage::full(size));
                } else {
                    // (presented-quiet) It painted and drew nothing, so
                    // nothing is sent and no `Presented` comes. All the
                    // same that paint may have ended an exit or let a
                    // shrink go, and moved hit boxes: what follows a
                    // CPU's empty paint follows it.
                    log::debug!("surface {}: a paint that sent nothing", id.0);
                    p.drew_nothing = Some(std::time::Instant::now());
                    state.sync_presented(*id);
                    state.host_mut().painted(*id, &Damage::new());
                }
            }
        }
    }

    /// Where a request goes.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum SendRoute {
        /// To the running thread.
        Send,
        /// To a thread started for it.
        Spawn,
        /// Kept until the ending threads have ended: a second device
        /// (and, under lavapipe, a second copy of LLVM's state) would
        /// live beside the one still being dropped.
        Queue,
    }

    fn send_route(running: bool, ending: bool) -> SendRoute {
        match (running, ending) {
            (true, _) => SendRoute::Send,
            (false, false) => SendRoute::Spawn,
            (false, true) => SendRoute::Queue,
        }
    }

    /// What an `Attached` reply does.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum OnAttached {
        /// Not attached by this host: released at once.
        Release,
        /// `Release` was sent after its `Attach`: nothing until `Released`.
        Wait,
        /// Read back although its handles were lent: given back to the
        /// manager.
        Unlend,
        /// Presented: handed off.
        HandOff,
        /// Read back, nothing lent.
        Keep,
    }

    fn on_attached(a: Option<&Attachment>, presenting: bool) -> OnAttached {
        match a {
            None => OnAttached::Release,
            Some(a) if a.releasing => OnAttached::Wait,
            Some(_) if presenting => OnAttached::HandOff,
            Some(a) if a.lent => OnAttached::Unlend,
            Some(_) => OnAttached::Keep,
        }
    }

    /// The surfaces whose handles a thread was given: the manager keeps
    /// them until that thread has let go of them.
    fn lent(attached: &BTreeMap<SurfaceId, Attachment>) -> Vec<SurfaceId> {
        attached
            .iter()
            .filter(|(_, a)| a.lent)
            .map(|(id, _)| *id)
            .collect()
    }

    /// What to do with a surface the manager must destroy while handed
    /// off (`SurfaceHost::gpu_release`).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Release {
        /// Ask the running thread; its `Released` takes it back.
        Send,
        /// A dropped thread still ending may own its swapchain: it is
        /// taken back when that thread has ended.
        Defer,
        /// No thread can hold it: take it back now.
        TakeBack,
    }

    /// The route for `id`'s release: `running` if a `Gpu` runs, `held`
    /// the surfaces each ending thread presented when it was dropped.
    /// The swapchain always goes before its `wl_surface`.
    fn release_route<'a>(
        id: SurfaceId,
        running: bool,
        mut held: impl Iterator<Item = &'a [SurfaceId]>,
    ) -> Release {
        if held.any(|h| h.contains(&id)) {
            Release::Defer
        } else if running {
            Release::Send
        } else {
            Release::TakeBack
        }
    }

    /// The GPU thread ended: what the driver freed goes back to the
    /// system. The Vulkan driver (and LLVM, under lavapipe) allocate with
    /// libc's malloc, whose per-thread arena keeps freed pages until
    /// trimmed: about 4 MiB more after every cycle, measured in
    /// `tests/gpu_idle.rs`. Strand's own allocations are mimalloc's.
    fn release_freed() {
        // SAFETY: `malloc_trim` takes no pointers and is thread-safe.
        unsafe {
            libc::malloc_trim(0);
        }
        super::super::trim();
    }

    /// A surface's buffer size and scale as the manager last resolved them.
    fn geometry(state: &State<Host>, id: SurfaceId) -> (Size, Scale) {
        state
            .surface(id)
            .map_or((Size::default(), Scale::default()), |s| {
                (s.buffer_size, s.scale)
            })
    }

    /// Now on the presentation clock's default (`CLOCK_MONOTONIC`), as
    /// `PaintTarget::time` is.
    fn monotonic() -> Duration {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        Duration::new(
            u64::try_from(t.tv_sec).unwrap_or(0),
            u32::try_from(t.tv_nsec).unwrap_or(0),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A surface lent to the thread (its handles in `Attach`) stays
        /// the manager's to keep alive until the thread lets go of it: a
        /// readback answer gives it back, a release sent before the answer
        /// waits for `Released`, and a dropped thread holds every lent
        /// surface, presented or not answered yet.
        #[test]
        fn a_lent_surface_is_kept_until_the_thread_lets_go() {
            let lent_to = |releasing| Attachment {
                mode: None,
                lent: true,
                releasing,
            };
            assert_eq!(on_attached(None, true), OnAttached::Release);
            assert_eq!(on_attached(Some(&lent_to(true)), true), OnAttached::Wait);
            assert_eq!(on_attached(Some(&lent_to(true)), false), OnAttached::Wait);
            assert_eq!(
                on_attached(Some(&lent_to(false)), true),
                OnAttached::HandOff
            );
            assert_eq!(
                on_attached(Some(&lent_to(false)), false),
                OnAttached::Unlend
            );
            assert_eq!(
                on_attached(Some(&Attachment::default()), false),
                OnAttached::Keep
            );
            let mut attached = BTreeMap::new();
            // Asked, not answered: the thread may be building its
            // swapchain.
            attached.insert(SurfaceId(1), lent_to(false));
            // Presented.
            attached.insert(
                SurfaceId(2),
                Attachment {
                    mode: Some(GpuMode::Present),
                    ..lent_to(false)
                },
            );
            // Read back without handles.
            attached.insert(
                SurfaceId(3),
                Attachment {
                    mode: Some(GpuMode::Readback),
                    ..Attachment::default()
                },
            );
            assert_eq!(lent(&attached), [SurfaceId(1), SurfaceId(2)]);
        }

        /// (gpu-first review) A presented surface whose next frame is held
        /// (here a size hold) wakes the loop at the hold's end: the
        /// surface manager arms that deadline only for surfaces it paints.
        /// One with a frame in flight waits for its `Presented`; one the
        /// manager paints is the manager's.
        #[test]
        fn a_held_presented_frame_wakes_the_loop_at_its_hold() {
            use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneDiff};
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            let font = std::fs::read(strand_text::test_font_path()).unwrap();
            let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
                std::sync::Arc::new(font),
            ]));
            let mut r = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
            r.set_resize_wait(Duration::from_secs(30));
            // A content-sized panel whose text grows.
            let (p, t) = (NodeId::new(1, 0), NodeId::new(2, 0));
            let mut d = SceneDiff::new();
            d.create(p, NodeKind::Panel, None, u32::MAX);
            d.set(
                p,
                Prop::Font,
                PropValue::Font(strand_scene::Font {
                    family: strand_text::TEST_FONT_FAMILY.into(),
                    size: 13.0,
                    weight: 400,
                }),
            );
            d.create(t, NodeKind::Text, Some(p), u32::MAX);
            d.set(t, Prop::Text, PropValue::Text("short".into()));
            r.apply(d);
            let spec = r.surface_spec(p).cloned().unwrap();
            let size = Size::new(spec.width.unwrap() as u32, spec.height.unwrap() as u32);
            let s = SurfaceId(5);
            r.attach_surface(s, p);
            r.configure_surface(s, size, Scale::ONE);
            let mut d = SceneDiff::new();
            d.set(
                t,
                Prop::Text,
                PropValue::Text("a much longer line of text".into()),
            );
            r.apply(d);
            assert!(r.frame_deadline(s).is_some(), "held for its configure");
            assert_eq!(host.wait(&r), None, "not presented: the manager's");
            host.presented.insert(
                s,
                Presented {
                    size,
                    scale: Scale::ONE,
                    in_flight: false,
                    sent: None,
                    drew_nothing: None,
                },
            );
            let w = host.wait(&r).expect("woken at the hold's end");
            assert!(
                w > Duration::from_secs(25) && w <= Duration::from_secs(30),
                "{w:?}"
            );
            if let Some(pr) = host.presented.get_mut(&s) {
                pr.in_flight = true;
            }
            assert_eq!(host.wait(&r), None, "in flight: its `Presented` wakes it");
        }

        /// The run's exit ends the GPU thread before the surface manager
        /// (and so the display its swapchains were made on) drops.
        #[test]
        fn shutdown_joins_the_running_thread() {
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            host.send(GpuRequest::Release(SurfaceId(1)));
            assert!(host.gpu.is_some(), "the request started a thread");
            assert!(host.shutdown(Duration::from_secs(30)));
            assert!(host.gpu.is_none() && host.exiting.is_empty());
            // Nothing left: at once.
            assert!(host.shutdown(Duration::ZERO));
        }

        /// One device per process: a thread is started only when no
        /// dropped one is still ending.
        #[test]
        fn no_thread_starts_while_another_is_ending() {
            assert_eq!(send_route(true, false), SendRoute::Send);
            assert_eq!(send_route(true, true), SendRoute::Send);
            assert_eq!(send_route(false, false), SendRoute::Spawn);
            assert_eq!(send_route(false, true), SendRoute::Queue);
        }

        #[test]
        fn a_release_waits_for_the_ending_thread_that_holds_the_surface() {
            let (a, b) = (SurfaceId(1), SurfaceId(2));
            let ending = [vec![a]];
            let held = || ending.iter().map(Vec::as_slice);
            // Held by a dropped thread still ending: deferred, with or
            // without a new thread running.
            assert_eq!(release_route(a, false, held()), Release::Defer);
            assert_eq!(release_route(a, true, held()), Release::Defer);
            // Not held: the running thread answers, or nothing holds it.
            assert_eq!(release_route(b, true, held()), Release::Send);
            assert_eq!(release_route(b, false, held()), Release::TakeBack);
            assert_eq!(
                release_route(a, false, std::iter::empty()),
                Release::TakeBack
            );
        }
    }
}

#[cfg(all(test, feature = "gpu"))]
mod status_tests {
    use super::*;
    use strand_gpu::{GpuError, GpuErrorKind, GpuReply};

    /// (m4-close-gpucap) With nothing on screen that only a GPU draws, a
    /// lost device's status is not told (as before), except the one that
    /// turns the GPU off: it is told once, at the third loss, and never
    /// again (logic makes it a WARN and a `strand watch` notice).
    #[test]
    fn the_gpu_turned_off_is_told_once_whatever_the_frame_shows() {
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let mut r = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let mut fwd = StatusForward::default();
        let lose = |r: &mut Renderer| {
            r.deliver_gpu(GpuReply::Lost(GpuError {
                kind: GpuErrorKind::Lost,
                message: "the GPU device was lost (test)".into(),
            }));
            r.deliver_gpu(GpuReply::Exited);
        };
        assert!(!r.gpu_in_demand());
        for _ in 1..strand_render::promote::LOST_CAP {
            lose(&mut r);
            assert_eq!(fwd.check(&r), None, "not off: not told");
        }
        lose(&mut r);
        assert_eq!(
            fwd.check(&r),
            Some(GpuStatus::Unavailable {
                reason: strand_render::promote::GPU_OFF.into()
            })
        );
        assert_eq!(fwd.check(&r), None, "told once");
        lose(&mut r);
        assert_eq!(fwd.check(&r), None, "a late loss changes nothing");
    }
}

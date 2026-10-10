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
/// a `strand watch` notice).
#[derive(Debug, Default)]
pub(crate) struct StatusForward {
    told: Option<GpuStatus>,
}

impl StatusForward {
    /// The status to send now, if any.
    pub(crate) fn check(&mut self, renderer: &Renderer) -> Option<GpuStatus> {
        if !renderer.gpu_in_demand() {
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

/// Tests only: how long the device outlives its last use, in
/// milliseconds, in place of 30 s (`strand/tests/gpu_idle.rs` runs two
/// promote, frames and drop cycles without waiting a minute).
#[cfg(feature = "gpu")]
pub(crate) const IDLE_ENV: &str = "STRAND_GPU_IDLE_MS";

/// The renderer's GPU settings from the environment ([`IDLE_ENV`]).
#[cfg(feature = "gpu")]
pub(crate) fn configure(renderer: &mut Renderer) {
    if let Some(ms) = std::env::var(IDLE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        renderer.set_gpu_idle(std::time::Duration::from_millis(ms));
    }
}

#[cfg(feature = "gpu")]
mod host {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use strand_gpu::{Gpu, GpuMode, GpuOptions, GpuReply, GpuRequest};
    use strand_render::Renderer;
    use strand_scene::{BackendChange, Scale, Size, SurfaceId};
    use strand_surface::State;

    use crate::demo::host::Host;

    /// How often an ending GPU thread is checked for having ended.
    const JOIN_POLL: Duration = Duration::from_millis(50);

    /// A surface the GPU thread presents.
    #[derive(Debug)]
    struct Presented {
        size: Size,
        scale: Scale,
        /// A frame was sent and its `Presented` has not come.
        in_flight: bool,
    }

    /// The `Gpu` and what the main thread knows of the surfaces it draws.
    pub(crate) struct GpuHost {
        gpu: Option<Gpu>,
        /// Dropped `Gpu`s whose threads are still dropping their device:
        /// joined once they have ended, never waited for; the surfaces they
        /// presented are taken back then (their swapchains are gone).
        exiting: Vec<(Gpu, Vec<SurfaceId>)>,
        opts: GpuOptions,
        /// Wakes the main loop after every reply.
        ping: calloop::ping::Ping,
        /// Surfaces attached (asked or answered), with how they draw.
        attached: BTreeMap<SurfaceId, Option<GpuMode>>,
        presented: BTreeMap<SurfaceId, Presented>,
    }

    impl std::fmt::Debug for GpuHost {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GpuHost")
                .field("running", &self.gpu.is_some())
                .field("exiting", &self.exiting.len())
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
                opts: GpuOptions::from_env(),
                ping,
                attached: BTreeMap::new(),
                presented: BTreeMap::new(),
            }
        }

        fn gpu(&mut self) -> &Gpu {
            let ping = self.ping.clone();
            let opts = self.opts;
            self.gpu
                .get_or_insert_with(|| Gpu::spawn(Box::new(move || ping.ping()), opts))
        }

        /// Everything owed since the last dispatch: replies to render,
        /// render's changes and requests to the GPU thread, frames of
        /// presented surfaces.
        pub(crate) fn pump(&mut self, state: &mut State<Host>) {
            // Replies first: they can attach, hand off and take back.
            self.replies(state);
            for id in std::mem::take(&mut state.host_mut().gpu_released) {
                if let Some(g) = &self.gpu {
                    g.send(GpuRequest::Release(id));
                } else {
                    state.take_back(id);
                }
            }
            for change in state.host_mut().renderer.take_backend_changes() {
                self.change(state, change);
            }
            let requests = state.host_mut().renderer.take_gpu_requests();
            if !requests.is_empty() {
                let gpu = self.gpu();
                for r in requests {
                    gpu.send(r);
                }
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
        }

        /// How soon the loop must pump again with nothing else waking
        /// it: a thread that is ending is polled until it has ended (its
        /// last ping can come before it has).
        pub(crate) fn wait(&self) -> Option<Duration> {
            (!self.exiting.is_empty()).then_some(JOIN_POLL)
        }

        fn replies(&mut self, state: &mut State<Host>) {
            while let Some(reply) = self.gpu.as_mut().and_then(Gpu::try_recv) {
                match &reply {
                    GpuReply::Attached { surface, mode } => {
                        let id = *surface;
                        let presenting = *mode == GpuMode::Present;
                        if !self.attached.contains_key(&id) || (presenting && !state.hand_off(id)) {
                            // Released or gone meanwhile.
                            if let Some(g) = &self.gpu {
                                g.send(GpuRequest::Release(id));
                            }
                            continue;
                        }
                        self.attached.insert(id, Some(*mode));
                        log::info!("GPU: surface {} attached ({mode:?})", id.0);
                        if presenting {
                            let (size, scale) = geometry(state, id);
                            self.presented.insert(
                                id,
                                Presented {
                                    size,
                                    scale,
                                    in_flight: false,
                                },
                            );
                        }
                    }
                    GpuReply::Released(id) => {
                        let id = *id;
                        log::info!("GPU: surface {} released", id.0);
                        self.attached.remove(&id);
                        self.presented.remove(&id);
                        state.take_back(id);
                    }
                    GpuReply::Presented { surface, .. }
                    | GpuReply::Failed {
                        surface: Some(surface),
                        ..
                    } => {
                        // A frame that failed is answered too: render
                        // takes the surface back if it cannot be drawn.
                        if let Some(p) = self.presented.get_mut(surface) {
                            p.in_flight = false;
                        }
                    }
                    GpuReply::Exited => {
                        // Every swapchain went with the thread: the
                        // manager commits its surfaces again.
                        self.attached.clear();
                        for (id, _) in std::mem::take(&mut self.presented) {
                            state.take_back(id);
                        }
                        if let Some(g) = self.gpu.take() {
                            self.exiting.push((g, Vec::new()));
                        }
                    }
                    _ => {}
                }
                state.host_mut().renderer.deliver_gpu(reply);
            }
        }

        fn change(&mut self, state: &mut State<Host>, change: BackendChange) {
            match change {
                BackendChange::Promote(id) => {
                    if self.attached.contains_key(&id) {
                        return;
                    }
                    let handles = state.raw_handles(id).map(|h| strand_gpu::RawHandles {
                        display: h.display,
                        window: h.window,
                    });
                    let (size, scale) = geometry(state, id);
                    self.attached.insert(id, None);
                    self.gpu().send(GpuRequest::Attach {
                        surface: id,
                        handles,
                        size,
                        scale,
                        opaque: false,
                    });
                }
                BackendChange::Demote(id) => {
                    if self.attached.contains_key(&id)
                        && let Some(g) = &self.gpu
                    {
                        // Its swapchain goes first; `Released` takes the
                        // surface back.
                        g.send(GpuRequest::Release(id));
                    }
                }
                BackendChange::Drop => {
                    // Render has forgotten the device. Its replies are no
                    // longer read: a surface still handed off (its
                    // `Released` not in yet) is taken back once the
                    // thread has ended, its swapchain dropped first.
                    let held: Vec<SurfaceId> =
                        std::mem::take(&mut self.presented).into_keys().collect();
                    self.attached.clear();
                    if let Some(g) = self.gpu.take() {
                        g.send(GpuRequest::Shutdown);
                        self.exiting.push((g, held));
                    } else {
                        for id in held {
                            state.take_back(id);
                        }
                    }
                }
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
                if p.in_flight {
                    continue;
                }
                let r: &mut Renderer = &mut state.host_mut().renderer;
                if let Some(frame) = r.paint_gpu(*id, now) {
                    gpu.send(GpuRequest::Frame(frame));
                    p.in_flight = true;
                }
            }
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
}

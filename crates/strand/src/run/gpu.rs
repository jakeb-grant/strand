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
//!
//! (lock-gpu, OWNER RULING) A lock's content surface is presented like a
//! panel's once render promotes it, after its first frame (the CPU's), and
//! never read back: where it cannot be presented (no handles, a readback
//! answer) it stays on the CPU (`Renderer::keep_on_cpu`); where it
//! cannot be attached yet (not mapped, a configure pending, the device
//! starting) a later frame asks again (`Renderer::refuse_promotion`). Its progress never waits on
//! the GPU thread: a thread that answers nothing for [`LOCK_GPU_BOUND`]
//! while a lock surface's attach or frame waits on it renews every lock
//! surface it holds and gets no lock content until it answers again; a
//! failure, a lost device, the thread's end, or the built-in fallback
//! showing on one renews that one (`State::renew_lock_content`): its
//! output gets a fresh lock surface drawn on the CPU until the lock ends,
//! and the session stays locked. A presented lock surface is never taken
//! back for `wl_shm`.

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

/// (lock-gpu) How long the GPU thread may answer nothing while a lock
/// surface's attach or frame waits on it before the lock stops waiting
/// for the GPU and renews its surfaces on the CPU (decisions.md
/// lock-gpu). It times the thread's silence from the later of the
/// request's sending and the thread's last answer, never the queue of
/// other outputs' requests ahead of it. Well above the slowest single
/// answer a working GPU gave (the owner's Arc B390, see decisions.md),
/// and well under what a user takes for a hung lock.
#[cfg(feature = "gpu")]
pub(crate) const LOCK_GPU_BOUND: std::time::Duration = std::time::Duration::from_millis(500);

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
        /// A frame or paced `Commit` was sent and its answer has not come.
        in_flight: bool,
        /// The sent frame's size and scale, for its `damage` log line
        /// once presented.
        sent: Option<(Size, Scale)>,
        /// When a paint last sent nothing (decisions.md presented-quiet).
        drew_nothing: Option<std::time::Instant>,
        /// (gpu-pace) Its last present's frame callback has not come: its
        /// next frame waits for that, which wakes the loop.
        paced: bool,
        /// (gpu-pace) A `Commit` waiting for that too: a present then
        /// would wait for the callback on the GPU thread.
        commit_owed: bool,
        /// (gpu-pace) The present in flight is a `Commit` that
        /// `State::pace_commit` paced, so its answer ends `in_flight`.
        commit_paced: bool,
        /// (lock-gpu) When what is in flight was sent.
        sent_at: Option<std::time::Instant>,
        /// (lock-4k) A lock surface's first presented frame: when it was
        /// sent, until its `Presented` (logged then); `None` before it
        /// goes and after.
        first_out: Option<std::time::Instant>,
        /// (lock-4k) Its first frame went.
        first_sent: bool,
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
        /// (lock-gpu) A lock's content surface: presented or kept on the
        /// CPU, never read back, and renewed when the GPU fails it.
        lock: bool,
        /// (lock-gpu) When its `Attach` was sent, until answered.
        asked: Option<std::time::Instant>,
        /// (lock-4k) How many requests were waiting on the thread when its
        /// `Attach` was sent (attaches and frames not answered yet).
        ahead: usize,
    }

    /// (lock-4k) The GPU thread's longest silence while a lock request
    /// waited on it, in one lock: from the later of that request's sending
    /// and the thread's last answer ([`waiting_since`]) to the next
    /// answer. Logged once the lock ends; the bound ([`super::LOCK_GPU_BOUND`])
    /// is judged against it.
    #[derive(Debug, Default)]
    struct Silence {
        /// The longest so far; `None` until a lock request was sent in
        /// this lock.
        longest: Option<std::time::Duration>,
    }

    /// (lock-4k) The times the thread sent its replies, numbered in
    /// order from 1 ([`GpuHost::reply_sent`]).
    #[derive(Debug, Default)]
    struct Stamps {
        sent: u64,
        at: std::collections::VecDeque<(u64, std::time::Instant)>,
    }

    impl Stamps {
        /// The thread sent its next reply at `at`.
        fn stamp(&mut self, at: std::time::Instant) {
            self.sent += 1;
            self.at.push_back((self.sent, at));
        }

        /// When reply `n` was sent, if stamped; earlier ones go (read
        /// before their stamp came).
        fn take(&mut self, n: u64) -> Option<std::time::Instant> {
            while self.at.front().is_some_and(|(k, _)| *k < n) {
                self.at.pop_front();
            }
            match self.at.front() {
                Some((k, t)) if *k == n => {
                    let t = *t;
                    self.at.pop_front();
                    Some(t)
                }
                _ => None,
            }
        }
    }

    impl Silence {
        /// A lock request was sent.
        fn asked(&mut self) {
            self.longest.get_or_insert(std::time::Duration::ZERO);
        }

        /// The thread answered (or the lock ended) at `now`, the oldest
        /// lock request then waiting having waited since `waiting`.
        fn heard(&mut self, waiting: Option<std::time::Instant>, now: std::time::Instant) {
            if let (Some(w), Some(l)) = (waiting, &mut self.longest) {
                *l = (*l).max(now.saturating_duration_since(w));
            }
        }

        /// The longest silence of the lock that ended, if a lock request
        /// went to the thread in it; ready for the next lock.
        fn take(&mut self) -> Option<std::time::Duration> {
            self.longest.take()
        }
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
        /// (lock-gpu) When the running thread last answered anything: it
        /// works through its requests in order, so one sent before then
        /// may have started no earlier ([`lock_deadline`]).
        answered: Option<std::time::Instant>,
        /// (lock-gpu) The running thread left a lock surface unanswered
        /// past [`super::LOCK_GPU_BOUND`]: no lock content is attached to
        /// it until it answers again or ends.
        lock_stalled: bool,
        /// (lock-4k) The thread's longest silence under the current lock.
        silence: Silence,
        /// (lock-4k) When the running thread sent each reply, stamped by
        /// its waker on the thread: a reply read late (the main thread
        /// busy) is timed from when it was sent.
        stamps: std::sync::Arc<std::sync::Mutex<Stamps>>,
        /// (lock-4k) Replies read from the running thread.
        read: u64,
        /// (lock-4k) A lock was asked for or held at the last pump.
        locked: bool,
    }

    impl std::fmt::Debug for GpuHost {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GpuHost")
                .field("running", &self.gpu.is_some())
                .field("exiting", &self.exiting.len())
                .field("queued", &self.queued.len())
                .field("attached", &self.attached)
                .field("presented", &self.presented)
                .field("lock_stalled", &self.lock_stalled)
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
                answered: None,
                lock_stalled: false,
                silence: Silence::default(),
                stamps: Default::default(),
                read: 0,
                locked: false,
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
            self.answered = None;
            self.lock_stalled = false;
            self.stamps = Default::default();
            self.read = 0;
            let stamps = std::sync::Arc::clone(&self.stamps);
            self.gpu.insert(Gpu::spawn(
                Box::new(move || {
                    // (lock-4k) Called once after each reply is sent.
                    if let Ok(mut s) = stamps.lock() {
                        s.stamp(std::time::Instant::now());
                    }
                    // (lock-gpu) The lock VM's `gpu_stall`: the thread
                    // stops here for good once armed.
                    super::super::lock::faults::gpu_waker();
                    ping.ping();
                }),
                self.opts,
            ))
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
            // (lock-4k) Stalls are judged on the replies just read, before
            // this pump paints: `present` paints each presented surface in
            // turn, so a frame sent for one waited out the next ones'
            // paints (two 3840 × 2160 stills drawn, about 900 ms on the
            // owner's laptop) and was given up on with its answer unread.
            // What this pump sends is judged at the next, after its answer.
            self.lock_safety(state);
            self.lock_ended(state);
            self.released(state);
            for change in state.host_mut().renderer.take_backend_changes() {
                self.change(state, change);
            }
            for r in state.host_mut().renderer.take_gpu_requests() {
                if let GpuRequest::Commit(id) = r {
                    // It commits whatever the manager left pending. A
                    // surface not presented has no last frame to send again.
                    state.take_owed_commit(id);
                    if let Some(p) = self.presented.get_mut(&id) {
                        p.commit_owed = true;
                    }
                    continue;
                }
                self.send(r);
            }
            self.present(state);
            super::super::lock::faults::main_busy(|| {
                self.attached
                    .iter()
                    .any(|(id, a)| a.lock && self.presented.get(id).is_some_and(|p| p.in_flight))
            });
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

        /// Surfaces the manager destroyed while the GPU thread held them
        /// (`SurfaceHost::gpu_release`): their swapchains go first.
        fn released(&mut self, state: &mut State<Host>) {
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
        }

        /// (lock-gpu) Lock surfaces the GPU thread holds are renewed on the
        /// CPU once one of them has gone unanswered for
        /// [`super::LOCK_GPU_BOUND`] (the thread stalled: every one of them,
        /// and no lock content goes to it until it answers again), or one
        /// at a time when the built-in fallback shows on it (drawn by the
        /// main thread only).
        fn lock_safety(&mut self, state: &mut State<Host>) {
            let stalled = self.lock_stall(std::time::Instant::now());
            let renew: Vec<(SurfaceId, &'static str)> = self
                .attached
                .iter()
                .filter_map(|(id, a)| {
                    let fallback = state.host().lock.shown_on(*id);
                    Some((*id, lock_gives_up(a, stalled, fallback)?))
                })
                .collect();
            for (id, why) in renew {
                self.renew_lock(state, id, why);
            }
        }

        /// (lock-gpu) True when a lock surface's attach or frame has waited
        /// past its bound at `now` ([`lock_deadline`]): the thread stalled,
        /// and no lock content goes to it until it answers again.
        fn lock_stall(&mut self, now: std::time::Instant) -> bool {
            let stalled = self.attached.iter().any(|(id, a)| {
                lock_deadline(a, self.presented.get(id), self.answered).is_some_and(|t| now >= t)
            });
            if stalled && !self.lock_stalled {
                self.lock_stalled = true;
                log::warn!(
                    "lock: the GPU thread answered nothing for {} ms: no lock surface goes to it until it answers again",
                    super::LOCK_GPU_BOUND.as_millis()
                );
            }
            stalled
        }

        /// (lock-gpu) The running thread answered at `now`: it is alive, so
        /// what it was sent before is timed from now, and a lock may use
        /// it again.
        #[cfg(test)]
        fn answer(&mut self, now: std::time::Instant) {
            self.answer_sent(now, now);
        }

        /// [`Self::answer`] of a reply the thread sent at `sent`: the
        /// longest silence is measured to `sent`.
        fn answer_sent(&mut self, sent: std::time::Instant, now: std::time::Instant) {
            let waiting = self.lock_waiting();
            self.silence.heard(waiting, sent);
            self.answered = Some(now);
            self.lock_stalled = false;
        }

        /// (lock-4k) When the thread sent the reply just read: its stamp,
        /// or now if the waker has not stamped it yet.
        fn reply_sent(&mut self) -> std::time::Instant {
            self.read += 1;
            let now = std::time::Instant::now();
            self.stamps
                .lock()
                .ok()
                .and_then(|mut s| s.take(self.read))
                .map_or(now, |t| t.min(now))
        }

        /// (lock-4k) Since when the thread's oldest unanswered lock
        /// request has waited on it ([`waiting_since`]).
        fn lock_waiting(&self) -> Option<std::time::Instant> {
            self.attached
                .iter()
                .filter_map(|(id, a)| waiting_since(a, self.presented.get(id), self.answered))
                .min()
        }

        /// (lock-4k) Requests waiting on the thread: attaches and frames
        /// not answered yet.
        fn waiting_requests(&self) -> usize {
            self.attached.values().filter(|a| a.asked.is_some()).count()
                + self.presented.values().filter(|p| p.in_flight).count()
        }

        /// (lock-4k) Once a lock ends, the thread's longest silence while
        /// a lock request waited on it, if one went to it.
        fn lock_ended(&mut self, state: &State<Host>) {
            let active = state.lock_active();
            if std::mem::replace(&mut self.locked, active) && !active {
                let waiting = self.lock_waiting();
                self.silence.heard(waiting, std::time::Instant::now());
                if let Some(l) = self.silence.take() {
                    log::info!(
                        "lock: the GPU thread's longest silence with a lock request waiting was {} ms",
                        l.as_millis()
                    );
                }
            }
        }

        /// (lock-gpu) Lock content `id` goes from the GPU thread to a fresh
        /// lock surface on the CPU (`State::renew_lock_content`); the old
        /// one's swapchain is released (a stalled thread may never answer:
        /// its `wl_surface` is kept until it does or ends).
        fn renew_lock(&mut self, state: &mut State<Host>, id: SurfaceId, why: &str) {
            log::warn!("lock: the GPU failed surface {} ({why})", id.0);
            // No frame of it goes again, whatever the thread answers.
            self.presented.remove(&id);
            let renewed = state.renew_lock_content(id).is_some();
            if let Some(a) = self.attached.get_mut(&id) {
                a.asked = None;
                // Not a lock's content any more (the lock ended): its
                // destroy releases it, as any surface's.
                a.lock &= renewed;
            }
            self.released(state);
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
        ///
        /// (presented-quiet) A presented surface that wants a frame now, with
        /// none in flight, has no reply coming and no hold left to wake the
        /// loop (a deadline that lapsed before this read it): it is painted
        /// at once, or, if its last paint sent nothing, a refresh after that
        /// one, so a surface that keeps wanting frames and drawing nothing
        /// is paced as a frame callback would pace it and never spins.
        pub(crate) fn wait(&self, renderer: &Renderer) -> Option<Duration> {
            let poll = (!self.exiting.is_empty()).then_some(JOIN_POLL);
            let now = std::time::Instant::now();
            let held = self
                .presented
                .iter()
                .filter(|(_, p)| !p.in_flight && !p.paced)
                .filter_map(|(id, _)| renderer.frame_deadline(*id))
                .min()
                .map(|t| t.saturating_duration_since(now));
            let due = self
                .presented
                .iter()
                .filter(|(id, p)| {
                    !p.in_flight && !p.paced && self.attached.get(id).is_some_and(|a| !a.releasing)
                })
                .filter_map(|(id, p)| {
                    let period = renderer.presented_due(*id)?;
                    Some(p.drew_nothing.map_or(Duration::ZERO, |t| {
                        (t + period).saturating_duration_since(now)
                    }))
                })
                .min();
            // (lock-gpu) A lock surface's unanswered attach or frame, at
            // its bound.
            let lock = self
                .attached
                .iter()
                .filter_map(|(id, a)| lock_deadline(a, self.presented.get(id), self.answered))
                .min()
                .map(|t| t.saturating_duration_since(std::time::Instant::now()));
            [poll, held, due, lock].into_iter().flatten().min()
        }

        /// Delivers the thread's replies to render; true when one may
        /// have changed what a surface shows (anything but a
        /// `Presented`).
        fn replies(&mut self, state: &mut State<Host>) -> bool {
            let mut changed = false;
            while let Some(reply) = self.gpu.as_mut().and_then(Gpu::try_recv) {
                let sent = self.reply_sent();
                self.answer_sent(sent, std::time::Instant::now());
                match &reply {
                    GpuReply::Attached { surface, mode } => {
                        let id = *surface;
                        let presenting = *mode == GpuMode::Present;
                        if let Some(a) = self.attached.get_mut(&id) {
                            if a.lock
                                && let Some(t) = a.asked
                            {
                                log::info!(
                                    "lock: surface {}'s attach was answered {} ms after it was sent \
                                     ({} requests were waiting on the thread ahead of it)",
                                    id.0,
                                    sent.saturating_duration_since(t).as_millis(),
                                    a.ahead
                                );
                            }
                            a.asked = None;
                        }
                        if !presenting
                            && self
                                .attached
                                .get(&id)
                                .is_some_and(|a| a.lock && !a.releasing)
                        {
                            // (lock-gpu) A lock is never read back: the thread
                            // made no swapchain, so the manager takes it back
                            // and it stays on the CPU.
                            log::info!("GPU: lock surface {} cannot be presented", id.0);
                            state.take_back(id);
                            if let Some(a) = self.attached.get_mut(&id) {
                                a.lent = false;
                            }
                            self.release(id);
                            state.host_mut().renderer.keep_on_cpu(id);
                            continue;
                        }
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
                                    paced: false,
                                    commit_owed: false,
                                    commit_paced: false,
                                    sent_at: None,
                                    first_out: None,
                                    first_sent: false,
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
                        state.host_mut().settle_presenting.remove(&id);
                        self.attached.remove(&id);
                        self.presented.remove(&id);
                        state.take_back(id);
                    }
                    GpuReply::Presented { surface, .. } => {
                        if state.host_mut().settle_presenting.remove(surface) {
                            state.lock_settle_look();
                        }
                        if self.attached.get(surface).is_some_and(|a| a.lock) {
                            super::super::lock::faults::lock_presented(state.lock_leaving());
                        }
                        if let Some(p) = self.presented.get_mut(surface) {
                            p.in_flight = false;
                            p.sent_at = None;
                            if let Some(t) = p.first_out.take() {
                                log::info!(
                                    "lock: surface {}'s first presented frame was presented {} ms \
                                     after it was sent",
                                    surface.0,
                                    sent.saturating_duration_since(t).as_millis()
                                );
                            }
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
                                    true,
                                );
                            }
                        }
                    }
                    GpuReply::Committed { surface, committed } => {
                        if let Some(p) = self.presented.get_mut(surface)
                            && std::mem::take(&mut p.commit_paced)
                        {
                            p.in_flight = false;
                            p.sent_at = None;
                            if !*committed {
                                state.forget_present_callback(*surface);
                            }
                        }
                        // Nothing for render, and nothing changed.
                        continue;
                    }
                    GpuReply::Failed {
                        surface: Some(surface),
                        ..
                    } => {
                        // A frame that failed is answered too: render
                        // takes the surface back if it cannot be drawn.
                        if let Some(p) = self.presented.get_mut(surface) {
                            p.in_flight = false;
                            p.commit_paced = false;
                            p.sent = None;
                            p.sent_at = None;
                        }
                        if state.host_mut().settle_presenting.remove(surface) {
                            state.lock_settle_look();
                        }
                        state.forget_present_callback(*surface);
                    }
                    GpuReply::Exited => {
                        // Every swapchain went with the thread: the
                        // manager commits its surfaces again and no
                        // longer keeps the lent ones for it.
                        self.presented.clear();
                        // (lock-int) No present is coming.
                        if !std::mem::take(&mut state.host_mut().settle_presenting).is_empty() {
                            state.lock_settle_look();
                        }
                        let attached = std::mem::take(&mut self.attached);
                        for id in lent(&attached) {
                            state.take_back(id);
                            // (lock-gpu) A lock surface the thread held is
                            // never committed to with `wl_shm`: it is
                            // renewed before any paint, so its old
                            // `wl_surface`, no longer held, goes at once.
                            if attached.get(&id).is_some_and(|a| a.lock) {
                                log::warn!("lock: the GPU thread ended under surface {}", id.0);
                                state.renew_lock_content(id);
                            }
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
                    let lock = state.is_lock_content(id);
                    let kept = lock && state.lock_kept_on_cpu(id);
                    // (lock-gpu review) A lock surface goes only to a
                    // device that is up and a thread that has not stalled
                    // under a lock (its bound never times a device start),
                    // and never beside an ending thread's swapchain. Its
                    // handles are asked for only then: a lent surface skips
                    // the manager's paints until it is taken back.
                    let up = matches!(
                        state.host().renderer.gpu_status(),
                        strand_scene::GpuStatus::Up(_)
                    );
                    let ready = !lock || (up && !self.lock_stalled && !ending);
                    let handles = if kept || !ready {
                        None
                    } else {
                        state
                            .raw_handles(id)
                            .filter(|_| !ending)
                            .map(|h| strand_gpu::RawHandles {
                                display: h.display,
                                window: h.window,
                            })
                    };
                    match lock_promotion(lock, handles.is_some(), ready, kept) {
                        LockPromotion::Attach => {}
                        LockPromotion::Refuse => {
                            // Not mapped yet, a configure pending at another
                            // size, the device not up yet, its thread
                            // stalled or ending: a later frame asks again.
                            state.host_mut().renderer.refuse_promotion(id);
                            return;
                        }
                        LockPromotion::Keep => {
                            // (lock-gpu) A lock is presented or drawn on the
                            // CPU, never read back: readback buys it nothing.
                            log::info!("GPU: lock surface {} stays on the CPU", id.0);
                            state.host_mut().renderer.keep_on_cpu(id);
                            return;
                        }
                    }
                    let (size, scale) = geometry(state, id);
                    let ahead = self.waiting_requests();
                    if lock {
                        self.silence.asked();
                        if let Some(asked) = state.lock_asked() {
                            log::info!(
                                "lock: surface {} lent to the GPU thread {} ms after the lock \
                                 request ({ahead} requests waiting on the thread ahead of it)",
                                id.0,
                                asked.elapsed().as_millis()
                            );
                        }
                    }
                    self.attached.insert(
                        id,
                        Attachment {
                            lent: handles.is_some(),
                            lock,
                            asked: lock.then(std::time::Instant::now),
                            ahead,
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
                    if self
                        .attached
                        .get(&id)
                        .is_some_and(|a| a.lock && !a.releasing)
                    {
                        // (lock-gpu) The GPU failed a lock surface: renewed,
                        // never taken back.
                        self.renew_lock(state, id, "a frame failed");
                    } else if self.attached.contains_key(&id) && self.running() {
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
                p.paced = !state.present_due(*id);
                if p.in_flight || releasing || p.paced {
                    continue;
                }
                // Without a compositor's FIFO, what follows a `Commit` waits
                // for its callback as for a frame's, or a frame sent now would
                // wait in Mesa, on an output that may be off for good.
                if std::mem::take(&mut p.commit_owed) {
                    state.take_owed_commit(*id);
                    let paced = state.pace_commit(*id);
                    gpu.send(GpuRequest::Commit(*id));
                    if paced {
                        p.commit_paced = true;
                        p.in_flight = true;
                        p.sent_at = Some(std::time::Instant::now());
                        continue;
                    }
                }
                let r: &mut Renderer = &mut state.host_mut().renderer;
                let due = r.presented_due(*id).is_some();
                // (lock-int) A lock's stills wait for this frame presented.
                let settling = due && r.lock_settle_pending(*id);
                let frame = if due { r.paint_gpu(*id, now) } else { None };
                if let Some(frame) = frame {
                    let (size, scale) = (frame.size, frame.scale);
                    // What follows a CPU frame follows this one too:
                    // its regions, as pending state the present commits
                    // (decisions.md gpu-regions), set before the thread
                    // can present it; layout facts and list windows to
                    // logic, fed nodes (its `damage` line waits for
                    // `Presented`).
                    state.sync_presented(*id);
                    state.take_owed_commit(*id);
                    // Asked before the frame goes, so the request reaches
                    // the compositor ahead of the commit the GPU thread
                    // makes for it; only a frame that goes asks one, so
                    // an idle surface has no wake.
                    state.ask_present_callback(*id);
                    gpu.send(GpuRequest::Frame(frame));
                    p.in_flight = true;
                    p.sent_at = Some(std::time::Instant::now());
                    if !std::mem::replace(&mut p.first_sent, true)
                        && self.attached.get(id).is_some_and(|a| a.lock)
                    {
                        p.first_out = p.sent_at;
                        self.silence.asked();
                    }
                    p.sent = Some((size, scale));
                    p.drew_nothing = None;
                    state.host_mut().painted(*id, &Damage::full(size));
                    if settling {
                        state.host_mut().settle_presenting.insert(*id);
                    }
                } else {
                    if due {
                        // (presented-quiet) It painted and drew nothing, so
                        // nothing is sent and no `Presented` comes. All the
                        // same that paint may have ended an exit or let a
                        // shrink go, and moved hit boxes: what follows a
                        // CPU's empty paint follows it.
                        state.host().log_unsent(*id);
                        p.drew_nothing = Some(std::time::Instant::now());
                        state.sync_presented(*id);
                        state.host_mut().painted(*id, &Damage::new());
                        if settling {
                            // Nothing changed: settled as it is.
                            state.lock_settle_look();
                        }
                    }
                    // A CPU's empty paint carries its regions in a bare
                    // commit. Only a present commits this surface.
                    if state.take_owed_commit(*id) {
                        let paced = state.pace_commit(*id);
                        gpu.send(GpuRequest::Commit(*id));
                        p.commit_paced = paced;
                        p.in_flight = paced;
                        p.sent_at = paced.then(std::time::Instant::now);
                    }
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

    /// (lock-gpu) When lock surface `a` (presented as `p`, if it is) is
    /// given up on unless an answer comes: [`super::LOCK_GPU_BOUND`] after
    /// its attach or its frame was sent, or after the thread last answered
    /// anything (`answered`), whichever is later. The thread works through
    /// its requests in order, one output's after another's, so a request
    /// queued behind others starts no earlier than the last answer: the
    /// bound times the thread's silence, never the queue. `None` for any
    /// other surface, one being released, or one with nothing unanswered.
    fn lock_deadline(
        a: &Attachment,
        p: Option<&Presented>,
        answered: Option<std::time::Instant>,
    ) -> Option<std::time::Instant> {
        Some(waiting_since(a, p, answered)? + super::LOCK_GPU_BOUND)
    }

    /// (lock-4k) Since when lock surface `a` has waited on the thread's
    /// silence: the later of its oldest unanswered request's sending and
    /// the thread's last answer (`None` as for [`lock_deadline`]).
    fn waiting_since(
        a: &Attachment,
        p: Option<&Presented>,
        answered: Option<std::time::Instant>,
    ) -> Option<std::time::Instant> {
        if !a.lock || a.releasing {
            return None;
        }
        let frame = p.filter(|p| p.in_flight).and_then(|p| p.sent_at);
        let sent = [a.asked, frame].into_iter().flatten().min()?;
        Some(sent.max(answered.unwrap_or(sent)))
    }

    /// (lock-gpu) Why the host gives up on the GPU for lock surface `a`,
    /// if it does: its thread `stalled` (a lock surface it holds went
    /// unanswered past its bound), or the built-in fallback showing on it
    /// (`fallback`), which only the main thread draws.
    fn lock_gives_up(a: &Attachment, stalled: bool, fallback: bool) -> Option<&'static str> {
        if !a.lock || a.releasing {
            return None;
        }
        if stalled {
            return Some("its GPU thread stalled");
        }
        fallback.then_some("the built-in password field shows")
    }

    /// What a promotion of lock content does ([`lock_promotion`]).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum LockPromotion {
        /// Attached as any surface (not lock content, or lent).
        Attach,
        /// Undone for now (`Renderer::refuse_promotion`): a later frame
        /// that animates a large area asks again.
        Refuse,
        /// Kept on the CPU for good (`Renderer::keep_on_cpu`).
        Keep,
    }

    /// (lock-gpu) A promotion of `lock` content: attached when its handles
    /// were lent (`lent`) and the GPU is `ready` (its device up, its thread
    /// not stalled under a lock); kept on the CPU for good when its
    /// output's content was renewed in this lock (`kept`, a refusal that
    /// lasts); otherwise undone for now (not mapped yet, a configure
    /// pending at another size, an ending thread holding it, the device
    /// starting, the thread stalled), since a lock is never read back.
    /// Anything else is attached as before.
    fn lock_promotion(lock: bool, lent: bool, ready: bool, kept: bool) -> LockPromotion {
        match (lock, kept, lent && ready) {
            (false, _, _) => LockPromotion::Attach,
            (true, true, _) => LockPromotion::Keep,
            (true, false, true) => LockPromotion::Attach,
            (true, false, false) => LockPromotion::Refuse,
        }
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
        /// A presented surface with nothing in flight, its last frame or
        /// paced commit sent at `sent_at`.
        fn presented(sent_at: Option<std::time::Instant>) -> Presented {
            Presented {
                size: Size::new(100, 100),
                scale: Scale::ONE,
                in_flight: false,
                sent: None,
                drew_nothing: None,
                paced: true,
                commit_owed: false,
                commit_paced: false,
                sent_at,
                first_out: None,
                first_sent: false,
            }
        }

        /// (lock-4k) The longest-silence tracker reports the largest gap
        /// between a lock request's wait starting (its sending, or the
        /// thread's last answer if later) and the thread's next answer,
        /// over a lock; nothing for a lock that sent the thread nothing.
        #[test]
        fn the_longest_silence_is_the_widest_gap_to_an_answer() {
            use std::time::{Duration, Instant};
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            assert_eq!(host.silence.take(), None, "nothing sent: no line");
            // Two outputs' attaches sent at t0, answered at 120 and 380 ms:
            // the second waited on the thread's silence from 120 ms.
            let attach = || Attachment {
                lent: true,
                lock: true,
                asked: Some(t0),
                ..Attachment::default()
            };
            host.attached.insert(SurfaceId(1), attach());
            host.attached.insert(SurfaceId(2), attach());
            host.silence.asked();
            host.answer(t0 + ms(120));
            host.attached.get_mut(&SurfaceId(1)).unwrap().asked = None;
            host.answer(t0 + ms(380));
            host.attached.get_mut(&SurfaceId(2)).unwrap().asked = None;
            // An answer with nothing waiting is no silence.
            host.answer(t0 + ms(2000));
            // A frame sent at 2100 ms, answered at 2150.
            let mut p = presented(Some(t0 + ms(2100)));
            p.in_flight = true;
            host.presented.insert(SurfaceId(1), p);
            host.answer(t0 + ms(2150));
            host.presented.get_mut(&SurfaceId(1)).unwrap().in_flight = false;
            assert_eq!(host.silence.take(), Some(ms(260)));
            assert_eq!(host.silence.take(), None, "taken once per lock");
            // A reply read late is timed from when the thread sent it.
            host.attached.get_mut(&SurfaceId(2)).unwrap().asked = Some(t0 + ms(3000));
            host.silence.asked();
            host.answer_sent(t0 + ms(3100), t0 + ms(3900));
            assert_eq!(host.silence.take(), Some(ms(100)));
            // Stamps are matched to replies in order; one read before its
            // stamp came is now, and its late stamp is dropped.
            let mut st = Stamps::default();
            st.stamp(t0);
            st.stamp(t0 + ms(5));
            assert_eq!(st.take(1), Some(t0));
            assert_eq!(st.take(3), None);
            st.stamp(t0 + ms(9));
            assert_eq!(st.take(4), None);
            st.stamp(t0 + ms(12));
            assert_eq!(st.take(4), Some(t0 + ms(12)));
        }

        #[test]
        fn a_lent_surface_is_kept_until_the_thread_lets_go() {
            let lent_to = |releasing| Attachment {
                mode: None,
                lent: true,
                releasing,
                ..Attachment::default()
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
                    paced: false,
                    commit_owed: false,
                    commit_paced: false,
                    sent_at: None,
                    first_out: None,
                    first_sent: false,
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
            if let Some(pr) = host.presented.get_mut(&s) {
                pr.in_flight = false;
                pr.paced = true;
            }
            assert_eq!(host.wait(&r), None, "paced: its frame callback wakes it");
        }

        /// (presented-quiet) A presented surface that wants a frame with none
        /// in flight wakes the loop though no hold is left, or it would wait
        /// for whatever woke it next: at once, or a refresh after a paint that
        /// sent nothing. One that nothing can paint wakes nothing, so the loop
        /// never spins on it.
        #[test]
        fn a_presented_surface_wanting_a_frame_always_has_a_wake() {
            use strand_scene::{Backend, NodeId, NodeKind, Prop, PropValue, SceneDiff};
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            let font = std::fs::read(strand_text::test_font_path()).unwrap();
            let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
                std::sync::Arc::new(font),
            ]));
            let mut r = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
            let p = NodeId::new(1, 0);
            let mut d = SceneDiff::new();
            d.create(p, NodeKind::Panel, None, u32::MAX);
            d.set(p, Prop::Width, PropValue::Number(100.0));
            d.set(p, Prop::Height, PropValue::Number(100.0));
            r.apply(d);
            let s = SurfaceId(5);
            r.attach_surface(s, p);
            r.configure_surface(s, Size::new(100, 100), Scale::ONE);
            host.attached.insert(s, Attachment::default());
            host.presented.insert(
                s,
                Presented {
                    size: Size::new(100, 100),
                    scale: Scale::ONE,
                    in_flight: false,
                    sent: None,
                    drew_nothing: None,
                    paced: false,
                    commit_owed: false,
                    commit_paced: false,
                    sent_at: None,
                    first_out: None,
                    first_sent: false,
                },
            );
            assert_eq!(
                host.wait(&r),
                None,
                "the CPU's: nothing for this host to do"
            );
            r.promote_now(s);
            r.set_backend(s, Backend::GpuPresent);
            assert_eq!(host.wait(&r), Some(Duration::ZERO), "wants one: at once");
            let period = r.presented_due(s).unwrap();
            host.presented.get_mut(&s).unwrap().drew_nothing = Some(std::time::Instant::now());
            let w = host.wait(&r).expect("woken for the next");
            assert!(w > Duration::ZERO && w <= period, "{w:?} of {period:?}");
            host.presented.get_mut(&s).unwrap().drew_nothing =
                Some(std::time::Instant::now() - period * 2);
            assert_eq!(host.wait(&r), Some(Duration::ZERO), "long since: at once");
            host.presented.get_mut(&s).unwrap().in_flight = true;
            assert_eq!(host.wait(&r), None, "in flight: its `Presented` wakes it");
            host.presented.get_mut(&s).unwrap().in_flight = false;
            if let Some(a) = host.attached.get_mut(&s) {
                a.releasing = true;
            }
            assert_eq!(
                host.wait(&r),
                None,
                "going: no frame is sent after `Release`"
            );
        }

        /// (lock-gpu) A lock surface's attach or frame unanswered for
        /// `LOCK_GPU_BOUND` gives the lock up on the GPU (it is renewed on
        /// the CPU); an answered one, a frame waiting for its compositor's
        /// callback (nothing in flight: an output turned off), one being
        /// released, or a surface no lock's never does. The built-in
        /// fallback showing on it does at once.
        #[test]
        fn a_lock_surface_is_given_up_on_at_its_bound() {
            use std::time::Instant;
            let bound = super::super::LOCK_GPU_BOUND;
            let t0 = Instant::now();
            let asked = Attachment {
                lent: true,
                lock: true,
                asked: Some(t0),
                ..Attachment::default()
            };
            assert_eq!(lock_deadline(&asked, None, None), Some(t0 + bound));
            assert_eq!(lock_gives_up(&asked, false, false), None);
            assert_eq!(
                lock_gives_up(&asked, true, false),
                Some("its GPU thread stalled")
            );
            // A panel's attach is never bounded, nor given up on.
            let panel = Attachment {
                lock: false,
                ..asked
            };
            assert_eq!(lock_deadline(&panel, None, None), None);
            assert_eq!(lock_gives_up(&panel, true, true), None);
            // Presented: only a frame in flight counts, from when it went.
            let shown = Attachment {
                mode: Some(GpuMode::Present),
                lent: true,
                lock: true,
                ..Attachment::default()
            };
            let mut p = presented(Some(t0));
            assert_eq!(
                lock_deadline(&shown, Some(&p), None),
                None,
                "waiting for its callback is no stall"
            );
            p.in_flight = true;
            assert_eq!(lock_deadline(&shown, Some(&p), None), Some(t0 + bound));
            assert_eq!(
                lock_gives_up(&shown, false, true),
                Some("the built-in password field shows")
            );
            // Released already (renewed): nothing more.
            let going = Attachment {
                releasing: true,
                ..shown
            };
            assert_eq!(lock_deadline(&going, Some(&p), None), None);
            assert_eq!(lock_gives_up(&going, true, true), None);
        }

        /// (lock-gpu review) The bound times the GPU thread's silence, not
        /// the queue: three outputs' first frames sent in one pump and
        /// answered one after another (the owner's Arc: 217, 141 and 76 ms,
        /// about 430 ms for the third) are never given up on, since each
        /// answer moves the later ones' deadlines; a thread that then
        /// answers nothing is given up on one bound after its last answer.
        #[test]
        fn the_bound_counts_from_the_threads_last_answer() {
            use std::time::{Duration, Instant};
            let bound = super::super::LOCK_GPU_BOUND;
            let t0 = Instant::now();
            let shown = Attachment {
                mode: Some(GpuMode::Present),
                lent: true,
                lock: true,
                ..Attachment::default()
            };
            let mut p = presented(Some(t0));
            p.in_flight = true;
            let ms = Duration::from_millis;
            // The third output's frame, behind two others.
            let mut answered = None;
            for (at, last) in [(ms(217), false), (ms(358), false), (ms(434), true)] {
                let deadline = lock_deadline(&shown, Some(&p), answered).unwrap();
                assert!(deadline > t0 + at, "given up at {at:?}, before its answer");
                if last {
                    break;
                }
                answered = Some(t0 + at);
            }
            assert_eq!(
                lock_deadline(&shown, Some(&p), answered),
                Some(t0 + ms(358) + bound)
            );
            // Sent after the thread's last answer: from its sending.
            assert_eq!(
                lock_deadline(&shown, Some(&p), Some(t0 - ms(100))),
                Some(t0 + bound)
            );
            // An attach and a frame waiting: the earlier one counts.
            let both = Attachment {
                asked: Some(t0 - ms(50)),
                ..shown
            };
            assert_eq!(
                lock_deadline(&both, Some(&p), None),
                Some(t0 - ms(50) + bound)
            );
        }

        /// (lock-gpu review) Lock content is attached only when lent and the
        /// GPU is ready (its device up, its thread not stalled under a
        /// lock); a refusal that lasts (its output renewed in this lock)
        /// keeps it on the CPU for good, and one that passes (not mapped
        /// yet, a configure pending, the device starting, a stalled
        /// thread) only undoes the promotion, so a later frame asks again.
        /// Anything else is attached as before, lent or not.
        #[test]
        fn a_lock_promotion_is_refused_for_now_or_for_good() {
            use LockPromotion::*;
            // Not lock content: as before, whatever the GPU's state.
            assert_eq!(lock_promotion(false, false, false, false), Attach);
            assert_eq!(lock_promotion(false, true, false, false), Attach);
            assert_eq!(lock_promotion(true, true, true, false), Attach);
            // Not lent yet, or the GPU not ready: for now.
            assert_eq!(lock_promotion(true, false, true, false), Refuse);
            assert_eq!(lock_promotion(true, true, false, false), Refuse);
            // Its output renewed in this lock: for good.
            assert_eq!(lock_promotion(true, false, true, true), Keep);
            assert_eq!(lock_promotion(true, false, false, true), Keep);
        }

        /// (lock-gpu review) A stalled thread: once one lock surface's bound
        /// passes, every lock surface the thread holds is given up on (one
        /// with nothing in flight would otherwise be sent a frame and wait
        /// a bound of its own), and no lock content goes to the thread
        /// until it answers again; any reply clears that.
        #[test]
        fn a_stalled_thread_gives_up_every_lock_surface_until_it_answers() {
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            let t0 = std::time::Instant::now() - super::super::LOCK_GPU_BOUND * 2;
            let lock = |asked: Option<std::time::Instant>| Attachment {
                mode: asked.is_none().then_some(GpuMode::Present),
                lent: true,
                lock: true,
                asked,
                ..Attachment::default()
            };
            host.attached.insert(SurfaceId(1), lock(Some(t0)));
            host.attached.insert(SurfaceId(2), lock(None));
            host.presented.insert(SurfaceId(2), presented(None));
            host.attached.insert(
                SurfaceId(3),
                Attachment {
                    lent: true,
                    ..Attachment::default()
                },
            );
            let now = std::time::Instant::now();
            assert!(host.lock_stall(now));
            assert!(host.lock_stalled);
            let given_up: Vec<_> = host
                .attached
                .iter()
                .filter(|(_, a)| lock_gives_up(a, true, false).is_some())
                .map(|(id, _)| id.0)
                .collect();
            assert_eq!(given_up, [1, 2], "every lock surface, no panel");
            // It answers: alive, and timed from that answer.
            host.answer(now);
            assert!(!host.lock_stalled);
            assert!(!host.lock_stall(now));
            assert!(host.lock_stall(now + super::super::LOCK_GPU_BOUND));
        }

        /// (lock-gpu) The loop wakes at a lock surface's bound with nothing
        /// else to wake it: a stalled GPU thread sends no reply.
        #[test]
        fn a_lock_surface_in_flight_wakes_the_loop_at_its_bound() {
            let (ping, _source) = calloop::ping::make_ping().unwrap();
            let mut host = GpuHost::new(ping);
            let font = std::fs::read(strand_text::test_font_path()).unwrap();
            let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
                std::sync::Arc::new(font),
            ]));
            let r = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
            let s = SurfaceId(5);
            assert_eq!(host.wait(&r), None);
            host.attached.insert(
                s,
                Attachment {
                    lent: true,
                    lock: true,
                    asked: Some(std::time::Instant::now()),
                    ..Attachment::default()
                },
            );
            let w = host.wait(&r).expect("woken at the bound");
            assert!(w <= super::super::LOCK_GPU_BOUND, "{w:?}");
            host.attached.get_mut(&s).unwrap().releasing = true;
            assert_eq!(host.wait(&r), None, "released: nothing to wait for");
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

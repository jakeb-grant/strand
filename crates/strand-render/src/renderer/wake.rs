//! Waking the render loop: the timer thread for its own due times (a
//! tooltip's delay, a stalled exit) and [`Renderer::update`], which the
//! host runs on every wake.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use strand_scene::{NodeId, Size, SurfaceId};

use super::Renderer;
use super::text::TextSlot;

/// Starts the thread that wakes the render loop at the latest due time
/// it was sent; a newer one replaces the one waited for, and `None`
/// cancels it. It ends when the renderer (the sender) goes.
pub(super) fn spawn_timer(waker: LoopWaker) -> Option<std::sync::mpsc::Sender<Option<Instant>>> {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel::<Option<Instant>>();
    std::thread::Builder::new()
        .name("strand-tooltip".into())
        .spawn(move || {
            let mut due: Option<Instant> = None;
            loop {
                let next = match due {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
                };
                match next {
                    Ok(d) => due = d,
                    Err(RecvTimeoutError::Timeout) => {
                        due = None;
                        if let Ok(w) = waker.0.lock() {
                            w();
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .ok()?;
    Some(tx)
}

/// The render loop's waker, callable from any thread.
#[derive(Clone)]
pub(super) struct LoopWaker(pub(super) Arc<std::sync::Mutex<strand_text::Waker>>);

impl std::fmt::Debug for LoopWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoopWaker")
    }
}

impl Renderer {
    /// Arms the render loop's timer at [`Renderer::next_wake`]: a
    /// tooltip waiting to show, or an exit on a surface whose output
    /// stopped sending frames. Its wake calls [`Renderer::update`] (the
    /// host's waker handler), so hosts need no timer of their own. A
    /// later due time than the one armed is not sent: the earlier wake
    /// re-arms (at most one wake per `EXIT_STALL` while exits play on a
    /// painting surface). Nothing left to wake for cancels the timer, so
    /// an idle shell is not woken. The paint cache's idle entries are
    /// freed at the next paint or wake that comes anyway, never by a
    /// wake of their own (an idle shell does zero work: a clocked bar
    /// would otherwise wake after every tick to free its shadow).
    pub(super) fn arm_timer(&mut self) {
        let Some(due) = self.next_wake() else {
            if self.timer_due.take().is_some()
                && let Some(tx) = &self.timer
            {
                // A failed send means the thread is gone: nothing to cancel.
                let _ = tx.send(None);
            }
            return;
        };
        if self.timer_due.is_some_and(|t| t <= due) {
            return;
        }
        if self.timer.is_none() {
            self.timer = self.waker.clone().and_then(spawn_timer);
        }
        match &self.timer {
            Some(tx) if tx.send(Some(due)).is_ok() => self.timer_due = Some(due),
            _ => {
                self.timer = None;
                self.timer_due = None;
            }
        }
    }

    /// When the host's loop must run [`Renderer::update`] even if
    /// nothing else happens: the earliest instant an exit in flight is
    /// ended for want of frames (its output asleep, see [`EXIT_STALL`]),
    /// so a closing surface whose frames stopped still closes and its
    /// ghosts unmount, a tooltip's delay ends, or a capped clock (M4:
    /// `effect shimmer` at 30 fps) is due its next tick (half a frame
    /// before it; after that `update`, the surface's
    /// [`strand_scene::Painter::wants_frame`] is true). `None` when nothing
    /// waits. The renderer arms its own timer thread at it after every
    /// `apply`, `update` and paint and wakes the loop through the text
    /// worker's waker, so a host whose waker handler runs `update` needs
    /// no timer of its own; a host without a waker (an inline text
    /// backend) arms one here.
    pub fn next_wake(&self) -> Option<Instant> {
        let stall = self.exit_stall;
        let tooltip = self
            .tooltip
            .as_ref()
            .filter(|t| t.popup.is_none())
            .map(|t| t.due);
        let exits = self
            .anim
            .exits()
            .filter_map(|(id, _)| {
                let started = self.anim.exit_started(id)?;
                let root = self.tree.root_of(id);
                let last = self
                    .surfaces
                    .values()
                    .filter(|s| Some(s.root) == root)
                    .filter_map(|s| s.painted_at)
                    .max();
                let asleep = last.map_or(started, |t| t.max(started)) + stall;
                Some(asleep.min(started + strand_scene::motion::MAX_MOTION + stall))
            })
            .min();
        // A capped clock's next tick (M4), half a frame early.
        let clocks = self.clocks.next_wake();
        [exits, tooltip, clocks].into_iter().flatten().min()
    }

    /// Collects finished text layouts and sends shaping requests for text
    /// that changed. Never blocks. Call after the text worker's waker fires.
    ///
    /// A dirty surface whose flattened scene turns out identical to its
    /// last painted frame (only text still being shaped changed) stops
    /// being dirty, so it asks for no frame until the layout arrives.
    pub fn update(&mut self) {
        let now = Instant::now();
        if self.timer_due.is_some_and(|d| d <= now) {
            // Fired (or about to): the thread waits for a new due time.
            self.timer_due = None;
        }
        self.raster.trim_idle(now);
        // A capped clock's tick has come: its surface wants a frame.
        self.clocks.woke(now);
        self.show_tooltip();
        self.poll_text();
        self.expire_exits();
        self.process_finished();
        self.refresh_specs();
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.dirty && s.cache.is_none() && s.size != Size::default())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(s) = self.surfaces.get(&id) {
                // A preview: nothing starts until a frame samples it.
                let (time, prev) = (s.time, s.painted_time);
                self.anim.set_slack(self.clocks.slack(id));
                self.sample_tokens(time, false);
                self.anim.begin(time, prev, false);
            }
            let f = self.flatten_surface(id);
            let animating = self.anim.active() || self.swap_moving(id) || self.scrolling(id);
            // Text with a request in flight and no layout for this
            // surface's scale and width yet. A stand-in from another scale
            // or width does not count: a first frame drawn with one would
            // be repainted as soon as the right layout lands.
            let texts = &self.texts;
            let waiting: Vec<NodeId> = f
                .text
                .iter()
                .filter(|(node, spec)| {
                    texts
                        .get(&TextSlot::of(*node, spec))
                        .is_some_and(|t| t.layout.is_none() && t.requested.is_some())
                })
                .map(|(node, _)| *node)
                .collect();
            // Of those, text with no layout anywhere to stand in.
            let blank = !waiting.is_empty() && {
                let shown: HashSet<NodeId> = texts
                    .iter()
                    .filter(|(_, t)| t.layout.is_some())
                    .map(|(slot, _)| slot.node)
                    .collect();
                waiting.iter().any(|n| !shown.contains(n))
            };
            let wait = self.new_text_wait;
            let window = self.busy_window;
            if let Some(s) = self.surfaces.get_mut(&id) {
                if s.valid && s.records == f.records && s.opaque == f.opaque {
                    s.dirty = false;
                }
                s.animating = animating;
                s.awaiting_text = !waiting.is_empty();
                // A surface in motion holds nothing (see BUSY_WINDOW).
                let now = Instant::now();
                let idle = s
                    .painted_at
                    .is_none_or(|t| now.saturating_duration_since(t) >= window);
                s.new_text_until = match (blank, s.new_text_until) {
                    (false, _) => None,
                    (true, Some(t)) => Some(t),
                    (true, None) if s.painted && idle && !wait.is_zero() => Some(now + wait),
                    (true, None) => None,
                };
                s.cache = Some(f);
            }
        }
        // A preview that laid out a change moving nothing (a row removed
        // at the end, a size set `~ instant`) lets a held surface shrink.
        self.release_holds();
        self.arm_timer();
    }
}

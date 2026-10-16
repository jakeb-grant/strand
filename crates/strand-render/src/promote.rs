//! (M4; gpu-first) When a surface moves to the GPU and back, and the
//! device's lifecycle (architecture.md, "`strand-gpu`", Promotion). Pure
//! state machines: the renderer feeds them each frame's damage and
//! whether a spring is in flight, with the time, and carries out what
//! they answer.
//!
//! The rule (decisions.md gpu-first, the owner's "Why wouldn't we default
//! to GPU for everything if it were available?"): when a device can
//! exist, whatever animates a large area draws on the GPU.
//!
//! - A surface is promoted at the first large frame (at least
//!   [`LARGE_DAMAGE`] pixels) of an animation: one with a spring in
//!   flight (a morph, a panel opening, a scroll, a theme crossfade), or
//!   the [`CLOCK_RUN`]th large frame in a row, each within [`RUN_GAP`] of
//!   the one before, with none (a clock: shader time, particles). That
//!   frame follows a still one, so the backends' antialiasing difference
//!   is lost in the motion. A lone large frame (new content, a first
//!   frame with nothing moving) is not an animation; nor is the frame on
//!   which a surface's last spring comes to rest, which still diffs large
//!   against the one before (the whole node is damaged) with nothing
//!   after it (m4-audit).
//! - It goes back once settled for [`SETTLE_AFTER`]: every frame since
//!   its last animated one (large, or with a spring in flight) small, or
//!   none at all. A frame with a spring in flight never switches back, so
//!   the switch lands on a still frame. A promoted surface that paints
//!   nothing goes back at a wake of its own ([`Promotion::wake`]): one
//!   wake after an animation ends, none while idle. So a bar ticking a
//!   clock draws on the CPU with exact damage.
//! - The device ([`Device`]) is kept up for the whole process once
//!   started: the binary starts it when the shell starts
//!   (`Renderer::warm_gpu`), off the frame path, so an animation never
//!   waits for a device or its pipelines. A failed start or a lost device
//!   is not asked for again for [`RETRY_AFTER`]; after [`LOST_CAP`] lost
//!   devices the GPU is off for the rest of the process (owner,
//!   m4-close-gpucap): no device is asked for again, and nothing is
//!   promoted.

use std::time::{Duration, Instant};

/// The longest pause between two large frames that keeps them one run
/// (many dropped frames at any refresh rate, a slow debug frame
/// included).
pub const RUN_GAP: Duration = Duration::from_millis(250);

/// Damage that counts as a large frame: 0.2 Mpx, where a CPU frame of
/// the design launcher costs about 2 ms (15 ms for its 1.35 Mpx); below
/// it the GPU's fixed costs (lowering, a full-damage present the
/// compositor recomposites) buy nothing a CPU frame does not have.
pub const LARGE_DAMAGE: u64 = 200_000;

/// Large frames in a row, with no spring in flight, that make a clock's
/// animation (a lone large frame, or two, is new content).
pub const CLOCK_RUN: u32 = 3;

/// How long a promoted surface must be settled before it goes back.
pub const SETTLE_AFTER: Duration = Duration::from_secs(3);

/// How long after a failed start or a lost device the device is asked
/// for again.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Lost devices (a hang, a reset, a panic: `GpuReply::Lost`) after which
/// the GPU stays off until strand restarts.
pub const LOST_CAP: u32 = 3;

/// Why the GPU is off once [`LOST_CAP`] devices were lost (the status's
/// reason, and so the inspector's, `strand report`'s and the notice's).
pub const GPU_OFF: &str =
    "the GPU was turned off after 3 lost devices; restarting strand brings it back";

/// A switch [`Promotion`] decided.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Switch {
    ToGpu,
    ToCpu,
}

/// One surface's promotion state.
#[derive(Clone, Debug, Default)]
pub struct Promotion {
    /// On the GPU (promoted, or asked to be).
    gpu: bool,
    /// When the surface last painted a frame.
    last: Option<Instant>,
    /// Large frames in a row, each within [`RUN_GAP`] of the one before.
    run: u32,
    /// The last frame had springs in flight.
    sprung: bool,
    /// The last frame animated (large, or springs in flight).
    hot: bool,
    /// On the GPU: when it last animated (or was promoted, or showed an
    /// animated frame). Its settle window runs from here.
    busy: Option<Instant>,
    /// [`SETTLE_AFTER`] unless a test shortens it.
    settle: Option<Duration>,
}

impl Promotion {
    /// True while promoted (or asked to be).
    pub fn on_gpu(&self) -> bool {
        self.gpu
    }

    /// Shortens [`SETTLE_AFTER`] (tests).
    pub fn set_settle(&mut self, settle: Duration) {
        self.settle = Some(settle);
    }

    fn settle(&self) -> Duration {
        self.settle.unwrap_or(SETTLE_AFTER)
    }

    /// A frame at `now` that damaged `damage` pixels, with `springs`
    /// in flight on the surface. Returns the switch to make now. A paint
    /// that damaged nothing is not a frame (nothing is committed): a
    /// clocked shader whose pass result comes every other paint does not
    /// break its run.
    pub fn frame(&mut self, now: Instant, damage: u64, springs: bool) -> Option<Switch> {
        if damage == 0 {
            return None;
        }
        // The frame where the springs came to rest: an animation driven
        // by springs alone ends here, though its damage is still large.
        let settling = std::mem::replace(&mut self.sprung, springs) && !springs;
        let large = damage >= LARGE_DAMAGE;
        let close = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) <= RUN_GAP);
        self.run = match (large, close) {
            (false, _) => 0,
            (true, true) => self.run.saturating_add(1),
            (true, false) => 1,
        };
        self.last = Some(now);
        self.hot = large || springs;
        if self.gpu {
            if self.hot {
                self.busy = Some(now);
                return None;
            }
            // Settled long enough: back at this still frame.
            let settled = self
                .busy
                .is_none_or(|t| now.saturating_duration_since(t) >= self.settle());
            return settled.then(|| self.switch(now));
        }
        let animates = springs || (self.run >= CLOCK_RUN && !settling);
        (large && animates).then(|| self.switch(now))
    }

    fn switch(&mut self, now: Instant) -> Switch {
        self.gpu = !self.gpu;
        if self.gpu {
            self.busy = Some(now);
            Switch::ToGpu
        } else {
            self.busy = None;
            Switch::ToCpu
        }
    }

    /// When a promoted surface that paints nothing more is settled long
    /// enough to go back ([`Promotion::idle`] then demotes it).
    pub fn wake(&self) -> Option<Instant> {
        if !self.gpu {
            return None;
        }
        self.busy.map(|t| t + self.settle())
    }

    /// The loop woke at `now` with no frame painted since
    /// [`Promotion::wake`]: a promoted surface goes back.
    pub fn idle(&mut self, now: Instant) -> Option<Switch> {
        match self.wake() {
            Some(t) if now >= t => {
                self.force_cpu();
                Some(Switch::ToCpu)
            }
            _ => None,
        }
    }

    /// A frame of the promoted surface reached the screen at `now` (a
    /// presented frame's `Presented`, or the attach): if it animated, its
    /// settle window starts there, not when it was painted (lavapipe's
    /// first frame compiles for longer than a settle window's share).
    pub fn shown(&mut self, now: Instant) {
        if self.gpu && self.hot {
            self.busy = Some(self.busy.map_or(now, |t| t.max(now)));
        }
    }

    /// On the GPU at once, busy from `now` (a test seam:
    /// `Renderer::promote_now`).
    pub fn force_gpu(&mut self, now: Instant) {
        self.gpu = true;
        self.hot = true;
        self.busy = Some(now);
    }

    /// Back on the CPU at once (a lost device, a failed start, a settled
    /// surface): no frame is waited for.
    pub fn force_cpu(&mut self) {
        self.gpu = false;
        self.busy = None;
    }
}

/// The device's lifecycle as render sees it.
#[derive(Clone, Debug, Default)]
pub struct Device {
    state: State,
    /// [`RETRY_AFTER`] unless a test shortens it.
    retry: Option<Duration>,
    /// Devices lost so far in this process.
    lost: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum State {
    #[default]
    Unused,
    Starting,
    Up,
    /// A failed start or a lost device, at that instant.
    Failed(Instant),
    /// [`LOST_CAP`] devices were lost: never asked for again.
    Off,
}

impl Device {
    /// Something needs the device at `now` (a promotion, a pass, the warm
    /// start). True if it may be used (it is up, or starting, or this
    /// asks for it); false while a failure is less than [`RETRY_AFTER`]
    /// old, and once the GPU is off.
    pub fn want(&mut self, now: Instant) -> bool {
        match self.state {
            State::Off => false,
            State::Failed(at)
                if now.saturating_duration_since(at) < self.retry.unwrap_or(RETRY_AFTER) =>
            {
                false
            }
            State::Unused | State::Failed(_) => {
                self.state = State::Starting;
                true
            }
            State::Starting | State::Up => true,
        }
    }

    /// True if [`Device::want`] would ask for a start.
    pub fn idle(&self) -> bool {
        matches!(self.state, State::Unused | State::Failed(_))
    }

    /// The device answered: up.
    pub fn up(&mut self) {
        if self.state == State::Starting {
            self.state = State::Up;
        }
    }

    /// True once up.
    pub fn is_up(&self) -> bool {
        self.state == State::Up
    }

    /// True while starting.
    pub fn starting(&self) -> bool {
        self.state == State::Starting
    }

    /// The start failed or the device was lost at `now`.
    pub fn failed(&mut self, now: Instant) {
        if self.state != State::Off {
            self.state = State::Failed(now);
        }
    }

    /// The device was lost at `now` (a hang, a reset, a panic). True if
    /// this loss turned the GPU off (the [`LOST_CAP`]th): it says so once.
    pub fn lost(&mut self, now: Instant) -> bool {
        self.lost = self.lost.saturating_add(1);
        if self.state == State::Off {
            return false;
        }
        if self.lost >= LOST_CAP {
            self.state = State::Off;
            return true;
        }
        self.failed(now);
        false
    }

    /// (M5) Devices lost so far in this process (`strand report`).
    pub fn losses(&self) -> u32 {
        self.lost
    }

    /// True once the GPU is off for the rest of the process.
    pub fn is_off(&self) -> bool {
        self.state == State::Off
    }

    /// The GPU thread ended (`Exited`) without a failure being recorded.
    pub fn dropped(&mut self) {
        if matches!(self.state, State::Starting | State::Up) {
            self.state = State::Unused;
        }
    }

    /// Shortens [`RETRY_AFTER`] (tests).
    pub fn set_retry(&mut self, retry: Duration) {
        self.retry = Some(retry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(16);

    /// Frames every 16 ms from `t` for `span`, `damage` each.
    fn run(
        p: &mut Promotion,
        t: &mut Instant,
        span: Duration,
        damage: u64,
        springs: bool,
    ) -> Vec<Switch> {
        let end = *t + span;
        let mut out = Vec::new();
        while *t < end {
            *t += FRAME;
            out.extend(p.frame(*t, damage, springs));
        }
        out
    }

    /// (gpu-first) The owner's launcher springs its card's size on every
    /// mode switch: the first large frame with the spring in flight
    /// promotes it, where 3a638d14 waited for 500 ms of large frames and
    /// then for a frame with no spring in flight, so a spring-driven
    /// morph never reached the GPU.
    #[test]
    fn a_spring_driven_morph_is_promoted_at_its_first_large_frame() {
        let mut p = Promotion::default();
        let t = Instant::now();
        // Still: a small frame (a caret), nothing in flight.
        assert_eq!(p.frame(t, 2_000, false), None);
        // The mode switch: the card's size springs.
        assert_eq!(
            p.frame(t + FRAME, 900_000, true),
            Some(Switch::ToGpu),
            "the first large frame of the spring"
        );
        assert!(p.on_gpu());
        // A morph shorter than the old 500 ms window, settling on a
        // large frame: it stays on the GPU.
        let mut t = t + FRAME;
        assert!(run(&mut p, &mut t, Duration::from_millis(300), 900_000, true).is_empty());
        t += FRAME;
        assert_eq!(p.frame(t, 900_000, false), None, "the settling frame");
        assert!(p.on_gpu());
    }

    /// A panel that opens with a transition is promoted on its first
    /// frame (all of it damaged, its enter spring in flight).
    #[test]
    fn a_panel_opening_is_promoted_at_its_first_frame() {
        let mut p = Promotion::default();
        assert_eq!(
            p.frame(Instant::now(), 1_350_000, true),
            Some(Switch::ToGpu)
        );
    }

    /// Large frames with nothing moving are new content, not an
    /// animation: a first frame, the text worker's layout after it, a
    /// list refilled. A clock's animation is promoted at its third large
    /// frame in a row; a pause past `RUN_GAP` starts the run over.
    #[test]
    fn a_clock_is_promoted_at_its_third_large_frame_in_a_row() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.frame(t, 1_000_000, false), None, "a first frame");
        t += FRAME;
        assert_eq!(p.frame(t, 1_000_000, false), None, "its text laid out");
        // Seconds later, two more: a pause breaks the run.
        t += Duration::from_secs(2);
        assert_eq!(p.frame(t, 1_000_000, false), None);
        t += RUN_GAP + FRAME;
        assert_eq!(p.frame(t, 1_000_000, false), None);
        // A small frame breaks it too.
        t += FRAME;
        assert_eq!(p.frame(t, 1_000, false), None);
        // A clock: the third large frame in a row (a dropped frame or
        // two within RUN_GAP keep the run).
        t += FRAME;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false), None);
        t += RUN_GAP;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false), None);
        t += FRAME;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false), Some(Switch::ToGpu));
        // Just under 0.2 Mpx is not large, however long it runs.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        assert!(
            run(
                &mut q,
                &mut t,
                Duration::from_secs(2),
                LARGE_DAMAGE - 1,
                true
            )
            .is_empty()
        );
        assert!(!q.on_gpu());
    }

    #[test]
    fn a_paint_with_no_damage_is_not_a_frame() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        let mut switched = None;
        for i in 0..8 {
            t += Duration::from_millis(8);
            let damage = if i % 2 == 0 { LARGE_DAMAGE } else { 0 };
            switched = switched.or(p.frame(t, damage, false));
        }
        assert_eq!(switched, Some(Switch::ToGpu));
    }

    /// (m4-audit) A spring that ends on a large frame (its node moved by
    /// less than the settle tolerance, the whole node damaged) when the
    /// surface could not be promoted while it ran (no device yet): that
    /// frame does not promote, since nothing follows it. A clock that
    /// goes on after the springs settle promotes on its next large frame.
    #[test]
    fn a_spring_that_settles_on_a_large_frame_is_not_promoted() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        let got = run(&mut p, &mut t, Duration::from_millis(600), 1_000_000, true);
        assert_eq!(got[0], Switch::ToGpu);
        p.force_cpu(); // no device: render takes it back at once
        assert!(run(&mut p, &mut t, Duration::from_millis(200), 1_000, true).is_empty());
        t += FRAME;
        assert_eq!(p.frame(t, 1_000_000, false), None, "a lone large frame");
        // Large frames all through the spring, the device refused each.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if q.frame(t, 1_000_000, true) == Some(Switch::ToGpu) {
                q.force_cpu();
            }
        }
        t += FRAME;
        assert_eq!(q.frame(t, 1_000_000, false), None, "the settling frame");
        assert!(!q.on_gpu());
        // Nothing follows: the next paint comes after a pause.
        t += Duration::from_secs(2);
        assert_eq!(q.frame(t, 1_000_000, false), None);
        // With a clock still running, the next large frame promotes.
        let mut c = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if c.frame(t, 1_000_000, true) == Some(Switch::ToGpu) {
                c.force_cpu();
            }
        }
        t += FRAME;
        assert_eq!(c.frame(t, 1_000_000, false), None, "the settling frame");
        t += FRAME;
        assert_eq!(c.frame(t, 1_000_000, false), Some(Switch::ToGpu));
    }

    /// Back on the CPU only after `SETTLE_AFTER` of small frames (a clock
    /// ticking), at a frame with no spring in flight; any animated frame
    /// starts the window over.
    #[test]
    fn a_settled_surface_goes_back_after_the_settle_period() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.frame(t, 900_000, true), Some(Switch::ToGpu));
        // A second's ticks, then the morph again: still promoted.
        let tick = Duration::from_secs(1);
        for _ in 0..2 {
            t += tick;
            assert_eq!(p.frame(t, 1_200, false), None);
        }
        t += FRAME;
        assert_eq!(p.frame(t, 900_000, true), None, "animating again");
        let busy = t;
        // Ticks until the settle period has passed since the morph.
        for _ in 0..2 {
            t += tick;
            assert_eq!(p.frame(t, 1_200, false), None);
        }
        // A small spring in flight (a hover) is not settled.
        t = busy + SETTLE_AFTER;
        assert_eq!(p.frame(t, 1_200, true), None);
        assert!(p.on_gpu());
        t += SETTLE_AFTER;
        assert_eq!(p.frame(t, 1_200, false), Some(Switch::ToCpu));
        assert!(!p.on_gpu());
        assert_eq!(p.wake(), None);
    }

    /// A promoted surface that paints nothing goes back at one wake,
    /// `SETTLE_AFTER` after its last animated frame; on the CPU it asks
    /// for none (an idle shell takes no wakeups).
    #[test]
    fn an_idle_promoted_surface_goes_back_at_its_wake() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.wake(), None, "on the CPU, no wake");
        run(&mut p, &mut t, Duration::from_millis(200), 300_000, true);
        assert!(p.on_gpu());
        let wake = p.wake().expect("a promoted surface has a wake");
        assert_eq!(wake, t + SETTLE_AFTER);
        assert_eq!(p.idle(wake - FRAME), None);
        assert_eq!(p.idle(wake), Some(Switch::ToCpu));
        assert_eq!(p.wake(), None, "and no wake after it");
        // A test's shorter window.
        let mut q = Promotion::default();
        q.set_settle(Duration::from_millis(100));
        let t = Instant::now();
        q.force_gpu(t);
        assert_eq!(q.wake(), Some(t + Duration::from_millis(100)));
    }

    /// (m4-integration-w2) An animated frame reaching the screen late
    /// moves the settle window to its `Presented`; a still one does not,
    /// and on the CPU it changes nothing.
    #[test]
    fn a_shown_frame_restarts_the_settle_window() {
        let t = Instant::now();
        let mut p = Promotion::default();
        p.shown(t);
        assert_eq!(p.wake(), None, "on the CPU");
        assert_eq!(p.frame(t, LARGE_DAMAGE, true), Some(Switch::ToGpu));
        assert_eq!(p.wake(), Some(t + SETTLE_AFTER));
        let late = t + Duration::from_millis(800);
        p.shown(late);
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER));
        assert_eq!(p.idle(t + SETTLE_AFTER), None, "not idle at the old wake");
        p.shown(t);
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER), "never earlier");
        // A still frame shown later does not move it.
        assert_eq!(p.frame(late + FRAME, 100, false), None);
        p.shown(late + Duration::from_secs(1));
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER));
        assert_eq!(p.idle(late + SETTLE_AFTER), Some(Switch::ToCpu));
    }

    #[test]
    fn a_forced_demotion_waits_for_nothing() {
        let mut p = Promotion::default();
        let t = Instant::now();
        assert_eq!(p.frame(t, 300_000, true), Some(Switch::ToGpu));
        p.force_cpu();
        assert!(!p.on_gpu());
        assert_eq!(p.wake(), None);
    }

    /// (gpu-first) The device stays up once started: nothing in it
    /// drops it, however long it goes unused; only a failure or the
    /// thread's end takes it down.
    #[test]
    fn the_device_is_kept_up_when_idle() {
        let mut d = Device::default();
        let t0 = Instant::now();
        assert!(d.idle());
        assert!(d.want(t0), "the warm start");
        assert!(d.starting());
        d.up();
        assert!(d.is_up());
        assert!(d.want(t0 + Duration::from_secs(86_400)), "a day later");
        assert!(d.is_up() && !d.idle(), "still up, no new start");
        d.dropped();
        assert!(d.idle(), "the thread ended: the next want starts one");
    }

    #[test]
    fn a_failure_is_retried_at_most_once_per_30s() {
        let mut d = Device::default();
        let t0 = Instant::now();
        assert!(d.want(t0));
        d.failed(t0 + Duration::from_millis(100));
        assert!(!d.want(t0 + Duration::from_secs(10)));
        assert!(!d.want(t0 + Duration::from_secs(30)));
        assert!(d.want(t0 + Duration::from_millis(30_100)));
        assert!(d.starting());
        // A lost device too.
        d.up();
        d.failed(t0 + Duration::from_secs(40));
        assert!(!d.want(t0 + Duration::from_secs(41)));
    }

    /// (m4-close-gpucap) The third lost device turns the GPU off, not the
    /// first two: each of those is retried after `RETRY_AFTER` as any
    /// failure is; after the third nothing is asked for again, however
    /// long the wait, a failed start does not undo it, and it is said
    /// once.
    #[test]
    fn the_third_lost_device_turns_the_gpu_off() {
        let mut d = Device::default();
        let mut t = Instant::now();
        for loss in 1..LOST_CAP {
            assert!(d.want(t), "loss {loss}: asked for");
            d.up();
            assert!(!d.lost(t), "loss {loss} does not turn it off");
            assert!(!d.is_off());
            assert!(!d.want(t + Duration::from_secs(1)), "within RETRY_AFTER");
            t += RETRY_AFTER + Duration::from_millis(1);
        }
        assert!(d.want(t), "asked for after the second loss's wait");
        d.up();
        assert!(d.lost(t), "the third loss turns it off");
        assert!(d.is_off());
        assert!(!d.idle(), "no start to ask for");
        for later in [RETRY_AFTER * 2, Duration::from_secs(86_400)] {
            assert!(!d.want(t + later), "never asked for again");
        }
        d.failed(t);
        d.dropped();
        assert!(d.is_off(), "off for the process");
        assert!(!d.lost(t), "said once");
        assert!(!d.want(t + Duration::from_secs(86_400)));
        assert!(GPU_OFF.contains(&LOST_CAP.to_string()));
    }
}

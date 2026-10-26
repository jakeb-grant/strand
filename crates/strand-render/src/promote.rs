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
//!   its last heavy one small, or none at all. Settled is a matter of
//!   damage, not of springs (gpu-first review): a small spring that
//!   never rests (an album cover turning with `t`) does not hold a
//!   surface on the GPU, unless the CPU was measured drawing such a
//!   frame of it slowly (at least [`CHEAP_FRAME`]): there the GPU is the
//!   faster of the two even for a small frame, and it stays. A promoted
//!   surface that paints nothing goes back at a wake of its own
//!   ([`Promotion::wake`]): one wake after an animation ends, none while
//!   idle. So a bar ticking a clock draws on the CPU with exact damage.
//!   A surface promoted again within [`AGAIN_WITHIN`] of going back
//!   settles for [`AGAIN_FACTOR`] times as long: one that animates every
//!   few seconds (a launcher switching modes) keeps its swapchain, so its
//!   next animation's first frames are not the CPU's while it attaches.
//! - The device ([`Device`]) is kept up for the whole process once
//!   started: the binary starts it when the shell starts
//!   (`Renderer::warm_gpu`), off the frame path, so an animation never
//!   waits for a device or its pipelines. A failed start or a lost device
//!   is not asked for again for [`RETRY_AFTER`]; after [`LOST_CAP`] lost
//!   devices the GPU is off for the rest of the process (owner,
//!   m4-close-gpucap): no device is asked for again, and nothing is
//!   promoted.

use std::time::Duration;
use strand_scene::time::Instant;

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

/// A surface promoted again this soon after it went back...
pub const AGAIN_WITHIN: Duration = Duration::from_secs(30);

/// ...settles this many times [`SETTLE_AFTER`] (15 s) before it goes
/// back again.
pub const AGAIN_FACTOR: u32 = 5;

/// A small frame with springs in flight that the CPU drew in at least
/// this long (half a 120 Hz frame) is heavy: a promoted surface whose
/// small animated frames are that slow on the CPU stays on the GPU while
/// they last.
pub const CHEAP_FRAME: Duration = Duration::from_millis(4);

/// The small spring frames whose CPU times are kept: their median is
/// the surface's cost of one ([`CHEAP_FRAME`]), known once
/// [`SPRING_SAMPLES_MIN`] of them came, so one slow frame (a preempted
/// raster on a loaded host) does not hold a surface on the GPU.
pub const SPRING_SAMPLES: usize = 5;

/// Samples needed before a small spring's cost counts.
pub const SPRING_SAMPLES_MIN: usize = 3;

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
    /// The last frame was heavy: large, or a small one with springs in
    /// flight on a surface whose such frames are slow on the CPU.
    hot: bool,
    /// The CPU's times for this surface's last small frames with springs
    /// in flight that it drew with about their own damage, newest last
    /// ([`SPRING_SAMPLES`] at most).
    spring_costs: std::collections::VecDeque<Duration>,
    /// When it last went back to the CPU, settled.
    back_at: Option<Instant>,
    /// Promoted again within [`AGAIN_WITHIN`] of going back: it settles
    /// for [`AGAIN_FACTOR`] times as long.
    again: bool,
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
        let base = self.settle.unwrap_or(SETTLE_AFTER);
        if self.again {
            base * AGAIN_FACTOR
        } else {
            base
        }
    }

    /// A frame at `now` that damaged `damage` pixels, with `springs`
    /// in flight on the surface; `cpu` is the CPU's time for it when the
    /// CPU drew about that damage (the renderer passes none for a frame
    /// widened past twice it by a buffer's age, or drawn in full).
    /// Returns the switch to make now. A paint that damaged
    /// nothing is not a frame (nothing is committed): a clocked shader
    /// whose pass result comes every other paint does not break its run.
    pub fn frame(
        &mut self,
        now: Instant,
        damage: u64,
        springs: bool,
        cpu: Option<Duration>,
    ) -> Option<Switch> {
        if damage == 0 {
            return None;
        }
        // The frame where the springs came to rest: an animation driven
        // by springs alone ends here, though its damage is still large.
        let settling = std::mem::replace(&mut self.sprung, springs) && !springs;
        let large = damage >= LARGE_DAMAGE;
        if let Some(c) = cpu.filter(|_| !large && springs) {
            if self.spring_costs.len() == SPRING_SAMPLES {
                self.spring_costs.pop_front();
            }
            self.spring_costs.push_back(c);
        }
        let close = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) <= RUN_GAP);
        self.run = match (large, close) {
            (false, _) => 0,
            (true, true) => self.run.saturating_add(1),
            (true, false) => 1,
        };
        self.last = Some(now);
        let slow = springs && self.spring_cost().is_some_and(|c| c >= CHEAP_FRAME);
        self.hot = large || slow;
        if self.gpu {
            if self.hot {
                self.busy = Some(now);
                return None;
            }
            // Settled long enough: back at this small frame.
            let settled = self
                .busy
                .is_none_or(|t| now.saturating_duration_since(t) >= self.settle());
            return settled.then(|| self.switch(now));
        }
        let animates = springs || (self.run >= CLOCK_RUN && !settling);
        (large && animates).then(|| self.switch(now))
    }

    /// The median of the small spring frames' CPU times, once
    /// [`SPRING_SAMPLES_MIN`] came.
    fn spring_cost(&self) -> Option<Duration> {
        if self.spring_costs.len() < SPRING_SAMPLES_MIN {
            return None;
        }
        let mut v: Vec<Duration> = self.spring_costs.iter().copied().collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    fn switch(&mut self, now: Instant) -> Switch {
        self.gpu = !self.gpu;
        if self.gpu {
            self.again = self
                .back_at
                .is_some_and(|t| now.saturating_duration_since(t) <= AGAIN_WITHIN);
            self.busy = Some(now);
            Switch::ToGpu
        } else {
            self.busy = None;
            self.back_at = Some(now);
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
                self.back_at = Some(now);
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
    /// No device can exist in this process (no render node, no Vulkan
    /// adapter, only a software one refused): never asked for again.
    Absent,
}

impl Device {
    /// Something needs the device at `now` (a promotion, a pass, the warm
    /// start). True if it may be used (it is up, or starting, or this
    /// asks for it); false while a failure is less than [`RETRY_AFTER`]
    /// old, and once the GPU is off.
    pub fn want(&mut self, now: Instant) -> bool {
        match self.state {
            State::Off | State::Absent => false,
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

    /// When a failed start or a lost device may be asked for again
    /// ([`RETRY_AFTER`] after it); `None` in any other state.
    pub fn retry_at(&self) -> Option<Instant> {
        match self.state {
            State::Failed(at) => Some(at + self.retry.unwrap_or(RETRY_AFTER)),
            _ => None,
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
        if !matches!(self.state, State::Off | State::Absent) {
            self.state = State::Failed(now);
        }
    }

    /// (gpu-first review) No device can exist in this process: no render
    /// node, no Vulkan adapter, or only a software one, refused. Nothing
    /// asks for one again (a machine that never has a device would
    /// otherwise make an instance, and map its drivers, at every
    /// animation 30 s apart); the GPU's turning off after lost devices
    /// stays what it is.
    pub fn absent(&mut self) {
        if self.state != State::Off {
            self.state = State::Absent;
        }
    }

    /// True once no device can exist in this process.
    pub fn is_absent(&self) -> bool {
        self.state == State::Absent
    }

    /// The device was lost at `now` (a hang, a reset, a panic). True if
    /// this loss turned the GPU off (the [`LOST_CAP`]th): it says so once.
    pub fn lost(&mut self, now: Instant) -> bool {
        self.lost = self.lost.saturating_add(1);
        if matches!(self.state, State::Off | State::Absent) {
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
            out.extend(p.frame(*t, damage, springs, None));
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
        assert_eq!(p.frame(t, 2_000, false, None), None);
        // The mode switch: the card's size springs.
        assert_eq!(
            p.frame(t + FRAME, 900_000, true, None),
            Some(Switch::ToGpu),
            "the first large frame of the spring"
        );
        assert!(p.on_gpu());
        // A morph shorter than the old 500 ms window, settling on a
        // large frame: it stays on the GPU.
        let mut t = t + FRAME;
        assert!(run(&mut p, &mut t, Duration::from_millis(300), 900_000, true).is_empty());
        t += FRAME;
        assert_eq!(p.frame(t, 900_000, false, None), None, "the settling frame");
        assert!(p.on_gpu());
    }

    /// A panel that opens with a transition is promoted on its first
    /// frame (all of it damaged, its enter spring in flight).
    #[test]
    fn a_panel_opening_is_promoted_at_its_first_frame() {
        let mut p = Promotion::default();
        assert_eq!(
            p.frame(Instant::now(), 1_350_000, true, None),
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
        assert_eq!(p.frame(t, 1_000_000, false, None), None, "a first frame");
        t += FRAME;
        assert_eq!(
            p.frame(t, 1_000_000, false, None),
            None,
            "its text laid out"
        );
        // Seconds later, two more: a pause breaks the run.
        t += Duration::from_secs(2);
        assert_eq!(p.frame(t, 1_000_000, false, None), None);
        t += RUN_GAP + FRAME;
        assert_eq!(p.frame(t, 1_000_000, false, None), None);
        // A small frame breaks it too.
        t += FRAME;
        assert_eq!(p.frame(t, 1_000, false, None), None);
        // A clock: the third large frame in a row (a dropped frame or
        // two within RUN_GAP keep the run).
        t += FRAME;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false, None), None);
        t += RUN_GAP;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false, None), None);
        t += FRAME;
        assert_eq!(p.frame(t, LARGE_DAMAGE, false, None), Some(Switch::ToGpu));
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
            switched = switched.or(p.frame(t, damage, false, None));
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
        assert_eq!(
            p.frame(t, 1_000_000, false, None),
            None,
            "a lone large frame"
        );
        // Large frames all through the spring, the device refused each.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if q.frame(t, 1_000_000, true, None) == Some(Switch::ToGpu) {
                q.force_cpu();
            }
        }
        t += FRAME;
        assert_eq!(
            q.frame(t, 1_000_000, false, None),
            None,
            "the settling frame"
        );
        assert!(!q.on_gpu());
        // Nothing follows: the next paint comes after a pause.
        t += Duration::from_secs(2);
        assert_eq!(q.frame(t, 1_000_000, false, None), None);
        // With a clock still running, the next large frame promotes.
        let mut c = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if c.frame(t, 1_000_000, true, None) == Some(Switch::ToGpu) {
                c.force_cpu();
            }
        }
        t += FRAME;
        assert_eq!(
            c.frame(t, 1_000_000, false, None),
            None,
            "the settling frame"
        );
        t += FRAME;
        assert_eq!(c.frame(t, 1_000_000, false, None), Some(Switch::ToGpu));
    }

    /// Back on the CPU only after `SETTLE_AFTER` of small frames (a clock
    /// ticking, a hover's small spring); any large frame starts the
    /// window over.
    #[test]
    fn a_settled_surface_goes_back_after_the_settle_period() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.frame(t, 900_000, true, None), Some(Switch::ToGpu));
        // A second's ticks, then the morph again: still promoted.
        let tick = Duration::from_secs(1);
        for _ in 0..2 {
            t += tick;
            assert_eq!(p.frame(t, 1_200, false, None), None);
        }
        t += FRAME;
        assert_eq!(p.frame(t, 900_000, true, None), None, "animating again");
        let busy = t;
        // Ticks and a hover until the settle period has passed since the
        // morph.
        for _ in 0..2 {
            t += tick;
            assert_eq!(p.frame(t, 1_200, false, None), None);
        }
        t += FRAME;
        assert_eq!(p.frame(t, 1_200, true, None), None, "a hover, unsettled");
        assert!(p.on_gpu());
        // A small spring in flight is settled once the period is over:
        // back at that frame.
        t = busy + SETTLE_AFTER;
        assert_eq!(p.frame(t, 1_200, true, None), Some(Switch::ToCpu));
        assert!(!p.on_gpu());
        assert_eq!(p.wake(), None);
    }

    /// (gpu-first review) Settled is a matter of damage: a promoted
    /// surface whose only motion is a small spring that never rests (the
    /// launcher's album cover, `rotate: t * 20deg`, 80×80 px) goes back
    /// after `SETTLE_AFTER`. d636d051 counted any spring in flight as
    /// animating, so the launcher stayed on the GPU for as long as the
    /// cover turned.
    #[test]
    fn a_perpetual_small_spring_does_not_hold_a_surface_on_the_gpu() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.frame(t, 1_350_000, true, None), Some(Switch::ToGpu));
        let back = run(&mut p, &mut t, SETTLE_AFTER + FRAME * 2, 6_400, true);
        assert_eq!(back, vec![Switch::ToCpu], "back once, after the period");
        assert!(!p.on_gpu());
        // On the CPU the cover turns on, cheaply: nothing promotes it.
        let more = run(&mut p, &mut t, Duration::from_secs(5), 6_400, true);
        assert!(more.is_empty());
    }

    /// (gpu-first review) A small spring the CPU was measured drawing
    /// slowly (at least `CHEAP_FRAME`: the launcher's turning cover,
    /// 13-19 ms a frame on the CPU, where the GPU draws the whole surface
    /// in about 9 ms off the main thread) is not settled: its surface
    /// stays on the GPU while it turns, and goes back once it rests. One
    /// measured fast goes back as any small frame does.
    #[test]
    fn a_small_spring_the_cpu_draws_slowly_keeps_its_surface_on_the_gpu() {
        let slow = Some(Duration::from_millis(13));
        let mut p = Promotion::default();
        let mut t = Instant::now();
        // On the CPU: the cover turns, measured slow; then a mode switch.
        for _ in 0..10 {
            t += FRAME;
            assert_eq!(p.frame(t, 6_400, true, slow), None, "small: not promoted");
        }
        t += FRAME;
        assert_eq!(p.frame(t, 1_350_000, true, slow), Some(Switch::ToGpu));
        // On the GPU (no CPU time) the cover turns on for 10 s: it stays.
        assert!(run(&mut p, &mut t, Duration::from_secs(10), 6_400, true).is_empty());
        assert!(p.on_gpu());
        // Paused: small frames with nothing in flight settle it.
        let rest = t;
        let back = run(&mut p, &mut t, SETTLE_AFTER + FRAME, 1_200, false);
        assert_eq!(back, vec![Switch::ToCpu]);
        assert!(t >= rest + SETTLE_AFTER);
        // The same surface measured fast goes back while it turns.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        let fast = Some(Duration::from_micros(400));
        for _ in 0..10 {
            t += FRAME;
            q.frame(t, 6_400, true, fast);
        }
        t += FRAME;
        assert_eq!(q.frame(t, 1_350_000, true, fast), Some(Switch::ToGpu));
        let back = run(&mut q, &mut t, SETTLE_AFTER + FRAME * 2, 6_400, true);
        assert_eq!(back, vec![Switch::ToCpu]);
        // A large frame's CPU time says nothing about small ones.
        let mut r = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(
            r.frame(t, 1_350_000, true, Some(Duration::from_millis(15))),
            Some(Switch::ToGpu)
        );
        let back = run(&mut r, &mut t, SETTLE_AFTER + FRAME * 2, 6_400, true);
        assert_eq!(back, vec![Switch::ToCpu]);
    }

    /// (gpu-first review 2) A small spring's cost is the median of its
    /// last frames, and counts once `SPRING_SAMPLES_MIN` came: one slow
    /// frame among cheap ones (a raster preempted on a loaded host, the
    /// full repaint after a demotion, which the renderer does not report)
    /// does not hold the surface on the GPU, and neither does a single
    /// sample. 47a3cd04 took the last frame's time alone.
    #[test]
    fn one_slow_spring_frame_does_not_hold_a_surface_on_the_gpu() {
        let fast = Some(Duration::from_micros(400));
        let slow = Some(Duration::from_millis(30));
        let mut p = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..4 {
            t += FRAME;
            p.frame(t, 6_400, true, fast);
        }
        // The outlier is the last frame before the mode switch.
        t += FRAME;
        p.frame(t, 6_400, true, slow);
        t += FRAME;
        assert_eq!(p.frame(t, 1_350_000, true, None), Some(Switch::ToGpu));
        let back = run(&mut p, &mut t, SETTLE_AFTER + FRAME * 2, 6_400, true);
        assert_eq!(back, vec![Switch::ToCpu], "the median is cheap");
        // One slow sample alone is not a cost yet.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        q.frame(t, 6_400, true, slow);
        t += FRAME;
        assert_eq!(q.frame(t, 1_350_000, true, None), Some(Switch::ToGpu));
        let back = run(&mut q, &mut t, SETTLE_AFTER + FRAME * 2, 6_400, true);
        assert_eq!(back, vec![Switch::ToCpu], "one sample is not enough");
        // Mostly slow frames are a cost, whatever one fast one says.
        let mut r = Promotion::default();
        let mut t = Instant::now();
        for c in [slow, fast, slow, slow, slow] {
            t += FRAME;
            r.frame(t, 6_400, true, c);
        }
        t += FRAME;
        assert_eq!(r.frame(t, 1_350_000, true, None), Some(Switch::ToGpu));
        assert!(run(&mut r, &mut t, SETTLE_AFTER * 2, 6_400, true).is_empty());
        assert!(r.on_gpu());
    }

    /// (gpu-first review) A surface promoted again within `AGAIN_WITHIN`
    /// of going back settles `AGAIN_FACTOR` times as long, so a launcher
    /// switching modes every few seconds keeps its swapchain; one
    /// promoted again later settles in `SETTLE_AFTER` again.
    #[test]
    fn a_surface_promoted_again_soon_settles_longer() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(p.frame(t, 900_000, true, None), Some(Switch::ToGpu));
        assert_eq!(p.wake(), Some(t + SETTLE_AFTER));
        t += SETTLE_AFTER;
        assert_eq!(p.idle(t), Some(Switch::ToCpu));
        // A mode switch a second later.
        t += Duration::from_secs(1);
        assert_eq!(p.frame(t, 900_000, true, None), Some(Switch::ToGpu));
        let long = SETTLE_AFTER * AGAIN_FACTOR;
        assert_eq!(p.wake(), Some(t + long));
        // Small frames for SETTLE_AFTER do not take it back now.
        let start = t;
        let back = run(&mut p, &mut t, long - FRAME * 2, 1_200, false);
        assert!(back.is_empty(), "held for the longer period");
        let back = run(&mut p, &mut t, FRAME * 3, 1_200, false);
        assert_eq!(back, vec![Switch::ToCpu]);
        assert!(t >= start + long);
        // Promoted again after AGAIN_WITHIN: the usual period.
        t += AGAIN_WITHIN + FRAME;
        assert_eq!(p.frame(t, 900_000, true, None), Some(Switch::ToGpu));
        assert_eq!(p.wake(), Some(t + SETTLE_AFTER));
        // A test's shorter window scales the same way.
        let mut q = Promotion::default();
        q.set_settle(Duration::from_millis(100));
        let t = Instant::now();
        assert_eq!(q.frame(t, 900_000, true, None), Some(Switch::ToGpu));
        let t = t + Duration::from_millis(100);
        assert_eq!(q.idle(t), Some(Switch::ToCpu));
        assert_eq!(q.frame(t + FRAME, 900_000, true, None), Some(Switch::ToGpu));
        assert_eq!(q.wake(), Some(t + FRAME + Duration::from_millis(500)));
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
        assert_eq!(p.frame(t, LARGE_DAMAGE, true, None), Some(Switch::ToGpu));
        assert_eq!(p.wake(), Some(t + SETTLE_AFTER));
        let late = t + Duration::from_millis(800);
        p.shown(late);
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER));
        assert_eq!(p.idle(t + SETTLE_AFTER), None, "not idle at the old wake");
        p.shown(t);
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER), "never earlier");
        // A still frame shown later does not move it.
        assert_eq!(p.frame(late + FRAME, 100, false, None), None);
        p.shown(late + Duration::from_secs(1));
        assert_eq!(p.wake(), Some(late + SETTLE_AFTER));
        assert_eq!(p.idle(late + SETTLE_AFTER), Some(Switch::ToCpu));
    }

    #[test]
    fn a_forced_demotion_waits_for_nothing() {
        let mut p = Promotion::default();
        let t = Instant::now();
        assert_eq!(p.frame(t, 300_000, true, None), Some(Switch::ToGpu));
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

    /// (gpu-first review) A machine where no device can exist (no
    /// render node, no adapter, only a software one refused) is never
    /// asked for one again, however long it runs: d636d051 retried every
    /// 30 s of demand, and with springs promoting at once that was every
    /// animation, each one an instance and its drivers mapped.
    #[test]
    fn a_machine_with_no_device_is_never_asked_again() {
        let mut d = Device::default();
        let t = Instant::now();
        assert!(d.want(t), "the warm start");
        d.absent();
        assert!(d.is_absent() && !d.idle() && !d.is_off());
        for later in [RETRY_AFTER * 2, Duration::from_secs(86_400)] {
            assert!(!d.want(t + later), "never asked for again");
        }
        d.failed(t);
        d.dropped();
        assert!(!d.lost(t));
        assert!(d.is_absent(), "for the process");
        // Known before any start (no render node): the same.
        let mut e = Device::default();
        e.absent();
        assert!(!e.want(t));
        // The GPU turned off after lost devices stays off.
        let mut f = Device::default();
        let mut at = t;
        for _ in 0..LOST_CAP {
            assert!(f.want(at));
            f.up();
            f.lost(at);
            at += RETRY_AFTER + Duration::from_millis(1);
        }
        assert!(f.is_off());
        f.absent();
        assert!(f.is_off());
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

//! (M4; gpu-first, gpu-stay) When a surface moves to the GPU, and the
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
//! - It stays there for the rest of its life (decisions.md gpu-stay, the
//!   owner's "if something gets elevated to GPU you just make it stay
//!   there"): nothing here takes it back, and a promoted surface that
//!   paints nothing asks for nothing, no wake and no frame. Only a real
//!   failure does ([`Promotion::force_cpu`]: a frame that cannot be
//!   drawn, a lost device).
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
}

impl Promotion {
    /// True while promoted (or asked to be).
    pub fn on_gpu(&self) -> bool {
        self.gpu
    }

    /// A frame at `now` that damaged `damage` pixels, with `springs`
    /// in flight on the surface. True if this frame promotes the surface;
    /// one already promoted stays so, whatever its frames. A paint that
    /// damaged nothing is not a frame (nothing is committed): a clocked
    /// shader whose pass result comes every other paint does not break
    /// its run.
    pub fn frame(&mut self, now: Instant, damage: u64, springs: bool) -> bool {
        if damage == 0 {
            return false;
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
        if self.gpu {
            return false;
        }
        let animates = springs || (self.run >= CLOCK_RUN && !settling);
        self.gpu = large && animates;
        self.gpu
    }

    /// On the GPU at once (a test seam: `Renderer::promote_now`).
    pub fn force_gpu(&mut self) {
        self.gpu = true;
    }

    /// Back on the CPU at once, because the GPU failed it (a lost device,
    /// a failed start, a frame that cannot be drawn): no frame is waited
    /// for.
    pub fn force_cpu(&mut self) {
        self.gpu = false;
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

    /// Frames every 16 ms from `t` for `span`, `damage` each: how many
    /// promoted the surface.
    fn run(p: &mut Promotion, t: &mut Instant, span: Duration, damage: u64, springs: bool) -> u32 {
        let end = *t + span;
        let mut out = 0;
        while *t < end {
            *t += FRAME;
            out += u32::from(p.frame(*t, damage, springs));
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
        assert!(!p.frame(t, 2_000, false));
        // The mode switch: the card's size springs.
        assert!(
            p.frame(t + FRAME, 900_000, true),
            "the first large frame of the spring"
        );
        assert!(p.on_gpu());
        // A morph shorter than the old 500 ms window, settling on a
        // large frame: it stays on the GPU.
        let mut t = t + FRAME;
        assert_eq!(
            run(&mut p, &mut t, Duration::from_millis(300), 900_000, true),
            0
        );
        t += FRAME;
        assert!(!p.frame(t, 900_000, false), "the settling frame");
        assert!(p.on_gpu());
    }

    /// A panel that opens with a transition is promoted on its first
    /// frame (all of it damaged, its enter spring in flight).
    #[test]
    fn a_panel_opening_is_promoted_at_its_first_frame() {
        let mut p = Promotion::default();
        assert!(p.frame(Instant::now(), 1_350_000, true));
    }

    /// Large frames with nothing moving are new content, not an
    /// animation: a first frame, the text worker's layout after it, a
    /// list refilled. A clock's animation is promoted at its third large
    /// frame in a row; a pause past `RUN_GAP` starts the run over.
    #[test]
    fn a_clock_is_promoted_at_its_third_large_frame_in_a_row() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert!(!p.frame(t, 1_000_000, false), "a first frame");
        t += FRAME;
        assert!(!p.frame(t, 1_000_000, false), "its text laid out");
        // Seconds later, two more: a pause breaks the run.
        t += Duration::from_secs(2);
        assert!(!p.frame(t, 1_000_000, false));
        t += RUN_GAP + FRAME;
        assert!(!p.frame(t, 1_000_000, false));
        // A small frame breaks it too.
        t += FRAME;
        assert!(!p.frame(t, 1_000, false));
        // A clock: the third large frame in a row (a dropped frame or
        // two within RUN_GAP keep the run).
        t += FRAME;
        assert!(!p.frame(t, LARGE_DAMAGE, false));
        t += RUN_GAP;
        assert!(!p.frame(t, LARGE_DAMAGE, false));
        t += FRAME;
        assert!(p.frame(t, LARGE_DAMAGE, false));
        // Just under 0.2 Mpx is not large, however long it runs.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        let promoted = run(
            &mut q,
            &mut t,
            Duration::from_secs(2),
            LARGE_DAMAGE - 1,
            true,
        );
        assert_eq!(promoted, 0);
        assert!(!q.on_gpu());
    }

    #[test]
    fn a_paint_with_no_damage_is_not_a_frame() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        let mut promoted = false;
        for i in 0..8 {
            t += Duration::from_millis(8);
            let damage = if i % 2 == 0 { LARGE_DAMAGE } else { 0 };
            promoted |= p.frame(t, damage, false);
        }
        assert!(promoted);
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
        assert_eq!(
            run(&mut p, &mut t, Duration::from_millis(600), 1_000_000, true),
            1
        );
        p.force_cpu(); // no device: render takes it back at once
        assert_eq!(
            run(&mut p, &mut t, Duration::from_millis(200), 1_000, true),
            0
        );
        t += FRAME;
        assert!(!p.frame(t, 1_000_000, false), "a lone large frame");
        // Large frames all through the spring, the device refused each.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if q.frame(t, 1_000_000, true) {
                q.force_cpu();
            }
        }
        t += FRAME;
        assert!(!q.frame(t, 1_000_000, false), "the settling frame");
        assert!(!q.on_gpu());
        // Nothing follows: the next paint comes after a pause.
        t += Duration::from_secs(2);
        assert!(!q.frame(t, 1_000_000, false));
        // With a clock still running, the next large frame promotes.
        let mut c = Promotion::default();
        let mut t = Instant::now();
        for _ in 0..10 {
            t += FRAME;
            if c.frame(t, 1_000_000, true) {
                c.force_cpu();
            }
        }
        t += FRAME;
        assert!(!c.frame(t, 1_000_000, false), "the settling frame");
        t += FRAME;
        assert!(c.frame(t, 1_000_000, false));
    }

    /// (gpu-stay) Promoted once, a surface stays on the GPU whatever
    /// follows: a clock's small ticks, a small spring that never rests
    /// (an album cover turning with `t`), a day with no frame at all,
    /// and more large frames. It is promoted once, and nothing it does
    /// says otherwise: a swapchain destroyed for a hand-back is what
    /// froze surfaces on Hyprland.
    #[test]
    fn a_promoted_surface_stays_on_the_gpu() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert!(p.frame(t, 900_000, true));
        let tick = Duration::from_secs(1);
        for _ in 0..600 {
            t += tick;
            assert!(!p.frame(t, 1_200, false), "a clock's tick");
            assert!(p.on_gpu());
        }
        assert_eq!(run(&mut p, &mut t, tick * 60, 6_400, true), 0);
        assert!(p.on_gpu(), "a small spring that never rests");
        t += Duration::from_secs(86_400);
        assert!(p.on_gpu(), "a day of nothing");
        assert!(!p.frame(t, 1_200, false), "and the next tick");
        assert_eq!(run(&mut p, &mut t, tick, 900_000, true), 0);
        assert!(p.on_gpu(), "animating again is not a second promotion");
    }

    /// A failure takes a surface back at once, with no frame waited for,
    /// and a later animation may promote it again (a device up by then).
    #[test]
    fn a_failure_takes_a_surface_back_at_once() {
        let mut p = Promotion::default();
        let t = Instant::now();
        assert!(p.frame(t, 300_000, true));
        p.force_cpu();
        assert!(!p.on_gpu());
        assert!(p.frame(t + FRAME, 300_000, true));
        assert!(p.on_gpu());
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

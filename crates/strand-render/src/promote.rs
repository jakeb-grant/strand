//! (M4) When a surface moves to the GPU and back, and when the device
//! goes (architecture.md, "`strand-gpu`", Promotion). Pure state
//! machines: the renderer feeds them each frame's damage and whether a
//! spring is in flight, with the time, and carries out what they answer.
//!
//! - A surface is promoted after more than [`PROMOTE_AFTER`] in which
//!   every frame damaged at least [`LARGE_DAMAGE`] pixels, at the first
//!   frame after that with no spring in flight that is itself large (a
//!   clock-driven animation: shader time, particles). A run that ends in
//!   a small settled frame (a long spring that came to rest) has nothing
//!   left to speed up, and starts over; so does one with a pause of more
//!   than [`RUN_GAP`] between two frames, since large frames apart (a
//!   panel's opening, then its closing seconds later) are not an
//!   animation (m4-audit: the design launcher's close was promoted).
//!   The frame on which a surface's last spring comes to rest is not that
//!   first settled frame either: it still diffs large against the frame
//!   before (a spring's tail moves its node by less than the settle
//!   tolerance, and the whole node is damaged), but nothing follows it;
//!   the next large frame with no spring in flight switches (m4-audit: a
//!   spring longer than 500 ms was promoted on its last frame).
//! - Demotion is the rule reversed: more than [`PROMOTE_AFTER`] of small
//!   frames, then the next settled frame; a promoted surface that paints
//!   nothing for [`PROMOTE_AFTER`] is settled and goes back at once (its
//!   wake is [`Promotion::wake`]), so an idle surface never holds the
//!   device.
//! - The device ([`Device`]) is dropped [`DROP_AFTER`] after its last use
//!   once no surface is promoted; a failed start or a lost device is not
//!   asked for again for [`RETRY_AFTER`]. After [`LOST_CAP`] lost devices
//!   the GPU is off for the rest of the process (owner, m4-close-gpucap):
//!   no device is asked for again, and nothing is promoted.

use std::time::{Duration, Instant};

/// How long every frame must be large (or small) before a switch.
pub const PROMOTE_AFTER: Duration = Duration::from_millis(500);

/// The longest pause between two large frames that keeps a run toward
/// promotion going: many dropped frames at any refresh rate (a slow
/// debug frame included), half of [`PROMOTE_AFTER`].
pub const RUN_GAP: Duration = Duration::from_millis(250);

/// Damage that counts as a large frame: 0.2 Mpx.
pub const LARGE_DAMAGE: u64 = 200_000;

/// How long the device outlives its last use.
pub const DROP_AFTER: Duration = Duration::from_secs(30);

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
    /// When the current run of frames on the other side of the line
    /// began (large on the CPU, small on the GPU).
    run: Option<Instant>,
    /// The run lasted long enough: switch at the next settled frame.
    due: bool,
    /// When the surface last painted.
    last: Option<Instant>,
    /// The last frame had springs in flight.
    sprung: bool,
}

impl Promotion {
    /// True while promoted (or asked to be).
    pub fn on_gpu(&self) -> bool {
        self.gpu
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
        // The frame where the springs came to rest: a large animation
        // driven by springs alone ends here, with nothing left to speed
        // up, though its damage still counts as large.
        let settling = std::mem::replace(&mut self.sprung, springs) && !springs;
        // A pause on the CPU ends the run: large frames that are not one
        // animation do not add up. (On the GPU a pause is `idle`'s.)
        if !self.gpu
            && self
                .last
                .is_some_and(|t| now.saturating_duration_since(t) > RUN_GAP)
        {
            self.run = None;
            self.due = false;
        }
        self.last = Some(now);
        let large = damage >= LARGE_DAMAGE;
        // The side of the line that argues for a switch.
        let other = large != self.gpu;
        if other {
            let start = *self.run.get_or_insert(now);
            if now.saturating_duration_since(start) > PROMOTE_AFTER {
                self.due = true;
            }
        } else if !self.due || !springs {
            // Back on its own side: the run ends, and a switch that was
            // due ends with a settled frame here (a long spring that came
            // to rest has nothing left to speed up). With springs still
            // in flight a due switch keeps waiting.
            self.run = None;
            self.due = false;
            return None;
        }
        // Demotion may happen as the springs settle; promotion waits for
        // a frame that proves something still animates.
        if self.due && !springs && other && !(settling && !self.gpu) {
            return Some(self.switch());
        }
        None
    }

    fn switch(&mut self) -> Switch {
        self.gpu = !self.gpu;
        self.run = None;
        self.due = false;
        if self.gpu {
            Switch::ToGpu
        } else {
            Switch::ToCpu
        }
    }

    /// When a promoted surface that paints nothing more is settled long
    /// enough to go back ([`Promotion::idle`] then demotes it).
    pub fn wake(&self) -> Option<Instant> {
        if !self.gpu {
            return None;
        }
        self.last.map(|t| t + PROMOTE_AFTER)
    }

    /// The loop woke at `now` with no frame painted since
    /// [`Promotion::wake`]: a promoted surface goes back.
    pub fn idle(&mut self, now: Instant) -> Option<Switch> {
        match self.wake() {
            Some(t) if now >= t => {
                self.gpu = false;
                self.run = None;
                self.due = false;
                Some(Switch::ToCpu)
            }
            _ => None,
        }
    }

    /// A frame of the promoted surface reached the screen at `now` (a
    /// presented frame's `Presented`): its idle window starts there, not
    /// when it was painted.
    pub fn shown(&mut self, now: Instant) {
        if self.gpu {
            self.last = Some(self.last.map_or(now, |t| t.max(now)));
        }
    }

    /// On the GPU at once (a test seam: `Renderer::promote_now`).
    pub fn force_gpu(&mut self) {
        self.gpu = true;
        self.run = None;
        self.due = false;
        self.last = None;
    }

    /// Back on the CPU at once (a lost device, a failed start): no
    /// settled frame is waited for.
    pub fn force_cpu(&mut self) {
        self.gpu = false;
        self.run = None;
        self.due = false;
    }
}

/// The device's lifecycle as render sees it.
#[derive(Clone, Debug, Default)]
pub struct Device {
    state: State,
    /// The last frame, readback or pass the GPU drew (or was asked for).
    last_use: Option<Instant>,
    /// [`DROP_AFTER`] unless a test shortens it.
    idle: Option<Duration>,
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
    /// Something needs the device at `now`. True if it may be used (it is
    /// up, or starting, or this asks for it); false while a failure is
    /// less than [`RETRY_AFTER`] old.
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
                self.last_use = Some(now);
                true
            }
            State::Starting | State::Up => {
                self.last_use = Some(now);
                true
            }
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
            self.last_use = None;
            return true;
        }
        self.failed(now);
        false
    }

    /// True once the GPU is off for the rest of the process.
    pub fn is_off(&self) -> bool {
        self.state == State::Off
    }

    /// The `Gpu` was dropped.
    pub fn dropped(&mut self) {
        if matches!(self.state, State::Starting | State::Up) {
            self.state = State::Unused;
        }
        self.last_use = None;
    }

    /// When the device is to be dropped, with nothing promoted.
    pub fn drop_at(&self) -> Option<Instant> {
        match self.state {
            State::Starting | State::Up => {
                self.last_use.map(|t| t + self.idle.unwrap_or(DROP_AFTER))
            }
            _ => None,
        }
    }

    /// The device is due to go at `now` (nothing promoted: the caller
    /// checks).
    pub fn due(&self, now: Instant) -> bool {
        self.drop_at().is_some_and(|t| now >= t)
    }

    /// How long the device outlives its last use ([`DROP_AFTER`]; tests
    /// shorten it).
    pub fn set_idle(&mut self, idle: Duration) {
        self.idle = Some(idle);
    }

    /// Shortens [`RETRY_AFTER`] (tests).
    pub fn set_retry(&mut self, retry: Duration) {
        self.retry = Some(retry);
    }

    /// A GPU frame, readback or pass at `now`.
    pub fn used(&mut self, now: Instant) {
        self.last_use = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (m4-integration-w2) A presented frame reaching the screen late
    /// moves the idle window to its `Presented`; on the CPU it changes
    /// nothing.
    #[test]
    fn a_shown_frame_restarts_the_idle_window() {
        let t = Instant::now();
        let mut p = Promotion::default();
        p.shown(t);
        assert_eq!(p.wake(), None, "on the CPU");
        p.force_gpu();
        assert_eq!(p.frame(t, LARGE_DAMAGE, false), None);
        assert_eq!(p.wake(), Some(t + PROMOTE_AFTER));
        let late = t + Duration::from_millis(800);
        p.shown(late);
        assert_eq!(p.wake(), Some(late + PROMOTE_AFTER));
        assert_eq!(p.idle(t + PROMOTE_AFTER), None, "not idle at the old wake");
        p.shown(t);
        assert_eq!(p.wake(), Some(late + PROMOTE_AFTER), "never earlier");
        assert_eq!(p.idle(late + PROMOTE_AFTER), Some(Switch::ToCpu));
    }

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

    /// (m4-audit) Large frames with pauses between them are not one
    /// animation: the design launcher's opening (a dozen large frames),
    /// seconds open with nothing painted, then its closing (a dozen more)
    /// had added up to "more than 500 ms of large frames" and started the
    /// GPU in a shell with no GPU effect. A pause past `RUN_GAP` starts
    /// the run over; one within it does not.
    #[test]
    fn large_frames_apart_are_not_one_run() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert!(run(&mut p, &mut t, Duration::from_millis(200), 1_000_000, false).is_empty());
        t += Duration::from_secs(3);
        assert!(run(&mut p, &mut t, Duration::from_millis(200), 1_000_000, false).is_empty());
        t += RUN_GAP + FRAME;
        assert!(run(&mut p, &mut t, Duration::from_millis(480), 1_000_000, false).is_empty());
        assert!(!p.on_gpu());
        // A pause within RUN_GAP (dropped frames) keeps the run.
        t += RUN_GAP - FRAME;
        let got = run(&mut p, &mut t, Duration::from_millis(60), 1_000_000, false);
        assert_eq!(got, [Switch::ToGpu]);
    }

    #[test]
    fn promotes_after_500ms_of_large_damage() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        // 480 ms of large frames: not yet.
        assert!(run(&mut p, &mut t, Duration::from_millis(480), 300_000, false).is_empty());
        assert!(!p.on_gpu());
        // One small frame breaks the run.
        assert_eq!(p.frame(t + FRAME, 1000, false), None);
        t += FRAME;
        assert!(run(&mut p, &mut t, Duration::from_millis(480), 300_000, false).is_empty());
        // Past 500 ms: the next (settled, large) frame promotes.
        let got = run(&mut p, &mut t, Duration::from_millis(60), 300_000, false);
        assert_eq!(got, [Switch::ToGpu]);
        assert!(p.on_gpu());
        // 0.2 Mpx exactly is large.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        assert_eq!(
            run(
                &mut q,
                &mut t,
                Duration::from_millis(560),
                LARGE_DAMAGE,
                false
            ),
            [Switch::ToGpu]
        );
    }

    #[test]
    fn a_paint_with_no_damage_is_not_a_frame() {
        let t0 = Instant::now();
        let mut p = Promotion::default();
        let mut t = t0;
        let mut switched = None;
        for i in 0..80 {
            t += Duration::from_millis(8);
            let damage = if i % 2 == 0 { LARGE_DAMAGE } else { 0 };
            switched = switched.or(p.frame(t, damage, false));
        }
        assert_eq!(switched, Some(Switch::ToGpu));
    }

    #[test]
    fn switches_only_when_settled() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        // A large animation with springs in flight: due, but never
        // settled while it runs.
        assert!(run(&mut p, &mut t, Duration::from_secs(1), 300_000, true).is_empty());
        // It comes to rest with a small frame: nothing left to speed up.
        assert_eq!(p.frame(t + FRAME, 2000, false), None);
        assert!(!p.on_gpu());
        // A clock-driven one (no springs) promotes; then its springs
        // keep it there until they settle on a small frame.
        t += FRAME;
        assert_eq!(
            run(&mut p, &mut t, Duration::from_millis(600), 300_000, false),
            [Switch::ToGpu]
        );
        assert!(run(&mut p, &mut t, Duration::from_secs(1), 1000, true).is_empty());
        assert!(p.on_gpu(), "springs in flight: no switch");
        assert_eq!(p.frame(t + FRAME, 1000, false), Some(Switch::ToCpu));
        assert!(!p.on_gpu());
    }

    /// (m4-audit) A spring-driven large animation longer than 500 ms
    /// ends on a frame that is still large (its node moved by less than
    /// the settle tolerance, and the whole node is damaged) and has no
    /// spring in flight: that frame does not promote, since nothing
    /// follows it. A clock-driven animation that goes on after its
    /// springs settle promotes on its next large frame.
    #[test]
    fn a_spring_that_settles_on_a_large_frame_is_not_promoted() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        assert!(run(&mut p, &mut t, Duration::from_millis(600), 1_000_000, true).is_empty());
        t += FRAME;
        assert_eq!(p.frame(t, 1_000_000, false), None, "the settling frame");
        assert!(!p.on_gpu());
        // Nothing follows: the next paint comes after a pause, and the run
        // starts over.
        t += Duration::from_secs(2);
        assert!(run(&mut p, &mut t, Duration::from_millis(200), 1_000_000, false).is_empty());
        assert!(!p.on_gpu());

        // Springs and a shader clock together: the springs settle on a
        // large frame, and the clock's next frame promotes.
        let mut q = Promotion::default();
        let mut t = Instant::now();
        assert!(run(&mut q, &mut t, Duration::from_millis(600), 1_000_000, true).is_empty());
        t += FRAME;
        assert_eq!(q.frame(t, 1_000_000, false), None);
        t += FRAME;
        assert_eq!(q.frame(t, 1_000_000, false), Some(Switch::ToGpu));
    }

    #[test]
    fn demotes_after_500ms_of_small_damage() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        run(&mut p, &mut t, Duration::from_millis(600), 300_000, false);
        assert!(p.on_gpu());
        assert!(run(&mut p, &mut t, Duration::from_millis(480), 100, false).is_empty());
        assert_eq!(
            run(&mut p, &mut t, Duration::from_millis(60), 100, false),
            [Switch::ToCpu]
        );
    }

    #[test]
    fn an_idle_promoted_surface_goes_back_at_its_wake() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        run(&mut p, &mut t, Duration::from_millis(600), 300_000, false);
        let wake = p.wake().expect("a promoted surface has a wake");
        assert_eq!(wake, t + PROMOTE_AFTER);
        assert_eq!(p.idle(wake - FRAME), None);
        assert_eq!(p.idle(wake), Some(Switch::ToCpu));
        assert_eq!(p.wake(), None);
    }

    #[test]
    fn drops_after_30s_with_one_wake() {
        let mut d = Device::default();
        let t0 = Instant::now();
        assert_eq!(d.drop_at(), None, "nothing to drop");
        assert!(d.want(t0));
        assert!(d.starting());
        d.up();
        assert!(d.is_up());
        d.used(t0 + Duration::from_secs(5));
        // One wake: the drop instant, 30 s after the last use.
        assert_eq!(d.drop_at(), Some(t0 + Duration::from_secs(35)));
        assert!(!d.due(t0 + Duration::from_secs(34)));
        assert!(d.due(t0 + Duration::from_secs(35)));
        d.dropped();
        assert_eq!(d.drop_at(), None);
        assert!(d.idle());
    }

    #[test]
    fn a_failure_is_retried_at_most_once_per_30s() {
        let mut d = Device::default();
        let t0 = Instant::now();
        assert!(d.want(t0));
        d.failed(t0 + Duration::from_millis(100));
        assert_eq!(d.drop_at(), None, "a failed device has nothing to drop");
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
        assert_eq!(d.drop_at(), None);
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

    #[test]
    fn a_forced_demotion_waits_for_nothing() {
        let mut p = Promotion::default();
        let mut t = Instant::now();
        run(&mut p, &mut t, Duration::from_millis(600), 300_000, false);
        assert!(p.on_gpu());
        p.force_cpu();
        assert!(!p.on_gpu());
        assert_eq!(p.wake(), None);
    }
}

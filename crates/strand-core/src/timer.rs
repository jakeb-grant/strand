//! Timers as vocabulary: `after T while cond { }`, `every T while cond { }`
//! and `on change x after T { }`.
//!
//! A timer counts time only while its condition is true; when the condition
//! turns false it pauses and keeps the time already counted. The condition
//! and duration are tracked like an effect, so a timer whose condition is
//! false schedules nothing: [`Runtime::next_deadline`] stays `None` and the
//! host sleeps. Bodies run as handlers when [`Runtime::advance_to`] passes
//! the deadline.
//!
//! `every T` keeps its phase: host wake-up latency does not accumulate
//! (`every 1s` fires at 1, 2, 3 … even when each tick arrives 16 ms late);
//! after a gap longer than a period it fires once and re-phases (missed
//! periods are not replayed). A zero period pauses an `every` timer and
//! reports [`Diagnostic::ZeroPeriod`]; periods under [`MIN_EVERY_PERIOD`]
//! are clamped to it. Deadlines past the end of time mean "never".
//!
//! A timer inside a suspended (frozen) component is paused like a false
//! condition: it keeps the time counted so far and counts again once the
//! component is released ([`Runtime::resume`]), so a toast frozen with a
//! fault does not expire behind the user's back. The logic clock only
//! moves when the host ticks, and a host can sleep while everything is
//! frozen, so a released timer starts counting at the next clock advance
//! (the host's real time), not at the last tick: until then it reports no
//! deadline, and the release calls the wake hook so the host ticks.
//! `await sleep(..)` in a frozen handler is paused the same way.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{
    Color, Diagnostic, HandlerCtx, NodeData, NodeId, NodeKind, RunOutcome, Runtime,
};
use crate::signal::Effect;

/// The shortest `every` period; shorter positive periods are clamped.
pub const MIN_EVERY_PERIOD: Duration = Duration::from_millis(1);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    After,
    Every,
    Debounce,
}

#[derive(Copy, Clone, Debug, Default)]
struct State {
    duration: Duration,
    /// Time counted before the current run segment.
    elapsed: Duration,
    /// Start of the current run segment while the condition holds.
    since: Option<Duration>,
    armed: bool,
    cond: bool,
    /// A zero `every` period was reported (cleared when it turns positive).
    zero: bool,
    /// Released from a frozen scope: starts counting at the next clock
    /// advance (the logic clock may be far behind the host's).
    resume_pending: bool,
}

type DurationFn = Box<dyn Fn(&Runtime) -> Result<Duration, Error>>;
type CondFn = Box<dyn Fn(&Runtime) -> Result<bool, Error>>;
type BodyFn = Box<dyn FnMut(&Runtime) -> Result<(), Error>>;

struct TimerData {
    mode: Mode,
    duration: DurationFn,
    cond: CondFn,
    body: RefCell<BodyFn>,
    st: Cell<State>,
}

impl TimerData {
    fn deadline(&self) -> Option<Duration> {
        let s = self.st.get();
        if !s.armed || self.mode == Mode::Every && s.duration.is_zero() {
            return None;
        }
        s.since
            .and_then(|since| since.checked_add(s.duration.saturating_sub(s.elapsed)))
    }

    fn counted(&self, now: Duration) -> Duration {
        let s = self.st.get();
        s.elapsed
            .saturating_add(s.since.map_or(Duration::ZERO, |t| now.saturating_sub(t)))
    }
}

impl NodeData for TimerData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    /// Re-evaluate condition and duration (tracked) and pause or resume.
    fn run(&self, rt: &Runtime, id: NodeId) -> RunOutcome {
        let mut duration = match (self.duration)(rt) {
            Ok(d) => d,
            Err(e) => return RunOutcome::Failed(e),
        };
        let mut zero = false;
        if self.mode == Mode::Every {
            if duration.is_zero() {
                zero = true;
                if !self.st.get().zero {
                    rt.diagnose(Diagnostic::ZeroPeriod { timer: id });
                }
            } else {
                duration = duration.max(MIN_EVERY_PERIOD);
            }
        }
        let cond = match (self.cond)(rt) {
            Ok(c) => c,
            Err(e) => return RunOutcome::Failed(e),
        };
        let now = rt.now();
        let mut s = self.st.get();
        s.duration = duration;
        s.cond = cond;
        s.zero = zero;
        // A zero `every` period counts nothing, like a false condition, and
        // so does a frozen component (frozen = paused).
        match (cond && s.armed && !zero && !rt.is_suspended(id), s.since) {
            // A resume found while catching up before a clock advance
            // happened somewhere in (previous now, new now]: count from the
            // new now, so the timer never counts time the condition may not
            // have held.
            (true, None) => match rt.inner.resume_at.get() {
                Some(at) => {
                    s.since = Some(at);
                    s.resume_pending = false;
                }
                // Released while the host slept: wait for its clock.
                None if s.resume_pending => {}
                None => s.since = Some(now),
            },
            (false, Some(t)) => {
                s.elapsed = s.elapsed.saturating_add(now.saturating_sub(t));
                s.since = None;
            }
            _ => {}
        }
        self.st.set(s);
        RunOutcome::Unchanged
    }
}

/// A running `after`/`every`/debounce timer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Timer {
    id: NodeId,
}

/// `on change x after T`: the change effect and the debounce timer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Debounced {
    /// Restarts the timer on every change.
    pub effect: Effect,
    /// Fires the handler after `T` of quiet.
    pub timer: Timer,
}

impl Debounced {
    /// Live reload of `on change x after T`: take over `old`'s debounce, so
    /// a countdown in flight when the handler was restarted still fires
    /// (once, at its rescaled time) and an idle one stays idle. See
    /// [`Timer::rescale_from`].
    pub fn rescale_from(self, rt: &Runtime, old: Debounced) -> Result<(), Error> {
        self.timer.rescale_from(rt, old.timer)
    }

    /// Stop both.
    pub fn dispose(self, rt: &Runtime) {
        self.effect.dispose(rt);
        self.timer.dispose(rt);
    }
}

impl Timer {
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }

    /// Stop the timer.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }

    fn with<R>(self, rt: &Runtime, f: impl FnOnce(&TimerData) -> R) -> Result<R, Error> {
        rt.with_data::<TimerData, _>(self.id, f)
    }

    /// When the timer fires next, if it is counting.
    pub fn deadline(self, rt: &Runtime) -> Result<Option<Duration>, Error> {
        self.with(rt, TimerData::deadline)
    }

    /// True while the condition holds and the timer is counting.
    pub fn is_running(self, rt: &Runtime) -> Result<bool, Error> {
        self.with(rt, |t| t.st.get().since.is_some())
    }

    /// Fraction of the period already counted, in `0..=1`.
    pub fn progress(self, rt: &Runtime) -> Result<f64, Error> {
        let now = rt.now();
        self.with(rt, |t| {
            let d = t.st.get().duration;
            if d.is_zero() {
                1.0
            } else {
                (t.counted(now).as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0)
            }
        })
    }

    /// Live reload of a timer duration ("remaining time rescaled"): this
    /// (new) timer takes over `old`'s countdown. Its lifecycle carries over,
    /// not just its fraction: an `after` that already fired stays done (a
    /// reload never repeats its side effect), an idle debounce stays idle,
    /// and a countdown in flight (an armed debounce included) keeps the
    /// fraction of its period already counted and finishes the rest at the
    /// new duration.
    pub fn rescale_from(self, rt: &Runtime, old: Timer) -> Result<(), Error> {
        let now = rt.now();
        let (armed, fraction) = old.with(rt, |t| {
            let s = t.st.get();
            let fraction = if s.duration.is_zero() {
                0.0
            } else {
                (t.counted(now).as_secs_f64() / s.duration.as_secs_f64()).clamp(0.0, 1.0)
            };
            (s.armed, fraction)
        })?;
        let frozen = rt.is_suspended(self.id);
        self.with(rt, |t| {
            let mut s = t.st.get();
            s.armed = armed;
            s.elapsed = if armed {
                Duration::try_from_secs_f64(s.duration.as_secs_f64() * fraction)
                    .unwrap_or(s.duration)
                    .min(s.duration)
            } else {
                Duration::ZERO
            };
            s.since = (armed && s.cond && !s.zero && !frozen).then_some(now);
            s.resume_pending &= frozen;
            t.st.set(s);
        })
    }

    /// Restart counting from zero (debounce).
    pub fn restart(self, rt: &Runtime) -> Result<(), Error> {
        let now = rt.now();
        let frozen = rt.is_suspended(self.id);
        self.with(rt, |t| {
            let mut s = t.st.get();
            s.armed = true;
            s.elapsed = Duration::ZERO;
            s.since = (s.cond && !frozen).then_some(now);
            s.resume_pending &= frozen;
            t.st.set(s);
        })
    }
}

impl Runtime {
    fn timer(&self, mode: Mode, duration: DurationFn, cond: CondFn, body: BodyFn) -> Timer {
        let id = self.create_node(
            NodeKind::Timer,
            Color::Dirty,
            Some(Rc::new(TimerData {
                mode,
                duration,
                cond,
                body: RefCell::new(body),
                st: Cell::new(State {
                    armed: mode != Mode::Debounce,
                    ..State::default()
                }),
            })),
        );
        self.inner.timers.borrow_mut().push(id);
        // Tasks its body starts are cancelled with the timer, not when its
        // condition changes.
        self.create_site_for(id);
        // Evaluate the condition now so the deadline is known immediately.
        let _ = self.update_if_necessary(id);
        Timer { id }
    }

    /// `after T while cond { body }`: run `body` once after `cond` has held
    /// for a total of `T`.
    pub fn after<C, B>(&self, duration: Duration, cond: C, body: B) -> Timer
    where
        C: Fn(&Runtime) -> Result<bool, Error> + 'static,
        B: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        self.after_dyn(move |_| Ok(duration), cond, body)
    }

    /// `after` with a reactive duration (`after n.timeout ?? 6s while …`).
    pub fn after_dyn<D, C, B>(&self, duration: D, cond: C, body: B) -> Timer
    where
        D: Fn(&Runtime) -> Result<Duration, Error> + 'static,
        C: Fn(&Runtime) -> Result<bool, Error> + 'static,
        B: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        self.timer(
            Mode::After,
            Box::new(duration),
            Box::new(cond),
            Box::new(body),
        )
    }

    /// `every T while cond { body }`: run `body` every `T` of time during
    /// which `cond` holds. Missed periods are not replayed.
    pub fn every<C, B>(&self, duration: Duration, cond: C, body: B) -> Timer
    where
        C: Fn(&Runtime) -> Result<bool, Error> + 'static,
        B: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        self.every_dyn(move |_| Ok(duration), cond, body)
    }

    /// `every` with a reactive duration.
    pub fn every_dyn<D, C, B>(&self, duration: D, cond: C, body: B) -> Timer
    where
        D: Fn(&Runtime) -> Result<Duration, Error> + 'static,
        C: Fn(&Runtime) -> Result<bool, Error> + 'static,
        B: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        self.timer(
            Mode::Every,
            Box::new(duration),
            Box::new(cond),
            Box::new(body),
        )
    }

    /// `on change x after T { … }`: run `handler` once `track` has stopped
    /// changing for `T`. Never fires for the first value.
    pub fn on_change_after<T, F, H>(&self, track: F, delay: Duration, handler: H) -> Debounced
    where
        T: PartialEq + 'static,
        F: Fn(&Runtime) -> Result<T, Error> + 'static,
        H: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        self.on_change_after_keyed(|_| Ok(()), track, delay, handler)
    }

    /// [`Runtime::on_change_after`] that re-baselines without restarting
    /// the countdown when `key` changes (see [`Runtime::on_change_keyed`]).
    pub fn on_change_after_keyed<K, KF, T, F, H>(
        &self,
        key: KF,
        track: F,
        delay: Duration,
        handler: H,
    ) -> Debounced
    where
        K: PartialEq + 'static,
        KF: Fn(&Runtime) -> Result<K, Error> + 'static,
        T: PartialEq + 'static,
        F: Fn(&Runtime) -> Result<T, Error> + 'static,
        H: FnMut(&Runtime) -> Result<(), Error> + 'static,
    {
        let timer = self.timer(
            Mode::Debounce,
            Box::new(move |_| Ok(delay)),
            Box::new(|_| Ok(true)),
            Box::new(handler),
        );
        let effect = self.on_change_keyed(key, track, move |rt, _| timer.restart(rt));
        Debounced { effect, timer }
    }

    pub(crate) fn timer_deadline(&self) -> Option<Duration> {
        let timers = self.inner.timers.borrow().clone();
        timers
            .into_iter()
            .filter(|&id| !self.is_suspended(id))
            .filter_map(|id| {
                self.with_data::<TimerData, _>(id, TimerData::deadline)
                    .ok()
                    .flatten()
            })
            .min()
    }

    /// Pause timers (and sleeping handlers) inside suspended scopes and
    /// mark the ones released: frozen = paused, like a `while` condition
    /// that turned false (time counted so far is kept). A released timer
    /// counts again from the next clock advance ([`Runtime::advance_to`]
    /// with the host's time), since the logic clock stands still while the
    /// host sleeps. Returns whether something is waiting for that advance
    /// (the caller wakes the host).
    pub(crate) fn sync_frozen_timers(&self) -> bool {
        let now = self.now();
        self.inner.timers.borrow_mut().retain(|&t| self.exists(t));
        let timers = self.inner.timers.borrow().clone();
        let mut resumed = false;
        for id in timers {
            let frozen = self.is_suspended(id);
            let _ = self.with_data::<TimerData, _>(id, |t| {
                let mut s = t.st.get();
                match (frozen, s.since) {
                    (true, Some(since)) => {
                        s.elapsed = s.elapsed.saturating_add(now.saturating_sub(since));
                        s.since = None;
                    }
                    (false, None) if s.cond && s.armed && !s.zero => {
                        s.resume_pending = true;
                        resumed = true;
                    }
                    _ => {}
                }
                t.st.set(s);
            });
        }
        resumed |= self.sync_frozen_sleepers();
        resumed
    }

    /// Start released timers and sleepers counting at `at` (the clock
    /// advance after their release).
    fn start_released(&self, at: Duration) {
        let timers = self.inner.timers.borrow().clone();
        for id in timers {
            let _ = self.with_data::<TimerData, _>(id, |t| {
                let mut s = t.st.get();
                if s.resume_pending && !self.is_suspended(id) {
                    s.resume_pending = false;
                    if s.since.is_none() && s.cond && s.armed && !s.zero {
                        s.since = Some(at);
                    }
                    t.st.set(s);
                }
            });
        }
        self.start_released_sleepers(at);
    }

    /// Bring timers whose condition or duration inputs changed up to date
    /// before the clock moves to `upcoming`: a pause counts up to the
    /// current (previous) time, a resume counts from `upcoming`. Either way
    /// no time is counted that the condition may not have held for.
    pub(crate) fn refresh_timers(&self, upcoming: Duration) {
        self.inner.timers.borrow_mut().retain(|&t| self.exists(t));
        let timers = self.inner.timers.borrow().clone();
        let at = upcoming.max(self.now());
        self.inner.resume_at.set(Some(at));
        for id in timers {
            if self.is_stale(id) {
                let _ = self.update_if_necessary(id);
            }
        }
        self.inner.resume_at.set(None);
        self.start_released(at);
    }

    /// Run every timer whose deadline has passed, earliest first.
    pub(crate) fn fire_timers(&self, errors: &mut Vec<(NodeId, Error)>) {
        let now = self.now();
        self.inner.timers.borrow_mut().retain(|&t| self.exists(t));
        let mut due: Vec<(Duration, u64, NodeId)> = self
            .inner
            .timers
            .borrow()
            .iter()
            .filter_map(|&id| {
                let d = self
                    .with_data::<TimerData, _>(id, TimerData::deadline)
                    .ok()
                    .flatten()?;
                let seq = self.inner.nodes.borrow().get(id)?.seq;
                // (A frozen timer is paused, so it has no deadline.)
                (d <= now && !self.is_suspended(id)).then_some((d, seq, id))
            })
            .collect();
        // Earliest first; equal deadlines in creation order, not slot order.
        due.sort_by_key(|&(d, seq, _)| (d, seq));
        for (_, _, id) in due {
            // An earlier body may have written this timer's condition.
            if self.is_stale(id) {
                let _ = self.update_if_necessary(id);
            }
            let Ok(data) = self.data(id) else { continue };
            let Some(t) = data.as_any().downcast_ref::<TimerData>() else {
                continue;
            };
            let Some(deadline) = t.deadline().filter(|&d| d <= now) else {
                continue;
            };
            let mut s = t.st.get();
            match t.mode {
                Mode::After | Mode::Debounce => {
                    s.armed = false;
                    s.since = None;
                    s.elapsed = Duration::ZERO;
                }
                Mode::Every => {
                    s.elapsed = Duration::ZERO;
                    // Keep the phase unless a whole period was missed.
                    let late = now.saturating_sub(deadline);
                    s.since = Some(if late < s.duration { deadline } else { now });
                }
            }
            t.st.set(s);
            let Ok(mut body) = t.body.try_borrow_mut() else {
                continue;
            };
            // Nodes the body creates belong to the timer's component.
            let ctx = HandlerCtx {
                writer: id,
                owner: self.owner_of(id).ok().flatten(),
                site: self.site_of(id),
                input: false,
            };
            let r = self.run_handler(ctx, |rt| body(rt));
            if let Err(e) = r {
                errors.push((id, e));
            }
        }
    }
}

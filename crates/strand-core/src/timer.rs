//! Timers as vocabulary: `after T while cond { }`, `every T while cond { }`
//! and `on change x after T { }`.
//!
//! A timer counts time only while its condition is true; when the condition
//! turns false it pauses and keeps the time already counted. The condition
//! and duration are tracked like an effect, so a timer whose condition is
//! false schedules nothing: [`Runtime::next_deadline`] stays `None` and the
//! host sleeps. Bodies run as handlers when [`Runtime::advance_to`] passes
//! the deadline.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{Color, NodeData, NodeId, NodeKind, RunOutcome, Runtime};
use crate::signal::Effect;

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
        if !s.armed {
            return None;
        }
        s.since
            .map(|since| since + s.duration.saturating_sub(s.elapsed))
    }

    fn counted(&self, now: Duration) -> Duration {
        let s = self.st.get();
        s.elapsed + s.since.map_or(Duration::ZERO, |t| now.saturating_sub(t))
    }
}

impl NodeData for TimerData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    /// Re-evaluate condition and duration (tracked) and pause or resume.
    fn run(&self, rt: &Runtime, _id: NodeId) -> RunOutcome {
        let duration = match (self.duration)(rt) {
            Ok(d) => d,
            Err(e) => return RunOutcome::Failed(e),
        };
        let cond = match (self.cond)(rt) {
            Ok(c) => c,
            Err(e) => return RunOutcome::Failed(e),
        };
        let now = rt.now();
        let mut s = self.st.get();
        s.duration = duration;
        s.cond = cond;
        match (cond && s.armed, s.since) {
            (true, None) => s.since = Some(now),
            (false, Some(t)) => {
                s.elapsed += now.saturating_sub(t);
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

    /// Live reload of a timer duration: the new timer keeps the fraction of
    /// its period that `old` had already counted ("remaining time
    /// rescaled").
    pub fn rescale_from(self, rt: &Runtime, old: Timer) -> Result<(), Error> {
        let fraction = old.progress(rt)?;
        let now = rt.now();
        self.with(rt, |t| {
            let mut s = t.st.get();
            s.elapsed = s.duration.mul_f64(fraction);
            if s.since.is_some() {
                s.since = Some(now);
            }
            t.st.set(s);
        })
    }

    /// Restart counting from zero (debounce).
    pub fn restart(self, rt: &Runtime) -> Result<(), Error> {
        let now = rt.now();
        self.with(rt, |t| {
            let mut s = t.st.get();
            s.armed = true;
            s.elapsed = Duration::ZERO;
            s.since = s.cond.then_some(now);
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
        let timer = self.timer(
            Mode::Debounce,
            Box::new(move |_| Ok(delay)),
            Box::new(|_| Ok(true)),
            Box::new(handler),
        );
        let effect = self.on_change(track, move |rt, _| timer.restart(rt));
        Debounced { effect, timer }
    }

    pub(crate) fn timer_deadline(&self) -> Option<Duration> {
        let timers = self.inner.timers.borrow().clone();
        timers
            .into_iter()
            .filter_map(|id| {
                self.with_data::<TimerData, _>(id, TimerData::deadline)
                    .ok()
                    .flatten()
            })
            .min()
    }

    /// Run every timer whose deadline has passed, earliest first.
    pub(crate) fn fire_timers(&self, errors: &mut Vec<(NodeId, Error)>) {
        let now = self.now();
        self.inner.timers.borrow_mut().retain(|&t| self.exists(t));
        let mut due: Vec<(Duration, NodeId)> = self
            .inner
            .timers
            .borrow()
            .iter()
            .filter_map(|&id| {
                let d = self
                    .with_data::<TimerData, _>(id, TimerData::deadline)
                    .ok()
                    .flatten()?;
                (d <= now).then_some((d, id))
            })
            .collect();
        due.sort();
        for (_, id) in due {
            let Ok(data) = self.data(id) else { continue };
            let Some(t) = data.as_any().downcast_ref::<TimerData>() else {
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
                    s.since = Some(now);
                }
            }
            t.st.set(s);
            let Ok(mut body) = t.body.try_borrow_mut() else {
                continue;
            };
            let prev_writer = self.inner.writer.replace(Some(id));
            let prev_owner = self.inner.owner.replace(Some(id));
            let r = self.untrack(|rt| body(rt));
            self.inner.owner.set(prev_owner);
            self.inner.writer.set(prev_writer);
            if let Err(e) = r {
                errors.push((id, e));
            }
        }
    }
}

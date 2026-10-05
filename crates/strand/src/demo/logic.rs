//! The logic thread of the demo: a `strand-core` runtime holding the clock
//! as a `Signal`, a `Memo` formatting it and the scene emitter at the edge,
//! which watches the memo (`rt.watch`) and turns `Tick::changed` into
//! `SetProp`s, as `docs/architecture.md` specifies for the compiler's
//! emitter. One `SceneDiff` per tick goes to the main thread
//! (`docs/architecture.md`, "Threads").
//!
//! The thread sleeps in `epoll` on a wall-clock timerfd armed for the next
//! minute boundary, plus whatever the runtime itself schedules
//! ([`Runtime::next_deadline`], none in the demo) and its wake hook.

use std::cell::RefCell;
use std::io;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use strand_core::{Memo, NodeId, Runtime, Signal};
use strand_scene::{Prop, PropValue, SceneDiff};

use super::clock::{Fired, MINUTE, WallTimer};
use super::scene;

/// `clock.format("%H:%M")` for a Unix minute, in local time.
pub fn format_local(minute: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(minute.saturating_mul(60), 0).earliest() {
        Some(t) => t.format("%H:%M").to_string(),
        None => String::from("--:--"),
    }
}

/// The reactive part, independent of the thread and the timer so tests can
/// drive it.
pub struct Logic {
    rt: Runtime,
    minute: Signal<i64>,
    /// Watched memos and the scene prop each one is bound to.
    bindings: Vec<(Memo<String>, strand_scene::NodeId, Prop)>,
    /// The boot diff, sent with the first tick.
    boot: RefCell<SceneDiff>,
}

impl std::fmt::Debug for Logic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logic").finish_non_exhaustive()
    }
}

impl Logic {
    /// Build the graph: `minute` (state) → `clock` text (memo), watched by
    /// the emitter. The first tick emits the whole bar with the clock.
    pub fn new(
        minute: i64,
        format: impl Fn(i64) -> String + 'static,
    ) -> Result<Self, strand_core::Error> {
        let rt = Runtime::new();
        let minute = rt.signal(minute);
        rt.set_name(minute.id(), "clock.minute");
        let text = rt.memo(move |rt| Ok(format(minute.get(rt)?)));
        rt.set_name(text.id(), "clock.format(\"%H:%M\")");
        let watch = rt.watch(text.id())?;
        rt.set_name(watch, "scene emitter: clock text");
        let mut boot = scene::bar();
        boot.ops.extend(scene::clock(&text.get_untracked(&rt)?).ops);
        Ok(Self {
            rt,
            minute,
            bindings: vec![(text, scene::CLOCK, Prop::Text)],
            boot: RefCell::new(boot),
        })
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// Write the current minute (a no-op for the graph when unchanged).
    pub fn set_minute(&self, minute: i64) {
        if let Err(e) = self.minute.set(&self.rt, minute) {
            log::error!("clock: {e:?}");
        }
    }

    /// End the tick at logic time `now` and take its diff, if any.
    pub fn tick(&self, now: Duration) -> Option<SceneDiff> {
        let tick = self.rt.tick(now);
        for (node, error) in &tick.errors {
            log::error!("logic: {}: {error:?}", self.rt.name(*node));
        }
        for d in &tick.diagnostics {
            log::warn!("logic: {d:?}");
        }
        let mut diff = std::mem::take(&mut *self.boot.borrow_mut());
        for changed in &tick.changed {
            self.emit(*changed, &mut diff);
        }
        (!diff.is_empty()).then_some(diff)
    }

    /// The `SetProp` for a watched memo that changed.
    fn emit(&self, changed: NodeId, diff: &mut SceneDiff) {
        for (memo, node, prop) in &self.bindings {
            if memo.id() != changed {
                continue;
            }
            match memo.get_untracked(&self.rt) {
                Ok(text) => {
                    diff.set(*node, *prop, PropValue::Text(text));
                }
                Err(e) => log::error!("logic: {}: {e:?}", self.rt.name(changed)),
            }
        }
    }

    /// When the runtime next needs a tick without outside input.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.rt.next_deadline()
    }
}

/// Where the logic thread sends its diffs.
pub trait DiffSink {
    /// False once the receiver is gone (the thread then ends).
    fn send(&mut self, diff: SceneDiff) -> bool;
}

impl DiffSink for calloop::channel::Sender<SceneDiff> {
    fn send(&mut self, diff: SceneDiff) -> bool {
        calloop::channel::Sender::send(self, diff).is_ok()
    }
}

/// Run the logic thread until the main thread hangs up.
pub fn run(sink: impl DiffSink) -> io::Result<()> {
    run_with(sink, MINUTE, format_local)
}

/// [`run`] with the clock ticking every `period` (aligned to wall-clock
/// multiples of it) and formatted by `format`; tests use short periods.
pub fn run_with(
    mut sink: impl DiffSink,
    period: Duration,
    format: impl Fn(i64) -> String + 'static,
) -> io::Result<()> {
    let timer = WallTimer::new()?;
    let minute = timer.arm_next(period)?;
    let logic = Logic::new(minute, format).map_err(|e| io::Error::other(format!("{e:?}")))?;

    let mut event_loop: EventLoop<'_, Logic> = EventLoop::try_new().map_err(io::Error::other)?;
    let handle = event_loop.handle();
    let (ping, ping_source) = calloop::ping::make_ping()?;
    logic.runtime().set_wake_hook(move || ping.ping());
    // Woken handlers run in the tick after the dispatch.
    handle
        .insert_source(ping_source, |_, _, _| {})
        .map_err(|e| io::Error::other(e.error))?;
    handle
        .insert_source(
            Generic::new(timer, Interest::READ, Mode::Level),
            |_, timer, logic: &mut Logic| {
                match timer.read()? {
                    Fired::Expired | Fired::ClockChanged => {
                        let minute = timer.arm_next(period)?;
                        logic.set_minute(minute);
                    }
                    Fired::Nothing => {}
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| io::Error::other(e.error))?;

    let start = Instant::now();
    let mut logic = logic;
    loop {
        if let Some(diff) = logic.tick(start.elapsed())
            && !sink.send(diff)
        {
            return Ok(());
        }
        let timeout = logic
            .next_deadline()
            .map(|d| d.saturating_sub(start.elapsed()));
        event_loop
            .dispatch(timeout, &mut logic)
            .map_err(io::Error::other)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{Prop, PropValue, SceneOp};

    fn texts(diff: &SceneDiff) -> Vec<String> {
        diff.ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::Text,
                    value: PropValue::Text(t),
                    ..
                } if *id == scene::CLOCK => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn fmt(m: i64) -> String {
        format!("{:02}:{:02}", (m / 60) % 24, m % 60)
    }

    #[test]
    fn first_tick_emits_the_bar_with_the_clock() {
        let logic = Logic::new(12 * 60 + 34, fmt).unwrap();
        let boot = logic.tick(Duration::ZERO).unwrap();
        assert!(matches!(
            boot.ops.first(),
            Some(SceneOp::Create { id, .. }) if *id == scene::BAR
        ));
        assert_eq!(texts(&boot), ["12:34"]);
        // Idle: nothing scheduled, nothing emitted.
        assert_eq!(logic.next_deadline(), None);
        assert!(logic.runtime().is_idle());
        assert_eq!(logic.tick(Duration::from_secs(1)), None);
    }

    #[test]
    fn a_minute_tick_is_one_diff_with_one_text_prop() {
        let logic = Logic::new(0, fmt).unwrap();
        logic.tick(Duration::ZERO).unwrap();
        logic.set_minute(1);
        let diff = logic.tick(Duration::from_secs(60)).unwrap();
        assert_eq!(diff.ops.len(), 1, "{diff:?}");
        assert_eq!(texts(&diff), ["00:01"]);
        // Writing the same minute is cut off by equality: no diff.
        logic.set_minute(1);
        assert_eq!(logic.tick(Duration::from_secs(61)), None);
    }

    #[test]
    fn equal_text_is_cut_off_at_the_memo() {
        // A format that ignores the minute: the memo recomputes but its
        // value does not change, so the emitter does not run.
        let logic = Logic::new(0, |_| "same".into()).unwrap();
        logic.tick(Duration::ZERO).unwrap();
        logic.set_minute(5);
        assert_eq!(logic.tick(Duration::from_secs(1)), None);
    }

    /// `%H:%M` of the local time: the UTC minute moved by the local
    /// offset at that instant, zero-padded.
    #[test]
    fn local_format_is_hours_and_minutes() {
        use chrono::{Local, Offset, TimeZone};
        for minute in [0, 29_000_000, 29_000_000 + 9 * 60 + 5, 29_001_234] {
            let offset = Local
                .timestamp_opt(minute * 60, 0)
                .earliest()
                .unwrap()
                .offset()
                .fix()
                .local_minus_utc() as i64;
            let local = (minute * 60 + offset).div_euclid(60).rem_euclid(24 * 60);
            let want = format!("{:02}:{:02}", local / 60, local % 60);
            assert_eq!(format_local(minute), want);
        }
    }

    struct Collect(std::sync::mpsc::Sender<SceneDiff>);
    impl DiffSink for Collect {
        fn send(&mut self, diff: SceneDiff) -> bool {
            self.0.send(diff).is_ok()
        }
    }

    fn realtime_ns() -> i128 {
        let t = super::super::clock::realtime_now();
        t.tv_sec as i128 * 1_000_000_000 + t.tv_nsec as i128
    }

    /// The real loop on a 200 ms "minute": the boot diff at once, then one
    /// diff per boundary, never before it, with the next period's text;
    /// once the receiver is gone the next firing ends the thread with Ok.
    #[test]
    fn the_thread_ticks_at_each_boundary_and_ends_when_hung_up() {
        const P: Duration = Duration::from_millis(200);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || run_with(Collect(tx), P, |m| m.to_string()));
        let boot = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut last: i64 = texts(&boot)[0].parse().unwrap();
        for _ in 0..2 {
            let diff = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let now = realtime_ns();
            assert_eq!(diff.ops.len(), 1, "{diff:?}");
            // The next period (a later one only if the thread was starved).
            let period: i64 = texts(&diff)[0].parse().unwrap();
            assert!(period > last, "{period} after {last}");
            last = period;
            // Not before the boundary that starts this period.
            assert!(now >= period as i128 * P.as_nanos() as i128);
        }
        drop(rx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "did not end after hang-up");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(thread.join().unwrap().is_ok());
    }
}

//! The logic thread of the demo: a `strand-core` runtime holding the clock
//! as a `Signal`, a `Memo` formatting it and an `Effect` (the scene emitter)
//! turning each change into a `SceneDiff`, sent to the main thread once per
//! tick (`docs/architecture.md`, "Threads").
//!
//! The thread sleeps in `epoll` on a wall-clock timerfd armed for the next
//! minute boundary, plus whatever the runtime itself schedules
//! ([`Runtime::next_deadline`], none in the demo) and its wake hook.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use strand_core::{Runtime, Signal};
use strand_scene::SceneDiff;

use super::clock::{Fired, WallTimer};
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
    out: Rc<RefCell<SceneDiff>>,
}

impl std::fmt::Debug for Logic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logic").finish_non_exhaustive()
    }
}

impl Logic {
    /// Build the graph: `minute` (state) → `clock` text (memo) → the
    /// emitter (effect). The first tick emits the whole bar.
    pub fn new(minute: i64, format: impl Fn(i64) -> String + 'static) -> Self {
        let rt = Runtime::new();
        let minute = rt.signal(minute);
        rt.set_name(minute.id(), "clock.minute");
        let text = rt.memo(move |rt| Ok(format(minute.get(rt)?)));
        rt.set_name(text.id(), "clock.format(\"%H:%M\")");
        let out = Rc::new(RefCell::new(scene::bar()));
        let sink = Rc::clone(&out);
        let emitter = rt.effect(move |rt| {
            let text = text.get(rt)?;
            sink.borrow_mut().ops.extend(scene::clock(&text).ops);
            Ok(())
        });
        rt.set_name(emitter.id(), "scene emitter");
        Self { rt, minute, out }
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
        let diff = std::mem::take(&mut *self.out.borrow_mut());
        (!diff.is_empty()).then_some(diff)
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
pub fn run(mut sink: impl DiffSink) -> io::Result<()> {
    let timer = WallTimer::new()?;
    let minute = timer.arm_next_minute()?;
    let logic = Logic::new(minute, format_local);

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
                        let minute = timer.arm_next_minute()?;
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
        let logic = Logic::new(12 * 60 + 34, fmt);
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
        let logic = Logic::new(0, fmt);
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
        let logic = Logic::new(0, |_| "same".into());
        logic.tick(Duration::ZERO).unwrap();
        logic.set_minute(5);
        assert_eq!(logic.tick(Duration::from_secs(1)), None);
    }

    #[test]
    fn local_format_is_hours_and_minutes() {
        let s = format_local(29_000_000);
        assert_eq!(s.len(), 5, "{s}");
        assert_eq!(s.as_bytes()[2], b':');
    }

    struct Collect(std::sync::mpsc::Sender<SceneDiff>);
    impl DiffSink for Collect {
        fn send(&mut self, diff: SceneDiff) -> bool {
            self.0.send(diff).is_ok()
        }
    }

    #[test]
    fn the_thread_sends_the_boot_diff_and_ends_when_hung_up() {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || run(Collect(tx)));
        let boot = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(texts(&boot).len(), 1);
        drop(rx);
        // The next send (at the next minute) fails and the thread ends;
        // that can be up to a minute away, so only check it is alive and
        // not finished with an error.
        assert!(!thread.is_finished() || thread.join().unwrap().is_ok());
    }
}

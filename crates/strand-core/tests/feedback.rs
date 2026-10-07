//! Feedback loops: echo suppression with generation tags, and the >30
//! writes/s per cell per handler monitor.

use std::time::Duration;

use strand_core::rate::{MAX_WRITES_PER_SEC, THROTTLED_INTERVAL};
use strand_core::{Diagnostic, Generation, Received, Runtime};

/// Tests that only look at echo bookkeeping send nothing.
fn no_send<T>(_: &Runtime, _: &T, _: Generation) {}

#[test]
fn tagged_echo_of_pending_write_is_ignored() {
    let rt = Runtime::new();
    let volume = rt.signal(40u32);
    // Slider drag: three local writes in flight.
    let g1 = volume.write_tagged(&rt, 50, no_send).unwrap().unwrap();
    let g2 = volume.write_tagged(&rt, 60, no_send).unwrap().unwrap();
    let g3 = volume.write_tagged(&rt, 70, no_send).unwrap().unwrap();
    assert!(g1 < g2 && g2 < g3);
    assert_eq!(volume.pending_writes(&rt), 3);
    // The service echoes the first write late: ignored, slider stays at 70.
    assert_eq!(volume.receive(&rt, 50, Some(g1)), Ok(Received::Echo));
    assert_eq!(volume.get(&rt), Ok(70));
    assert_eq!(volume.receive(&rt, 60, Some(g2)), Ok(Received::Echo));
    assert_eq!(volume.get(&rt), Ok(70));
    // The last echo settles; nothing pending.
    assert_eq!(volume.receive(&rt, 70, Some(g3)), Ok(Received::Echo));
    assert_eq!(volume.pending_writes(&rt), 0);
    // A duplicate of an old echo is still ignored.
    assert_eq!(volume.receive(&rt, 50, Some(g1)), Ok(Received::Echo));
    assert_eq!(volume.get(&rt), Ok(70));
    // An outside change (another app) applies.
    assert_eq!(volume.receive(&rt, 30, None), Ok(Received::Applied));
    assert_eq!(volume.get(&rt), Ok(30));
}

/// An outside change overtakes a write the service has not answered yet:
/// the outside value applies at once, and the service's answer to the
/// write, handled after that change, settles the cell on what the service
/// now holds.
#[test]
fn a_write_answered_after_an_outside_change_settles() {
    let rt = Runtime::new();
    let volume = rt.signal(0.5f64);
    let g = volume.write_tagged(&rt, 0.6, no_send).unwrap().unwrap();
    // Another app set 0.3 before the service saw our write.
    assert_eq!(volume.receive(&rt, 0.3, None), Ok(Received::Applied));
    assert_eq!(volume.get(&rt), Ok(0.3));
    assert_eq!(volume.pending_writes(&rt), 0);
    // The service then applies our write and answers it.
    assert_eq!(volume.receive(&rt, 0.6, Some(g)), Ok(Received::Applied));
    assert_eq!(volume.get(&rt), Ok(0.6));
    // A duplicate of that answer changes nothing.
    assert_eq!(volume.receive(&rt, 0.6, Some(g)), Ok(Received::Echo));
    // With a newer write of ours pending, the overtaken write's answer is
    // an echo: the newer answer comes next.
    let g1 = volume.write_tagged(&rt, 0.7, no_send).unwrap().unwrap();
    assert_eq!(volume.receive(&rt, 0.2, None), Ok(Received::Applied));
    let g2 = volume.write_tagged(&rt, 0.8, no_send).unwrap().unwrap();
    assert_eq!(volume.receive(&rt, 0.7, Some(g1)), Ok(Received::Echo));
    assert_eq!(volume.get(&rt), Ok(0.8));
    assert_eq!(volume.receive(&rt, 0.8, Some(g2)), Ok(Received::Echo));
    assert_eq!(volume.get(&rt), Ok(0.8));
    assert_eq!(volume.pending_writes(&rt), 0);
}

#[test]
fn service_clamping_our_write_is_applied() {
    let rt = Runtime::new();
    let volume = rt.signal(40u32);
    let g = volume.write_tagged(&rt, 180, no_send).unwrap().unwrap();
    // The service acknowledges our write but caps it at 150.
    assert_eq!(volume.receive(&rt, 150, Some(g)), Ok(Received::Applied));
    assert_eq!(volume.get(&rt), Ok(150));
}

#[test]
fn untagged_echoes_match_by_value() {
    // D-Bus PropertiesChanged carries no tag.
    let rt = Runtime::new();
    let b = rt.signal(0.5f64);
    b.write_tagged(&rt, 0.6, no_send).unwrap().unwrap();
    b.write_tagged(&rt, 0.7, no_send).unwrap().unwrap();
    assert_eq!(b.receive(&rt, 0.6, None), Ok(Received::Echo));
    assert_eq!(b.get(&rt), Ok(0.7));
    assert_eq!(b.pending_writes(&rt), 1);
    assert_eq!(b.receive(&rt, 0.7, None), Ok(Received::Echo));
    assert_eq!(b.pending_writes(&rt), 0);
    // Not a pending value: an outside change wins and clears pending.
    b.write_tagged(&rt, 0.8, no_send).unwrap().unwrap();
    assert_eq!(b.receive(&rt, 0.2, None), Ok(Received::Applied));
    assert_eq!(b.get(&rt), Ok(0.2));
    assert_eq!(b.pending_writes(&rt), 0);
}

#[test]
fn echo_does_not_wake_observers() {
    let rt = Runtime::new();
    let v = rt.signal(1);
    rt.watch(v.id()).unwrap();
    let g = v.write_tagged(&rt, 2, no_send).unwrap().unwrap();
    assert_eq!(rt.flush().changed, vec![v.id()]);
    v.receive(&rt, 2, Some(g)).unwrap();
    assert!(rt.is_idle());
    assert!(rt.flush().changed.is_empty());
}

/// A handler (`every` timer) incrementing one cell at `hz` for `secs`.
/// Returns the diagnostics and the times at which the cell was written.
fn run_writer(
    hz: u32,
    secs: u32,
) -> (
    Runtime,
    strand_core::Signal<u32>,
    Vec<Diagnostic>,
    Vec<Duration>,
) {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    rt.set_name(cell.id(), "launcher.query");
    // Round up so `hz` writes never fit in less than a second.
    let period = Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(hz)));
    let running = rt.signal(true);
    let timer = rt.every(
        period,
        move |rt| running.get(rt),
        move |rt| cell.update(rt, |v| *v += 1),
    );
    rt.set_name(timer.id(), "every");
    let mut diags = Vec::new();
    let mut applied = Vec::new();
    let mut t = Duration::ZERO;
    for _ in 0..hz * secs {
        t += period;
        let tick = rt.tick(t);
        if tick.written.contains(&cell.id()) {
            applied.push(t);
        }
        diags.extend(tick.diagnostics);
    }
    // Stop it (disposing it would cancel its held write).
    running.set(&rt, false).unwrap();
    (rt, cell, diags, applied)
}

#[test]
fn thirty_writes_per_second_is_fine() {
    let (rt, cell, diags, applied) = run_writer(30, 3);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(cell.get(&rt), Ok(90));
    assert_eq!(applied.len(), 90);
}

#[test]
fn more_than_thirty_writes_per_second_warns_and_throttles() {
    let (rt, cell, diags, applied) = run_writer(120, 3);
    let warnings: Vec<_> = diags
        .iter()
        .filter_map(|d| match d {
            Diagnostic::WriteRate { names, .. } => Some(names.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings,
        vec!["every -> launcher.query".to_string()],
        "warns once"
    );
    // Once tripped (the 31st write within a second), a leaky bucket: one
    // write per 1/30 s, smoothly, never a burst followed by a stall.
    let tripped = applied[MAX_WRITES_PER_SEC];
    let after: Vec<Duration> = applied.iter().copied().filter(|&t| t > tripped).collect();
    for w in after.windows(2) {
        let gap = w[1] - w[0];
        assert!(
            gap >= THROTTLED_INTERVAL,
            "{gap:?} between throttled writes"
        );
        assert!(
            gap <= THROTTLED_INTERVAL + Duration::from_millis(9),
            "{gap:?}: the cell stalled"
        );
    }
    for (i, &t) in after.iter().enumerate() {
        let in_window = after[i..]
            .iter()
            .take_while(|&&u| u < t + Duration::from_secs(1))
            .count();
        assert!(in_window <= MAX_WRITES_PER_SEC + 1, "{in_window} writes/s");
    }
    // `x += 1` under throttling reads its own held write: no increment is
    // lost, and the held value lands once the window has room.
    rt.tick(rt.now() + Duration::from_secs(1));
    assert_eq!(cell.get(&rt), Ok(360));
}

#[test]
fn throttled_latest_value_is_applied_later() {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    let target = rt.signal(0u32);
    // An effect copying `target` into `cell` every tick, 100 ticks in 1 s.
    rt.effect(move |rt| {
        let v = target.get(rt)?;
        cell.set(rt, v)
    });
    let mut t = Duration::ZERO;
    for i in 1..=100u32 {
        target.set(&rt, i).unwrap();
        t += Duration::from_millis(10);
        rt.tick(t);
    }
    assert!(cell.get(&rt).unwrap() < 100, "throttled");
    assert!(
        rt.take_diagnostics().is_empty(),
        "warnings were drained into ticks"
    );
    assert!(
        rt.next_deadline().is_some(),
        "a deferred write is scheduled"
    );
    // When the window has room again, the latest value lands.
    rt.tick(t + Duration::from_secs(1));
    assert_eq!(cell.get(&rt), Ok(100));
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn newer_write_supersedes_a_throttled_one() {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    let target = rt.signal(0u32);
    rt.effect(move |rt| {
        let v = target.get(rt)?;
        cell.set(rt, v)
    });
    let mut t = Duration::ZERO;
    for i in 1..=50u32 {
        target.set(&rt, i).unwrap();
        t += Duration::from_millis(10);
        rt.tick(t);
    }
    // A service (outside any handler) writes the cell meanwhile.
    cell.set(&rt, 999).unwrap();
    rt.tick(t + Duration::from_secs(2));
    assert_eq!(cell.get(&rt), Ok(999), "stale throttled value never lands");
}

#[test]
fn many_writes_in_one_handler_run_count_once() {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    let trigger = rt.signal(0u32);
    rt.effect(move |rt| {
        trigger.get(rt)?;
        for _ in 0..100 {
            cell.update(rt, |v| *v += 1)?;
        }
        Ok(())
    });
    let tick = rt.flush();
    assert_eq!(cell.get(&rt), Ok(100), "read-your-writes inside a handler");
    assert!(tick.diagnostics.is_empty());
}

#[test]
fn writes_from_outside_handlers_are_not_throttled() {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    for i in 0..1000 {
        cell.set(&rt, i).unwrap();
        assert!(rt.flush().diagnostics.is_empty());
    }
    assert_eq!(cell.get(&rt), Ok(999));
}

#[test]
fn generations_are_ordered() {
    assert!(Generation(1) < Generation(2));
}

#[test]
fn throttling_covers_the_service_write_path() {
    // A listener of a service event writing back into the service at 200
    // writes per second (a loop through the service).
    use std::cell::RefCell;
    use std::rc::Rc;
    let rt = Runtime::new();
    let volume = rt.signal(0u32);
    let sent: Rc<RefCell<Vec<(u32, Generation)>>> = Rc::new(RefCell::new(Vec::new()));
    let report = rt.events::<u32>();
    let s = sent.clone();
    report
        .on(&rt, move |rt, v| {
            let s = s.clone();
            volume.write_tagged(rt, *v, move |_, v, g| s.borrow_mut().push((*v, g)))?;
            Ok(())
        })
        .unwrap();
    let mut t = Duration::ZERO;
    let mut warned = false;
    for v in 1..=200u32 {
        report.emit(&rt, v).unwrap();
        t += Duration::from_millis(5);
        let tick = rt.tick(t);
        warned |= tick
            .diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::WriteRate { .. }));
    }
    assert!(warned);
    // 30 before the trip, then one per 1/30 s.
    let n = sent.borrow().len();
    let bound = MAX_WRITES_PER_SEC
        + (Duration::from_secs(1).as_nanos() / THROTTLED_INTERVAL.as_nanos()) as usize
        + 1;
    assert!(n <= bound, "{n} service writes in one second");
    assert!(volume.pending_writes(&rt) <= strand_core::MAX_PENDING_ECHOES);
    // The latest value is sent once the window has room, with a fresh tag.
    rt.tick(t + Duration::from_secs(1));
    let last = *sent.borrow().last().unwrap();
    assert_eq!(last.0, 200);
    assert_eq!(volume.get(&rt), Ok(200));
    assert_eq!(volume.receive(&rt, 200, Some(last.1)), Ok(Received::Echo));
}

#[test]
fn pending_echoes_are_bounded() {
    let rt = Runtime::new();
    let v = rt.signal(0u32);
    for i in 0..1000 {
        v.write_tagged(&rt, i, no_send).unwrap();
    }
    assert_eq!(v.pending_writes(&rt), strand_core::MAX_PENDING_ECHOES);
}

#[test]
fn a_handler_spawning_one_task_per_event_is_one_writer() {
    // on sink.changed(dy) { volume += dy } (a service event, so a possible
    // loop), with a coroutine per event.
    let rt = Runtime::new();
    let volume = rt.signal(0i32);
    rt.set_name(volume.id(), "volume");
    let scroll = rt.events::<i32>();
    let listener = scroll
        .on(&rt, move |rt, dy| {
            let dy = *dy;
            let weak = rt.downgrade();
            rt.spawn(async move {
                let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                volume.update(&rt, |v| *v += dy)
            });
            Ok(())
        })
        .unwrap();
    rt.set_name(listener, "on_changed");
    let mut t = Duration::ZERO;
    let mut diags = Vec::new();
    for _ in 0..200 {
        scroll.emit(&rt, 1).unwrap();
        t += Duration::from_millis(5);
        diags.extend(rt.tick(t).diagnostics);
    }
    let warnings: Vec<String> = diags
        .into_iter()
        .filter_map(|d| match d {
            Diagnostic::WriteRate { names, .. } => Some(names.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(warnings, vec!["on_changed -> volume".to_string()]);
    // No scroll step is lost: held writes are read back by `update`.
    rt.tick(t + Duration::from_secs(1));
    assert_eq!(volume.get(&rt), Ok(200));
}

#[test]
fn spawn_for_names_the_handler_site() {
    // The VM starts each invocation from outside any handler.
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    let site = rt.handler_site();
    let mut t = Duration::ZERO;
    let mut diags = Vec::new();
    for i in 1..=100u32 {
        let weak = rt.downgrade();
        rt.spawn_for(site, async move {
            let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
            cell.set(&rt, i)
        });
        t += Duration::from_millis(5);
        diags.extend(rt.tick(t).diagnostics);
    }
    assert!(
        diags
            .iter()
            .any(|d| matches!(d, Diagnostic::WriteRate { writer, .. } if *writer == site))
    );
}

#[test]
fn rewriting_the_same_value_does_not_count() {
    let rt = Runtime::new();
    let cell = rt.signal(7u32);
    let timer = rt.every(
        Duration::from_millis(5),
        |_| Ok(true),
        move |rt| cell.set(rt, 7),
    );
    for i in 1..=400u32 {
        assert!(rt.tick(i * Duration::from_millis(5)).diagnostics.is_empty());
    }
    timer.dispose(&rt);
}

fn rate_warnings(diags: &[Diagnostic]) -> usize {
    diags
        .iter()
        .filter(|d| matches!(d, Diagnostic::WriteRate { .. }))
        .count()
}

/// `on scroll(dy) { volume += dy }` at 60 Hz for 2 s, three ways the VM can
/// run it: the listener body, a task per event spawned by the listener, and
/// a task per event started with `spawn_input`. Input is the user, not a
/// loop: no warning, and every step lands in its own tick.
#[test]
fn smooth_input_scrolling_is_neither_warned_nor_delayed() {
    for how in 0..3 {
        let rt = Runtime::new();
        let volume = rt.signal(0i32);
        let scroll = rt.input_events::<i32>();
        let site = rt.handler_site();
        if how < 2 {
            scroll
                .on(&rt, move |rt, dy| {
                    let dy = *dy;
                    if how == 0 {
                        return volume.update(rt, |v| *v += dy);
                    }
                    let weak = rt.downgrade();
                    rt.spawn(async move {
                        let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                        volume.update(&rt, |v| *v += dy)
                    });
                    Ok(())
                })
                .unwrap();
        }
        let mut diags = Vec::new();
        let mut t = Duration::ZERO;
        for i in 1..=120 {
            if how == 2 {
                let weak = rt.downgrade();
                rt.spawn_input(Some(site), async move {
                    let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                    volume.update(&rt, |v| *v += 1)
                });
            } else {
                scroll.emit(&rt, 1).unwrap();
            }
            t += Duration::from_nanos(16_666_667);
            let tick = rt.tick(t);
            assert!(
                tick.written.contains(&volume.id()),
                "case {how}: step {i} held"
            );
            assert_eq!(volume.get(&rt), Ok(i), "case {how}");
            diags.extend(tick.diagnostics);
        }
        assert_eq!(rate_warnings(&diags), 0, "case {how}: {diags:?}");
        assert_eq!(rt.next_deadline(), None, "case {how}: nothing held");
    }
}

/// The same 60 Hz writer driven by the graph (an `on change` handler
/// following a service value) is a loop: it warns and is throttled.
#[test]
fn a_graph_triggered_writer_is_still_throttled() {
    let rt = Runtime::new();
    let reported = rt.signal(0i32);
    let volume = rt.signal(0i32);
    rt.on_change(move |rt| reported.get(rt), move |rt, v| volume.set(rt, *v));
    rt.flush();
    let mut diags = Vec::new();
    let mut held = 0;
    let mut t = Duration::ZERO;
    for i in 1..=120 {
        reported.set(&rt, i).unwrap();
        t += Duration::from_nanos(16_666_667);
        let tick = rt.tick(t);
        if !tick.written.contains(&volume.id()) {
            held += 1;
        }
        diags.extend(tick.diagnostics);
    }
    assert_eq!(rate_warnings(&diags), 1);
    assert!(held > 30, "{held} held");
    rt.tick(t + Duration::from_secs(1));
    assert_eq!(volume.get(&rt), Ok(120), "latest value lands");
}

/// A held write of a handler that is then disposed (unmount, or a reload
/// cancelling it) never lands.
#[test]
fn a_held_write_of_a_disposed_handler_never_lands() {
    let rt = Runtime::new();
    let target = rt.signal(0u32);
    let cell = rt.signal(0u32);
    let effect = rt.effect(move |rt| {
        let v = target.get(rt)?;
        cell.set(rt, v)
    });
    let mut t = Duration::ZERO;
    for i in 1..=60u32 {
        target.set(&rt, i).unwrap();
        t += Duration::from_millis(10);
        rt.tick(t);
    }
    let shown = cell.get(&rt).unwrap();
    assert!(
        shown < 60 && rt.next_deadline().is_some(),
        "a write is held"
    );
    effect.dispose(&rt);
    assert_eq!(rt.next_deadline(), None, "dropped with its handler");
    rt.tick(t + Duration::from_secs(1));
    assert_eq!(cell.get(&rt), Ok(shown));
}

/// "Latest value wins" across handlers, even when the latest write equals
/// the current value.
#[test]
fn an_unchanged_newer_write_supersedes_a_held_one() {
    let rt = Runtime::new();
    let target = rt.signal(0u32);
    let cell = rt.signal(0u32);
    rt.effect(move |rt| {
        let v = target.get(rt)?;
        cell.set(rt, v)
    });
    let other = rt.events::<u32>();
    other.on(&rt, move |rt, v| cell.set(rt, *v)).unwrap();
    let mut t = Duration::ZERO;
    let mut i = 0;
    // Drive until a write is held (cell lags target).
    while cell.get(&rt) == target.get(&rt) {
        i += 1;
        target.set(&rt, i).unwrap();
        t += Duration::from_millis(5);
        rt.tick(t);
        assert!(i < 100);
    }
    let current = cell.get(&rt).unwrap();
    // Another handler writes the current value: newest write wins.
    other.emit(&rt, current).unwrap();
    rt.flush();
    rt.tick(t + Duration::from_secs(1));
    assert_eq!(cell.get(&rt), Ok(current));
}

/// `on click { loop { x += 1; await sleep(10ms) } }`: the click's
/// synchronous response is input, but the loop it starts is a 100 Hz
/// writer like any other: warned once and throttled.
#[test]
fn a_runaway_loop_started_by_a_click_is_throttled() {
    let rt = Runtime::new();
    let x = rt.signal(0i32);
    let clicks = rt.input_events::<()>();
    let first = rt.signal(0i32);
    clicks
        .on(&rt, move |rt, _| {
            // Synchronous response: exempt.
            first.set(rt, 1)?;
            let weak = rt.downgrade();
            rt.spawn(async move {
                let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                loop {
                    x.update(&rt, |v| *v += 1)?;
                    rt.sleep(Duration::from_millis(10)).await;
                }
            });
            Ok(())
        })
        .unwrap();
    clicks.emit(&rt, ()).unwrap();
    let mut diags = Vec::new();
    let mut landed = 0;
    for i in 0..=200u32 {
        let tick = rt.tick(i * Duration::from_millis(10));
        landed += usize::from(tick.written.contains(&x.id()));
        diags.extend(tick.diagnostics);
    }
    assert_eq!(first.get(&rt), Ok(1));
    assert_eq!(rate_warnings(&diags), 1, "{diags:?}");
    // The cell moves at ~30 Hz over 2 s instead of 100 Hz (`update` reads
    // the held value, so no step is lost, only batched).
    assert!(landed < 110, "{landed} writes landed");
}

/// `on scroll(dy) { await x; volume += dy }` at 60 Hz for 2 s: every
/// event's run awaits once, then writes once. That is one write per input
/// event, not a loop: no warning, and no step is held (each lands in the
/// tick its sleep comes due). Three ways the VM can start the run: a task
/// per event with `spawn_input` on one handler site, the same without a
/// site, and a task spawned by an input listener.
#[test]
fn input_tasks_writing_after_an_await_at_60_hz_are_not_throttled() {
    for how in 0..3 {
        let rt = Runtime::new();
        let volume = rt.signal(0i32);
        let site = rt.handler_site();
        let scroll = rt.input_events::<i32>();
        let body = move |rt: &Runtime, dy: i32| {
            let weak = rt.downgrade();
            async move {
                let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                rt.sleep(Duration::from_millis(1)).await;
                volume.update(&rt, |v| *v += dy)
            }
        };
        if how == 2 {
            scroll
                .on(&rt, move |rt, dy| {
                    rt.spawn(body(rt, *dy));
                    Ok(())
                })
                .unwrap();
        }
        let mut diags = Vec::new();
        let mut t = Duration::ZERO;
        for i in 1..=120 {
            match how {
                0 => drop(rt.spawn_input(Some(site), body(&rt, 1))),
                1 => drop(rt.spawn_input(None, body(&rt, 1))),
                _ => scroll.emit(&rt, 1).unwrap(),
            }
            t += Duration::from_nanos(16_666_667);
            let tick = rt.tick(t);
            diags.extend(tick.diagnostics);
            // The previous event's run woke from its 1 ms sleep and wrote.
            assert_eq!(volume.get(&rt), Ok(i - 1), "case {how}: step {i} held");
        }
        t += Duration::from_nanos(16_666_667);
        diags.extend(rt.tick(t).diagnostics);
        assert_eq!(volume.get(&rt), Ok(120), "case {how}");
        assert_eq!(rate_warnings(&diags), 0, "case {how}: {diags:?}");
        assert_eq!(rt.next_deadline(), None, "case {how}: nothing held");
    }
}

/// A loop inside one input-started run is still a loop: counted against
/// the run, it warns once (named by its handler site) and is throttled.
#[test]
fn a_loop_inside_one_input_run_is_throttled() {
    let rt = Runtime::new();
    let x = rt.signal(0i32);
    let site = rt.handler_site();
    let weak = rt.downgrade();
    rt.spawn_input(Some(site), async move {
        let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
        loop {
            x.update(&rt, |v| *v += 1)?;
            rt.sleep(Duration::from_millis(10)).await;
        }
    });
    let mut diags = Vec::new();
    let mut landed = 0;
    for i in 0..=200u32 {
        let tick = rt.tick(i * Duration::from_millis(10));
        landed += usize::from(tick.written.contains(&x.id()));
        diags.extend(tick.diagnostics);
    }
    let warned: Vec<_> = diags
        .iter()
        .filter_map(|d| match d {
            Diagnostic::WriteRate { writer, .. } => Some(*writer),
            _ => None,
        })
        .collect();
    assert_eq!(warned, vec![site], "{diags:?}");
    assert!(landed < 110, "{landed} writes landed");
    // `update` reads the held value: no step is lost, at most the latest
    // ones are still held.
    let v = x.get(&rt).unwrap();
    assert!((195..=201).contains(&v), "{v}");
}

/// The input exemption covers a spawned task's synchronous part: a task
/// per click that writes once before any await is never counted.
#[test]
fn input_tasks_writing_before_their_first_await_stay_exempt() {
    let rt = Runtime::new();
    let x = rt.signal(0i32);
    let site = rt.handler_site();
    let mut diags = Vec::new();
    for i in 1..=120u32 {
        let weak = rt.downgrade();
        rt.spawn_input(Some(site), async move {
            let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
            x.update(&rt, |v| *v += 1)?;
            rt.sleep(Duration::from_secs(5)).await;
            Ok(())
        });
        diags.extend(rt.tick(i * Duration::from_nanos(16_666_667)).diagnostics);
        assert_eq!(x.get(&rt), Ok(i as i32));
    }
    assert_eq!(rate_warnings(&diags), 0);
}

/// A timer body and the task it spawns are one writer; their writes in
/// `advance_to` and in the following flush are separate attempts, so 2
/// writes per 50 ms tick (40/s) is over the limit.
#[test]
fn timer_body_and_its_task_writes_count_separately() {
    let rt = Runtime::new();
    let x = rt.signal(0i32);
    rt.every(
        Duration::from_millis(50),
        |_| Ok(true),
        move |rt| {
            x.update(rt, |v| *v += 1)?;
            let weak = rt.downgrade();
            rt.spawn(async move {
                let rt = weak.upgrade().ok_or(strand_core::Error::Cancelled)?;
                x.update(&rt, |v| *v += 1)
            });
            Ok(())
        },
    );
    rt.flush();
    let mut diags = Vec::new();
    for i in 1..=40u32 {
        diags.extend(rt.tick(i * Duration::from_millis(50)).diagnostics);
    }
    assert_eq!(rate_warnings(&diags), 1, "{diags:?}");
}

#[test]
fn keyed_collection_writes_are_throttled_without_losing_items() {
    use strand_core::{KeyedSource, KeyedVec};
    let rt = Runtime::new();
    let log = rt.keyed(KeyedVec::new(|e: &(u32, u32)| e.0));
    rt.set_name(log.id(), "history");
    let running = rt.signal(true);
    let next = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let n = next.clone();
    // A runaway loop: every 8 ms push a row and drop the oldest past 50.
    let timer = rt.every(
        Duration::from_millis(8),
        move |rt| running.get(rt),
        move |rt| {
            let i = n.get();
            n.set(i + 1);
            log.push(rt, (i, i * 2))?;
            // The handler reads its own held writes: the duplicate is
            // caught at once, and the oldest row it pushed can be removed.
            assert!(log.push(rt, (i, 0)).is_err(), "duplicate of a held push");
            if i >= 50 {
                log.remove_key(rt, &(i - 50))?;
            }
            Ok(())
        },
    );
    rt.set_name(timer.id(), "every");
    let snap = log.snapshot(&rt).unwrap();
    let mut mirror: Vec<(u32, (u32, u32))> = Vec::new();
    let mut seen = Some(snap.version());
    let mut diags = Vec::new();
    let mut written = Vec::new();
    let mut t = Duration::ZERO;
    for _ in 0..375 {
        t += Duration::from_millis(8);
        let tick = rt.tick(t);
        if tick.written.contains(&log.id()) {
            written.push(t);
        }
        diags.extend(tick.diagnostics);
        // A consumer following diffs (the scene emitter) stays exact.
        let snap = log.snapshot(&rt).unwrap();
        for d in snap.diffs_or_reset(seen) {
            d.apply(&mut mirror).unwrap();
        }
        seen = Some(snap.version());
        assert_eq!(mirror.as_slice(), snap.items());
    }
    let warnings: Vec<String> = diags
        .iter()
        .filter_map(|d| match d {
            Diagnostic::WriteRate { names, .. } => Some(names.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(warnings, vec!["every -> history".to_string()], "warns once");
    // Throttled to 30 changes a second once tripped.
    let tripped = written[MAX_WRITES_PER_SEC];
    let after: Vec<Duration> = written.iter().copied().filter(|&w| w > tripped).collect();
    assert!(!after.is_empty());
    for w in after.windows(2) {
        assert!(w[1] - w[0] >= THROTTLED_INTERVAL, "{:?}", w[1] - w[0]);
    }
    // Stop the loop; the held copy lands: every push and removal counted.
    running.set(&rt, false).unwrap();
    rt.tick(t + Duration::from_secs(1));
    let pushed = next.get();
    let items: Vec<u32> = log.snapshot(&rt).unwrap().keys().copied().collect();
    assert_eq!(items, (pushed - 50..pushed).collect::<Vec<_>>());
}

#[test]
fn input_handlers_and_services_write_collections_unthrottled() {
    use strand_core::KeyedVec;
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|e: &u32| *e));
    let clicks = rt.input_events::<u32>();
    clicks.on(&rt, move |rt, &i| xs.push(rt, i)).unwrap();
    let mut t = Duration::ZERO;
    let mut changes = 0;
    for i in 0..120 {
        t += Duration::from_millis(5);
        clicks.emit(&rt, i).unwrap();
        // A service batch from outside any handler.
        xs.apply(
            &rt,
            &[strand_core::VecDiff::Insert {
                index: 0,
                key: 1000 + i,
                value: 1000 + i,
            }],
        )
        .unwrap();
        let tick = rt.tick(t);
        changes += usize::from(tick.written.contains(&xs.id()));
        assert!(tick.diagnostics.is_empty(), "{:?}", tick.diagnostics);
    }
    assert_eq!(changes, 120);
    assert_eq!(xs.get_untracked(&rt).unwrap().len(), 240);
}

/// A runaway `every 5ms` pusher on `xs` (keys from `first`), throttled
/// after the first 30 pushes. Returns the timer's `running` switch and how
/// many rows it pushed.
fn runaway_pusher(
    rt: &Runtime,
    xs: strand_core::KeyedSignal<u32, u32>,
    first: u32,
) -> (strand_core::Signal<bool>, std::rc::Rc<std::cell::Cell<u32>>) {
    let running = rt.signal(true);
    let pushed = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let p = pushed.clone();
    rt.every(
        Duration::from_millis(5),
        move |rt| running.get(rt),
        move |rt| {
            xs.push(rt, first + p.get())?;
            p.set(p.get() + 1);
            Ok(())
        },
    );
    (running, pushed)
}

/// Tick every 5 ms from `t` for `n` ticks; returns the new time and the
/// diagnostics.
fn run_for(rt: &Runtime, t: &mut Duration, n: usize) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for _ in 0..n {
        *t += Duration::from_millis(5);
        diags.extend(rt.tick(*t).diagnostics);
    }
    diags
}

fn sorted_keys(xs: strand_core::KeyedSignal<u32, u32>, rt: &Runtime) -> Vec<u32> {
    let mut k: Vec<u32> = xs
        .with(rt, |v| v.items().iter().map(|(k, _)| *k).collect())
        .unwrap();
    k.sort_unstable();
    k
}

#[test]
fn a_held_collection_copy_survives_an_input_write() {
    // `every 5ms { history.push(..) }` gets throttled; a click pushes one
    // row in place meanwhile. Neither loses a row.
    use strand_core::KeyedVec;
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|e: &u32| *e));
    let (running, pushed) = runaway_pusher(&rt, xs, 0);
    let clicks = rt.input_events::<u32>();
    clicks.on(&rt, move |rt, &k| xs.push(rt, k)).unwrap();
    let mut t = Duration::ZERO;
    run_for(&rt, &mut t, 60);
    assert!(
        xs.with(&rt, |v| v.len()).unwrap() < pushed.get() as usize,
        "throttled"
    );
    clicks.emit(&rt, 10_000).unwrap();
    run_for(&rt, &mut t, 1);
    running.set(&rt, false).unwrap();
    let diags = run_for(&rt, &mut t, 40);
    assert!(
        !diags
            .iter()
            .any(|d| matches!(d, Diagnostic::KeyedConflict { .. }))
    );
    let mut want: Vec<u32> = (0..pushed.get()).collect();
    want.push(10_000);
    assert_eq!(sorted_keys(xs, &rt), want);
}

#[test]
fn a_held_collection_copy_survives_a_service_batch() {
    use strand_core::{KeyedVec, VecDiff};
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|e: &u32| *e));
    let (running, pushed) = runaway_pusher(&rt, xs, 0);
    let mut t = Duration::ZERO;
    run_for(&rt, &mut t, 60);
    // A service inserts at the front from outside any handler.
    xs.apply(
        &rt,
        &[VecDiff::Insert {
            index: 0,
            key: 20_000,
            value: 20_000,
        }],
    )
    .unwrap();
    running.set(&rt, false).unwrap();
    run_for(&rt, &mut t, 40);
    let keys: Vec<u32> = xs
        .with(&rt, |v| v.items().iter().map(|(k, _)| *k).collect())
        .unwrap();
    assert_eq!(keys[0], 20_000, "the service's row keeps its place");
    let mut want: Vec<u32> = (0..pushed.get()).collect();
    want.push(20_000);
    assert_eq!(sorted_keys(xs, &rt), want);
    // Pushes stay in push order.
    assert!(keys[1..].windows(2).all(|w| w[0] < w[1]), "{keys:?}");
}

#[test]
fn two_throttled_writers_on_one_list_keep_both_their_rows() {
    use strand_core::KeyedVec;
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|e: &u32| *e));
    let (a_running, a) = runaway_pusher(&rt, xs, 0);
    let (b_running, b) = runaway_pusher(&rt, xs, 50_000);
    let mut t = Duration::ZERO;
    let mut diags = run_for(&rt, &mut t, 80);
    a_running.set(&rt, false).unwrap();
    b_running.set(&rt, false).unwrap();
    diags.extend(run_for(&rt, &mut t, 40));
    assert_eq!(
        diags
            .iter()
            .filter(|d| matches!(d, Diagnostic::WriteRate { .. }))
            .count(),
        2,
        "both throttled: {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| matches!(d, Diagnostic::KeyedConflict { .. }))
    );
    let mut want: Vec<u32> = (0..a.get()).chain(50_000..50_000 + b.get()).collect();
    want.sort_unstable();
    assert_eq!(sorted_keys(xs, &rt), want);
}

#[test]
fn a_held_insert_of_a_key_another_write_inserted_is_reported() {
    use strand_core::KeyedVec;
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|e: &u32| *e));
    let (running, pushed) = runaway_pusher(&rt, xs, 0);
    let mut t = Duration::ZERO;
    run_for(&rt, &mut t, 60);
    // A click inserts the key the pusher holds next-to-last.
    let held_key = pushed.get() - 1;
    assert!(!xs.with(&rt, |v| v.contains_key(&held_key)).unwrap());
    let clicks = rt.input_events::<u32>();
    clicks.on(&rt, move |rt, &k| xs.push(rt, k)).unwrap();
    clicks.emit(&rt, held_key).unwrap();
    let mut diags = run_for(&rt, &mut t, 1);
    running.set(&rt, false).unwrap();
    diags.extend(run_for(&rt, &mut t, 40));
    assert!(
        diags.iter().any(|d| matches!(
            d,
            Diagnostic::KeyedConflict { cell, skipped: 1, .. } if *cell == xs.id()
        )),
        "{diags:?}"
    );
    assert_eq!(sorted_keys(xs, &rt), (0..pushed.get()).collect::<Vec<_>>());
}

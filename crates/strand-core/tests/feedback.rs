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

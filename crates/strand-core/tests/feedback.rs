//! Feedback loops: echo suppression with generation tags, and the >30
//! writes/s per cell per handler monitor.

use std::time::Duration;

use strand_core::{Diagnostic, Generation, Received, Runtime, rate::MAX_WRITES_PER_SEC};

#[test]
fn tagged_echo_of_pending_write_is_ignored() {
    let rt = Runtime::new();
    let volume = rt.signal(40u32);
    // Slider drag: three local writes in flight.
    let g1 = volume.write_tagged(&rt, 50).unwrap();
    let g2 = volume.write_tagged(&rt, 60).unwrap();
    let g3 = volume.write_tagged(&rt, 70).unwrap();
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
    let g = volume.write_tagged(&rt, 180).unwrap();
    // The service acknowledges our write but caps it at 150.
    assert_eq!(volume.receive(&rt, 150, Some(g)), Ok(Received::Applied));
    assert_eq!(volume.get(&rt), Ok(150));
}

#[test]
fn untagged_echoes_match_by_value() {
    // D-Bus PropertiesChanged carries no tag.
    let rt = Runtime::new();
    let b = rt.signal(0.5f64);
    b.write_tagged(&rt, 0.6).unwrap();
    b.write_tagged(&rt, 0.7).unwrap();
    assert_eq!(b.receive(&rt, 0.6, None), Ok(Received::Echo));
    assert_eq!(b.get(&rt), Ok(0.7));
    assert_eq!(b.pending_writes(&rt), 1);
    assert_eq!(b.receive(&rt, 0.7, None), Ok(Received::Echo));
    assert_eq!(b.pending_writes(&rt), 0);
    // Not a pending value: an outside change wins and clears pending.
    b.write_tagged(&rt, 0.8).unwrap();
    assert_eq!(b.receive(&rt, 0.2, None), Ok(Received::Applied));
    assert_eq!(b.get(&rt), Ok(0.2));
    assert_eq!(b.pending_writes(&rt), 0);
}

#[test]
fn echo_does_not_wake_observers() {
    let rt = Runtime::new();
    let v = rt.signal(1);
    rt.watch(v.id()).unwrap();
    let g = v.write_tagged(&rt, 2).unwrap();
    assert_eq!(rt.flush().changed, vec![v.id()]);
    v.receive(&rt, 2, Some(g)).unwrap();
    assert!(rt.is_idle());
    assert!(rt.flush().changed.is_empty());
}

/// A handler (`every` timer) writing one cell at `hz` for `secs`.
fn run_writer(hz: u32, secs: u32) -> (Runtime, strand_core::Signal<u32>, Vec<Diagnostic>) {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    rt.set_name(cell.id(), "launcher.query");
    // Round up so `hz` writes never fit in less than a second.
    let period = Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(hz)));
    let timer = rt.every(period, |_| Ok(true), move |rt| cell.update(rt, |v| *v += 1));
    rt.set_name(timer.id(), "every");
    let mut diags = Vec::new();
    let mut t = Duration::ZERO;
    for _ in 0..hz * secs {
        t += period;
        rt.tick(t);
        diags.extend(rt.take_diagnostics());
    }
    (rt, cell, diags)
}

#[test]
fn thirty_writes_per_second_is_fine() {
    let (rt, cell, diags) = run_writer(30, 3);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(cell.get(&rt), Ok(90));
}

#[test]
fn more_than_thirty_writes_per_second_warns_and_throttles() {
    let (rt, cell, diags) = run_writer(120, 3);
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
    // Throttled to at most 30 applied writes per second (plus the
    // latest-value catch-up write each window).
    let applied = cell.get(&rt).unwrap() as usize;
    assert!(applied <= 3 * (MAX_WRITES_PER_SEC + 1), "{applied}");
    assert!(applied >= 3 * MAX_WRITES_PER_SEC - 1, "{applied}");
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
    rt.flush();
    assert_eq!(cell.get(&rt), Ok(100), "read-your-writes inside a handler");
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn writes_from_outside_handlers_are_not_throttled() {
    let rt = Runtime::new();
    let cell = rt.signal(0u32);
    for i in 0..1000 {
        cell.set(&rt, i).unwrap();
        rt.flush();
    }
    assert_eq!(cell.get(&rt), Ok(999));
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn generations_are_ordered() {
    assert!(Generation(1) < Generation(2));
}

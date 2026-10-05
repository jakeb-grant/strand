//! Handler semantics: who owns what a handler creates, cancelled loads,
//! derived `Async`, timers that see this tick's writes, `every` phase,
//! identity-aware `on change`, and bounded flushes.

use std::cell::RefCell;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use strand_core::{Async, Diagnostic, Error, Runtime, WeakRuntime};

const MS: Duration = Duration::from_millis(1);

/// A load that sleeps `d` on the logic clock, then yields `v`.
async fn slow<T>(weak: WeakRuntime, d: Duration, v: T) -> Result<T, Error> {
    if let Some(rt) = weak.upgrade() {
        rt.sleep(d).await;
    }
    Ok(v)
}

#[test]
fn a_load_started_by_a_handler_outlives_the_handler() {
    // on click { hits.load(...) }
    let rt = Runtime::new();
    let mut diags = Vec::new();
    let click = rt.events::<()>();
    let hits = rt.signal(Async::<u32>::empty());
    let weak = rt.downgrade();
    let (_component, _) = rt.scope(|rt| {
        click
            .on(rt, move |rt, _| {
                hits.load(rt, slow(weak.clone(), 50 * MS, 7)).map(|_| ())
            })
            .unwrap()
    });
    click.emit(&rt, ()).unwrap();
    diags.extend(rt.flush().diagnostics);
    assert!(hits.get(&rt).unwrap().pending());
    diags.extend(rt.tick(50 * MS).diagnostics);
    let a = hits.get(&rt).unwrap();
    assert_eq!(a.value(), Some(&7));
    assert!(!a.pending());
    assert!(
        diags.is_empty(),
        "the load was not cancelled when the click handler returned"
    );
}

#[test]
fn handlers_do_not_accumulate_nodes() {
    let rt = Runtime::new();
    let ev = rt.events::<u32>();
    let total = rt.signal(0u32);
    let (component, _) = rt.scope(|rt| {
        ev.on(rt, move |rt, n| {
            let n = *n;
            rt.spawn(async move {
                let _ = n;
                Ok(())
            });
            total.update(rt, |t| *t += n)
        })
        .unwrap()
    });
    rt.flush();
    let before = rt.stats().nodes;
    for i in 0..100 {
        ev.emit(&rt, i).unwrap();
        // 25 events a second: under the write-rate limit.
        rt.tick(i * 40 * MS);
    }
    assert_eq!(total.get(&rt), Ok(4950));
    assert_eq!(rt.stats().nodes, before, "finished handler tasks are gone");
    assert_eq!(
        rt.owned(component.id()).unwrap().len(),
        1,
        "just the listener"
    );
}

#[test]
fn timer_and_on_change_bodies_create_nodes_for_their_component() {
    let rt = Runtime::new();
    let mut diags = Vec::new();
    let x = rt.signal(0);
    let hits = rt.signal(Async::<u32>::empty());
    let made = Rc::new(RefCell::new(Vec::new()));
    let weak = rt.downgrade();
    let w2 = weak.clone();
    let (m1, m2) = (made.clone(), made.clone());
    let (component, (timer, effect)) = rt.scope(|rt| {
        let t = rt.after(
            10 * MS,
            |_| Ok(true),
            move |rt| {
                m1.borrow_mut().push(rt.signal(0).id());
                hits.load(rt, slow(weak.clone(), 100 * MS, 1)).map(|_| ())
            },
        );
        let e = rt.on_change(
            move |rt| x.get(rt),
            move |rt, _| {
                m2.borrow_mut().push(rt.signal(0).id());
                hits.load(rt, slow(w2.clone(), 100 * MS, 2)).map(|_| ())
            },
        );
        (t, e)
    });
    diags.extend(rt.flush().diagnostics);
    diags.extend(rt.tick(10 * MS).diagnostics);
    x.set(&rt, 1).unwrap();
    diags.extend(rt.flush().diagnostics);
    x.set(&rt, 2).unwrap();
    diags.extend(rt.flush().diagnostics);
    // Nodes the bodies create belong to the component.
    for &n in made.borrow().iter() {
        assert_eq!(rt.owner_of(n), Ok(Some(component.id())));
    }
    // Their loads belong to the handlers' sites: not the component, and not
    // the effect itself (whose re-run would cancel them).
    assert!(rt.owned(effect.id()).unwrap().is_empty());
    let timer_site = rt.site_of(timer.id()).unwrap();
    assert_eq!(rt.owned(timer_site).unwrap().len(), 1, "the timer's load");
    let effect_site = rt.site_of(effect.id()).unwrap();
    assert_eq!(rt.owned(effect_site).unwrap().len(), 2, "both loads alive");
    diags.extend(rt.tick(200 * MS).diagnostics);
    assert_eq!(hits.get(&rt).unwrap().value(), Some(&2));
    assert!(diags.is_empty(), "nothing was cancelled");
}

/// Reload restarts changed handler code: disposing a listener, a timer or
/// an `on change` handler cancels its in-flight `await`, reports it, and
/// leaves nothing scheduled.
#[test]
fn disposing_a_handler_cancels_its_in_flight_tasks() {
    for which in 0..3 {
        let rt = Runtime::new();
        let x = rt.signal(0);
        let after = rt.signal(0);
        let click = rt.events::<()>();
        let body = move |rt: &Runtime| {
            let weak = rt.downgrade();
            rt.spawn(async move {
                let rt = weak.upgrade().ok_or(Error::Cancelled)?;
                rt.sleep(Duration::from_secs(5)).await;
                after.set(&rt, 1)
            });
            Ok(())
        };
        let (_component, handler) = rt.scope(|rt| match which {
            0 => click.on(rt, move |rt, _| body(rt)).unwrap(),
            1 => rt.after(10 * MS, |_| Ok(true), body).id(),
            _ => rt
                .on_change(move |rt| x.get(rt), move |rt, _| body(rt))
                .id(),
        });
        rt.flush();
        click.emit(&rt, ()).unwrap();
        x.set(&rt, 1).unwrap();
        rt.flush();
        rt.tick(10 * MS);
        assert!(rt.next_deadline().is_some(), "case {which}: sleeping");
        rt.take_diagnostics();
        rt.dispose(handler);
        let diags = rt.take_diagnostics();
        assert!(
            matches!(diags.as_slice(), [Diagnostic::Cancelled { .. }]),
            "case {which}: {diags:?}"
        );
        assert_eq!(rt.next_deadline(), None, "case {which}");
        rt.tick(Duration::from_secs(6));
        assert_eq!(after.get(&rt), Ok(0), "case {which}: never resumed");
    }
}

/// A timer or `on change` re-evaluating its condition does not cancel the
/// tasks its body started.
#[test]
fn reevaluating_a_handler_keeps_its_tasks() {
    let rt = Runtime::new();
    let mut diags = Vec::new();
    let hover = rt.signal(false);
    let done = rt.signal(0);
    let weak = rt.downgrade();
    let timer = rt.every(
        10 * MS,
        move |rt| Ok(!hover.get(rt)?),
        move |rt| {
            let weak = weak.clone();
            rt.spawn(async move {
                let rt = weak.upgrade().ok_or(Error::Cancelled)?;
                rt.sleep(Duration::from_secs(1)).await;
                done.update(&rt, |d| *d += 1)
            });
            Ok(())
        },
    );
    diags.extend(rt.tick(10 * MS).diagnostics);
    hover.set(&rt, true).unwrap();
    diags.extend(rt.tick(20 * MS).diagnostics);
    assert_eq!(rt.owned(rt.site_of(timer.id()).unwrap()).unwrap().len(), 1);
    diags.extend(rt.tick(Duration::from_secs(2)).diagnostics);
    assert_eq!(done.get(&rt), Ok(1));
    assert!(diags.is_empty());
}

#[test]
fn a_cancelled_load_clears_pending() {
    let rt = Runtime::new();
    let hits = rt.signal(Async::<u32>::empty());
    let weak = rt.downgrade();
    let (scope, _) = rt.scope(|rt| hits.load(rt, slow(weak, Duration::from_secs(5), 7)));
    rt.flush();
    assert!(hits.get(&rt).unwrap().pending());
    scope.dispose(&rt);
    let a = hits.get(&rt).unwrap();
    assert!(!a.pending(), "no spinner forever");
    assert_eq!(a.value(), None);
    assert_eq!(a.error(), None, "a cancellation is not an error");
}

#[test]
fn a_superseded_cancelled_load_leaves_the_newer_one_pending() {
    let rt = Runtime::new();
    let hits = rt.signal(Async::<u32>::empty());
    let weak = rt.downgrade();
    let (old, _) = rt.scope(|rt| hits.load(rt, slow(weak.clone(), Duration::from_secs(5), 1)));
    hits.load(&rt, slow(weak, Duration::from_secs(5), 2))
        .unwrap();
    rt.flush();
    old.dispose(&rt);
    assert!(
        hits.get(&rt).unwrap().pending(),
        "the newer load is in flight"
    );
    rt.tick(Duration::from_secs(5));
    assert_eq!(hits.get(&rt).unwrap().value(), Some(&2));
}

#[test]
fn many_begins_per_second_from_one_handler_stay_consistent() {
    // Search-as-you-type with key repeat: 60 loads a second from one
    // handler. Async bookkeeping is not rate-gated, so ids stay distinct
    // and the last load resolves.
    let rt = Runtime::new();
    let key = rt.events::<u32>();
    let hits = rt.signal(Async::<u32>::empty());
    let ids = Rc::new(RefCell::new(Vec::new()));
    let i2 = ids.clone();
    let weak = rt.downgrade();
    key.on(&rt, move |rt, n| {
        let task = hits.load(rt, slow(weak.clone(), 5 * MS, *n))?;
        i2.borrow_mut().push(task.id());
        Ok(())
    })
    .unwrap();
    let mut t = Duration::ZERO;
    for n in 0..120 {
        key.emit(&rt, n).unwrap();
        t += Duration::from_micros(16_667);
        rt.tick(t);
    }
    rt.tick(t + Duration::from_secs(2));
    let a = hits.get(&rt).unwrap();
    assert!(!a.pending());
    assert_eq!(a.value(), Some(&119));
}

#[test]
fn async_memo_follows_its_input_and_supersedes_quietly() {
    let rt = Runtime::new();
    let query = rt.signal(String::new());
    let weak = rt.downgrade();
    let hits = rt.async_memo(
        move |rt| query.get(rt),
        move |q: String| slow(weak.clone(), 20 * MS, q.len()),
    );
    let pending = rt.memo(move |rt| Ok(hits.get(rt)?.pending()));
    let mut diags = rt.flush().diagnostics;
    assert_eq!(pending.get(&rt), Ok(true));
    diags.extend(rt.tick(20 * MS).diagnostics);
    assert_eq!(hits.get(&rt).unwrap().value(), Some(&0));
    // Typing: each keystroke supersedes the previous request.
    for (i, q) in ["f", "fi", "fir"].iter().enumerate() {
        query.set(&rt, q.to_string()).unwrap();
        diags.extend(rt.tick((25 + i as u32 * 5) * MS).diagnostics);
        assert!(hits.get(&rt).unwrap().pending());
        assert_eq!(
            hits.get(&rt).unwrap().value(),
            Some(&0),
            "kept while typing"
        );
    }
    diags.extend(rt.tick(100 * MS).diagnostics);
    let a = hits.get(&rt).unwrap();
    assert_eq!(a.value(), Some(&3));
    assert!(!a.pending());
    assert!(
        diags.is_empty(),
        "superseded requests are not reported as cancelled"
    );
    // An input error becomes the cell's error; the value is kept.
    let flag = rt.signal(false);
    let weak = rt.downgrade();
    let failing = rt.async_memo(
        move |rt| {
            if flag.get(rt)? {
                Err(Error::failed("no index"))
            } else {
                Ok(1u32)
            }
        },
        move |v| slow(weak.clone(), MS, v),
    );
    rt.flush();
    rt.tick(200 * MS);
    flag.set(&rt, true).unwrap();
    rt.flush();
    let a = failing.get(&rt).unwrap();
    assert_eq!(a.error(), Some(&Error::failed("no index")));
    assert_eq!(a.value(), Some(&1));
}

#[test]
fn a_timer_sees_a_pause_written_in_the_same_tick() {
    // after 6s while !hover: the pointer enters at 5.9 s and the host
    // batches input with the next tick at 6.016 s.
    let rt = Runtime::new();
    let hover = rt.signal(false);
    let expired = rt.signal(false);
    rt.after(
        Duration::from_secs(6),
        move |rt| Ok(!hover.get(rt)?),
        move |rt| expired.set(rt, true),
    );
    rt.tick(5900 * MS);
    hover.set(&rt, true).unwrap();
    rt.tick(6016 * MS);
    assert_eq!(expired.get(&rt), Ok(false), "paused while hovered");
    assert_eq!(rt.next_deadline(), None);
    // 0.1 s were left at 5.9 s.
    hover.set(&rt, false).unwrap();
    rt.tick(7000 * MS);
    assert_eq!(rt.next_deadline(), Some(7100 * MS));
    rt.tick(7100 * MS);
    assert_eq!(expired.get(&rt), Ok(true));
}

#[test]
fn a_timer_body_pausing_another_timer_takes_effect_at_once() {
    let rt = Runtime::new();
    let paused = rt.signal(false);
    let fired = rt.signal(false);
    rt.after(10 * MS, |_| Ok(true), move |rt| paused.set(rt, true));
    rt.after(
        20 * MS,
        move |rt| Ok(!paused.get(rt)?),
        move |rt| fired.set(rt, true),
    );
    rt.tick(20 * MS);
    assert_eq!(fired.get(&rt), Ok(false));
}

#[test]
fn every_keeps_its_phase_despite_late_ticks() {
    let rt = Runtime::new();
    let fires = Rc::new(RefCell::new(Vec::new()));
    let f = fires.clone();
    let weak = rt.downgrade();
    rt.every(
        Duration::from_secs(1),
        |_| Ok(true),
        move |_| {
            if let Some(rt) = weak.upgrade() {
                f.borrow_mut().push(rt.now());
            }
            Ok(())
        },
    );
    for _ in 0..10 {
        let due = rt.next_deadline().unwrap();
        // The host wakes 16 ms late every time.
        rt.tick(due + 16 * MS);
    }
    assert_eq!(rt.next_deadline(), Some(Duration::from_secs(11)));
    // After a long gap: one fire, then a new phase (no replay).
    rt.tick(Duration::from_millis(15_500));
    assert_eq!(fires.borrow().len(), 11);
    assert_eq!(rt.next_deadline(), Some(Duration::from_millis(16_500)));
}

#[test]
fn zero_period_every_pauses_and_reports() {
    let rt = Runtime::new();
    let period = rt.signal(Duration::ZERO);
    let count = rt.signal(0u32);
    let timer = rt.every_dyn(
        move |rt| period.get(rt),
        |_| Ok(true),
        move |rt| count.update(rt, |c| *c += 1),
    );
    let tick = rt.flush();
    assert_eq!(rt.next_deadline(), None, "no busy loop");
    assert_eq!(
        tick.diagnostics,
        vec![Diagnostic::ZeroPeriod { timer: timer.id() }]
    );
    assert!(rt.take_diagnostics().is_empty(), "drained into the tick");
    assert!(rt.flush().diagnostics.is_empty(), "reported once");
    rt.tick(10 * MS);
    assert_eq!(count.get(&rt), Ok(0));
    // Sub-millisecond periods are clamped.
    period.set(&rt, Duration::from_nanos(1)).unwrap();
    rt.flush();
    assert_eq!(rt.next_deadline(), Some(11 * MS));
}

#[test]
fn huge_durations_mean_never_and_do_not_panic() {
    let rt = Runtime::new();
    rt.tick(Duration::from_secs(5));
    let fired = rt.signal(false);
    rt.after(Duration::MAX, |_| Ok(true), move |rt| fired.set(rt, true));
    assert_eq!(rt.next_deadline(), None);
    let weak = rt.downgrade();
    let task = rt.spawn(async move {
        if let Some(rt) = weak.upgrade() {
            rt.sleep(Duration::MAX).await;
        }
        Ok(())
    });
    rt.flush();
    assert_eq!(rt.next_deadline(), None);
    rt.tick(Duration::from_secs(1_000_000));
    assert_eq!(fired.get(&rt), Ok(false));
    assert!(!task.is_finished(&rt));
}

#[test]
fn on_change_keyed_ignores_a_sink_switch() {
    // on change audio.sink.volume { shown = true }; switching to a sink with
    // another volume is a change of identity, not of volume.
    #[derive(Clone, PartialEq)]
    struct Sink {
        id: u32,
        volume: u32,
    }
    let rt = Runtime::new();
    let sink = rt.signal(Sink { id: 1, volume: 40 });
    let shown = rt.signal(0u32);
    rt.on_change_keyed(
        move |rt| Ok(sink.get(rt)?.id),
        move |rt| Ok(sink.get(rt)?.volume),
        move |rt, _| shown.update(rt, |n| *n += 1),
    );
    rt.flush();
    sink.set(&rt, Sink { id: 2, volume: 70 }).unwrap();
    rt.flush();
    assert_eq!(shown.get(&rt), Ok(0), "sink switch: no OSD");
    sink.set(&rt, Sink { id: 2, volume: 75 }).unwrap();
    rt.flush();
    assert_eq!(shown.get(&rt), Ok(1), "volume change: OSD");
    // The debounced form follows the same rule.
    let hidden = rt.signal(0u32);
    rt.on_change_after_keyed(
        move |rt| Ok(sink.get(rt)?.id),
        move |rt| Ok(sink.get(rt)?.volume),
        Duration::from_millis(1200),
        move |rt| hidden.update(rt, |n| *n += 1),
    );
    rt.flush();
    sink.set(&rt, Sink { id: 3, volume: 10 }).unwrap();
    rt.flush();
    assert_eq!(rt.next_deadline(), None, "no countdown on a switch");
    sink.set(&rt, Sink { id: 3, volume: 15 }).unwrap();
    rt.flush();
    rt.tick(rt.now() + Duration::from_millis(1200));
    assert_eq!(hidden.get(&rt), Ok(1));
}

#[test]
fn a_self_waking_task_does_not_spin_the_flush() {
    let rt = Runtime::new();
    let polls = Rc::new(RefCell::new(0u32));
    let p = polls.clone();
    rt.spawn(async move {
        // yield_now forever.
        std::future::poll_fn(|cx| {
            *p.borrow_mut() += 1;
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        })
        .await;
        Ok(())
    });
    rt.flush();
    assert_eq!(*polls.borrow(), 1, "polled once per flush");
    assert!(!rt.is_idle(), "it asked to run again");
    rt.flush();
    assert_eq!(*polls.borrow(), 2);
}

#[test]
fn listeners_reemitting_in_a_loop_are_a_cycle() {
    let rt = Runtime::new();
    let a = rt.events::<u32>();
    let b = rt.events::<u32>();
    rt.set_name(a.id(), "a");
    rt.set_name(b.id(), "b");
    let la = a.on(&rt, move |rt, n| b.emit(rt, n + 1)).unwrap();
    let lb = b.on(&rt, move |rt, n| a.emit(rt, n + 1)).unwrap();
    rt.set_name(la, "on_a");
    rt.set_name(lb, "on_b");
    a.emit(&rt, 0).unwrap();
    let tick = rt.flush();
    let cycles: Vec<String> = tick
        .errors
        .iter()
        .filter_map(|(_, e)| match e {
            Error::Cycle(p) => Some(p.to_string()),
            _ => None,
        })
        .collect();
    assert!(!cycles.is_empty(), "{:?}", tick.errors);
    assert!(
        cycles
            .iter()
            .all(|c| c == "a -> on_a -> b -> on_b -> a" || c == "b -> on_b -> a -> on_a -> b"),
        "{cycles:?}"
    );
    // Parked: nothing is lost, and the runtime is idle until the next emit.
    assert!(rt.is_idle());
    assert_eq!(a.queued(&rt).unwrap() + b.queued(&rt).unwrap(), 1);
    // A listener emitting to its own queue is caught the same way.
    let c = rt.events::<u32>();
    c.on(&rt, move |rt, n| c.emit(rt, *n)).unwrap();
    c.emit(&rt, 1).unwrap();
    let tick = rt.flush();
    assert!(
        tick.errors
            .iter()
            .any(|(_, e)| matches!(e, Error::Cycle(_)))
    );
}

/// Free slots so that new nodes get lower slot indices than older ones.
fn free_slots(rt: &Runtime, n: usize) {
    let cells: Vec<_> = (0..n).map(|_| rt.signal(0)).collect();
    for c in cells {
        c.dispose(rt);
    }
}

/// Handlers woken in one flush run in wake order, so the later handler's
/// write is the one that sticks (latest value wins), whatever slots the
/// tasks landed in.
#[test]
fn woken_tasks_run_in_wake_order_not_slot_order() {
    let rt = Runtime::new();
    let x = rt.signal(0);
    free_slots(&rt, 8);
    for v in [1, 2] {
        let weak = rt.downgrade();
        rt.spawn(async move {
            let rt = weak.upgrade().ok_or(Error::Cancelled)?;
            x.set(&rt, v)
        });
    }
    rt.flush();
    assert_eq!(x.get(&rt), Ok(2));
}

/// Timers due at the same instant fire in creation order.
#[test]
fn timers_due_together_fire_in_creation_order() {
    let rt = Runtime::new();
    let x = rt.signal(0);
    free_slots(&rt, 8);
    let _a = rt.after(10 * MS, |_| Ok(true), move |rt| x.set(rt, 1));
    let _b = rt.after(10 * MS, |_| Ok(true), move |rt| x.set(rt, 2));
    rt.tick(10 * MS);
    assert_eq!(x.get(&rt), Ok(2));
}

/// A task that woke itself during a flush waits for the next one; the wake
/// hook fires so a host sleeping on it comes back.
#[test]
fn a_task_left_ready_after_a_flush_calls_the_wake_hook() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let rt = Runtime::new();
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    rt.set_wake_hook(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let mut yielded = false;
    rt.spawn(async move {
        std::future::poll_fn(|cx| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        Ok(())
    });
    let before = woken.load(Ordering::SeqCst);
    rt.flush();
    assert!(!rt.is_idle());
    assert!(
        woken.load(Ordering::SeqCst) > before,
        "host told to flush again"
    );
    rt.flush();
    assert!(rt.is_idle());
}

/// `on change x, y` created before an effect that writes `x = y * 10`: one
/// outside write to `y` fires it once, with the settled values, never with
/// the intermediate `(0, 1)`.
#[test]
fn on_change_fires_once_per_outside_write_after_writers_settle() {
    let rt = Runtime::new();
    let x = rt.signal(0);
    let y = rt.signal(0);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    rt.on_change(
        move |rt| Ok((x.get(rt)?, y.get(rt)?)),
        move |_, &v| {
            s.borrow_mut().push(v);
            Ok(())
        },
    );
    rt.effect(move |rt| x.set(rt, y.get(rt)? * 10));
    rt.flush();
    assert!(seen.borrow().is_empty(), "boot is the baseline");
    y.set(&rt, 1).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty());
    assert_eq!(*seen.borrow(), vec![(10, 1)]);
    y.set(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![(10, 1), (20, 2)]);
}

/// A late `on change` handler that writes still triggers the effects that
/// read what it wrote, in the same flush.
#[test]
fn an_on_change_handler_write_reaches_effects_in_the_same_flush() {
    let rt = Runtime::new();
    let a = rt.signal(0);
    let b = rt.signal(0);
    let c = rt.signal(0);
    rt.effect(move |rt| c.set(rt, b.get(rt)? + 1));
    rt.on_change(move |rt| a.get(rt), move |rt, &v| b.set(rt, v * 2));
    rt.flush();
    a.set(&rt, 3).unwrap();
    rt.flush();
    assert_eq!(b.get(&rt), Ok(6));
    assert_eq!(c.get(&rt), Ok(7));
    assert!(rt.is_idle());
}

/// A future that parks its waker in `slot` until `done` is set.
struct Parked {
    slot: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::future::Future for Parked {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        if self.done.load(std::sync::atomic::Ordering::Acquire) {
            return Poll::Ready(());
        }
        *self.slot.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[test]
fn a_wake_from_another_thread_mid_flush_waits_for_the_next_flush() {
    // A reply arriving from an IO thread while sinks run must not write
    // behind a reader that already ran in this flush.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let rt = Runtime::new();
    let x = rt.signal(0);
    let reply = rt.signal(0);
    let slot = Arc::new(Mutex::new(None::<std::task::Waker>));
    let done = Arc::new(AtomicBool::new(false));
    let (s, d) = (slot.clone(), done.clone());
    rt.spawn(async move {
        Parked { slot: s, done: d }.await;
        Ok(())
    });
    let weak = rt.downgrade();
    let (s, d) = (slot.clone(), done.clone());
    rt.spawn(async move {
        Parked { slot: s, done: d }.await;
        if let Some(rt) = weak.upgrade() {
            reply.set(&rt, 7)?;
        }
        Ok(())
    });
    let runs = Rc::new(RefCell::new(Vec::new()));
    let r = runs.clone();
    // The reader runs first (creation order, both rank 0) ...
    rt.effect(move |rt| {
        r.borrow_mut().push((x.get(rt)?, reply.get(rt)?));
        Ok(())
    });
    // ... then this one completes the IO on another thread mid-flush.
    let (s, d) = (slot.clone(), done.clone());
    rt.effect(move |rt| {
        if x.get(rt)? == 1 {
            let (s, d) = (s.clone(), d.clone());
            std::thread::spawn(move || {
                d.store(true, Ordering::Release);
                if let Some(w) = s.lock().unwrap().take() {
                    w.wake();
                }
            })
            .join()
            .unwrap();
        }
        Ok(())
    });
    rt.flush();
    assert!(slot.lock().unwrap().is_some(), "the task is parked");
    runs.borrow_mut().clear();
    x.set(&rt, 1).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    assert_eq!(*runs.borrow(), vec![(1, 0)], "once in this flush");
    assert!(!rt.is_idle(), "the reply waits for the next flush");
    rt.flush();
    assert_eq!(*runs.borrow(), vec![(1, 0), (1, 7)]);
}

#[test]
fn a_sink_retriggered_without_a_feedback_path_stops_at_the_hard_cap() {
    // Each run of the reader wakes the next of 300 parked tasks, and each
    // task writes what the reader reads. A wake is not an edge the cycle
    // guard can follow, so only the hard cap stops it.
    use strand_core::HARD_RUNS_PER_FLUSH;
    let rt = Runtime::new();
    let n = rt.signal(0usize);
    let weak = rt.downgrade();
    for i in 0..300usize {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, d) = (slot.clone(), done.clone());
        let w = weak.clone();
        rt.spawn(async move {
            Parked { slot: s, done: d }.await;
            if let Some(rt) = w.upgrade() {
                n.set(&rt, i + 1)?;
            }
            Ok(())
        });
        // Keep the handles so the reader can release task `i`.
        HANDLES.with(|h| h.borrow_mut().push((slot, done)));
    }
    rt.flush();
    let runs = Rc::new(RefCell::new(0u32));
    let r = runs.clone();
    let reader = rt.effect(move |rt| {
        let i = n.get(rt)?;
        *r.borrow_mut() += 1;
        HANDLES.with(|h| {
            if let Some((slot, done)) = h.borrow().get(i) {
                done.store(true, std::sync::atomic::Ordering::Release);
                if let Some(w) = slot.lock().unwrap().take() {
                    w.wake();
                }
            }
        });
        Ok(())
    });
    let tick = rt.flush();
    assert_eq!(*runs.borrow(), HARD_RUNS_PER_FLUSH, "stopped at the cap");
    assert!(
        matches!(&tick.errors[..], [(id, Error::Cycle(_))] if *id == reader.id()),
        "{:?}",
        tick.errors
    );
    // The flush ended and the reader is parked: nothing is left to run
    // until something outside writes `n` again.
    assert!(rt.is_idle());
    assert!(rt.flush().errors.is_empty());
    assert_eq!(*runs.borrow(), HARD_RUNS_PER_FLUSH, "parked");
    HANDLES.with(|h| h.borrow_mut().clear());
}

/// A parked task's waker slot and its "done" flag.
type ParkedHandle = (
    std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
);

thread_local! {
    static HANDLES: RefCell<Vec<ParkedHandle>> = const { RefCell::new(Vec::new()) };
}

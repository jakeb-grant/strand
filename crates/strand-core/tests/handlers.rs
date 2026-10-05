//! Handlers: lossless events vs coalesced state, coroutines with
//! cancellation, `after`/`every` timers that pause, debounce, `Async<T>`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use strand_core::{Async, Diagnostic, Error, Runtime};

const MS: Duration = Duration::from_millis(1);

#[test]
fn events_are_lossless_while_state_coalesces() {
    let rt = Runtime::new();
    let state = rt.signal(0);
    let received = rt.events::<u32>();
    let state_seen = Rc::new(RefCell::new(Vec::new()));
    let events_seen = Rc::new(RefCell::new(Vec::new()));
    let s = state_seen.clone();
    rt.on_change(
        move |rt| state.get(rt),
        move |_, v| {
            s.borrow_mut().push(*v);
            Ok(())
        },
    );
    let e = events_seen.clone();
    received
        .on(&rt, move |_, n| {
            e.borrow_mut().push(*n);
            Ok(())
        })
        .unwrap();
    rt.flush();
    for i in 1..=5 {
        state.set(&rt, i).unwrap();
        received.emit(&rt, i as u32).unwrap();
    }
    assert_eq!(received.queued(&rt), Ok(5));
    rt.flush();
    assert_eq!(*state_seen.borrow(), vec![5], "state: latest value only");
    assert_eq!(
        *events_seen.borrow(),
        vec![1, 2, 3, 4, 5],
        "events: every one, in order"
    );
    assert_eq!(received.queued(&rt), Ok(0));
}

#[test]
fn events_emitted_by_listeners_are_delivered_in_the_same_flush() {
    let rt = Runtime::new();
    let a = rt.events::<u32>();
    let b = rt.events::<u32>();
    let log = Rc::new(RefCell::new(Vec::new()));
    a.on(&rt, move |rt, n| b.emit(rt, n * 10)).unwrap();
    let l = log.clone();
    b.on(&rt, move |_, n| {
        l.borrow_mut().push(*n);
        Ok(())
    })
    .unwrap();
    a.emit(&rt, 1).unwrap();
    a.emit(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*log.borrow(), vec![10, 20]);
    assert!(rt.is_idle());
}

#[test]
fn listener_disposed_with_its_scope() {
    let rt = Runtime::new();
    let q = rt.events::<u32>();
    let count = Rc::new(RefCell::new(0));
    let c = count.clone();
    let (scope, _) = rt.scope(|rt| {
        q.on(rt, move |_, _| {
            *c.borrow_mut() += 1;
            Ok(())
        })
    });
    q.emit(&rt, 1).unwrap();
    rt.flush();
    scope.dispose(&rt);
    q.emit(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*count.borrow(), 1);
}

#[test]
fn listener_errors_are_values() {
    let rt = Runtime::new();
    let q = rt.events::<u32>();
    let l = q.on(&rt, |_, _| Err(Error::failed("bad event"))).unwrap();
    q.emit(&rt, 1).unwrap();
    let tick = rt.flush();
    assert_eq!(tick.errors, vec![(l, Error::failed("bad event"))]);
}

#[test]
fn handler_coroutine_sleeps_on_the_logic_clock() {
    let rt = Runtime::new();
    let shown = rt.signal(false);
    let weak = rt.downgrade();
    let task = rt.spawn(async move {
        let rt = weak.upgrade().ok_or(Error::Cancelled)?;
        shown.set(&rt, true)?;
        rt.sleep(Duration::from_millis(1200)).await;
        shown.set(&rt, false)
    });
    assert_eq!(rt.next_deadline(), None);
    rt.flush();
    assert_eq!(shown.get(&rt), Ok(true));
    assert_eq!(rt.next_deadline(), Some(1200 * MS));
    rt.tick(1199 * MS);
    assert_eq!(shown.get(&rt), Ok(true));
    rt.tick(1200 * MS);
    assert_eq!(shown.get(&rt), Ok(false));
    assert!(task.is_finished(&rt));
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn unmount_cancels_a_handler_at_its_next_await() {
    let rt = Runtime::new();
    let reached = rt.signal(0);
    let weak = rt.downgrade();
    let (scope, task) = rt.scope(|rt| {
        rt.spawn(async move {
            let rt = weak.upgrade().ok_or(Error::Cancelled)?;
            reached.set(&rt, 1)?;
            rt.sleep(Duration::from_secs(1)).await;
            reached.set(&rt, 2)
        })
    });
    rt.flush();
    assert_eq!(reached.get(&rt), Ok(1));
    scope.dispose(&rt);
    assert!(task.is_finished(&rt));
    assert_eq!(
        rt.take_diagnostics(),
        vec![Diagnostic::Cancelled { task: task.id() }]
    );
    rt.tick(Duration::from_secs(2));
    assert_eq!(reached.get(&rt), Ok(1), "never resumed");
}

#[test]
fn handler_errors_are_values() {
    let rt = Runtime::new();
    let task = rt.spawn(async { Err(Error::failed("no such sink")) });
    let tick = rt.flush();
    assert_eq!(
        tick.errors,
        vec![(task.id(), Error::failed("no such sink"))]
    );
}

#[test]
fn foreign_wakers_schedule_a_flush() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Poll, Waker};

    let rt = Runtime::new();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let h = hook_calls.clone();
    rt.set_wake_hook(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });
    let slot: Rc<RefCell<(Option<Waker>, bool)>> = Rc::new(RefCell::new((None, false)));
    let s = slot.clone();
    let done = rt.signal(false);
    let weak = rt.downgrade();
    rt.spawn(async move {
        std::future::poll_fn(|cx| {
            let mut st = s.borrow_mut();
            if st.1 {
                Poll::Ready(())
            } else {
                st.0 = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        let rt = weak.upgrade().ok_or(Error::Cancelled)?;
        done.set(&rt, true)
    });
    rt.flush();
    let before = hook_calls.load(Ordering::SeqCst);
    assert!(rt.is_idle());
    // A service reply arrives on another thread.
    let waker = slot.borrow_mut().0.take().unwrap();
    slot.borrow_mut().1 = true;
    std::thread::spawn(move || waker.wake()).join().unwrap();
    assert_eq!(hook_calls.load(Ordering::SeqCst), before + 1);
    assert!(!rt.is_idle());
    rt.flush();
    assert_eq!(done.get(&rt), Ok(true));
}

#[test]
fn after_while_pauses_and_resumes() {
    // after 6s while !hover { n.expire() }
    let rt = Runtime::new();
    let hover = rt.signal(false);
    let expired = rt.signal(false);
    let timer = rt.after(
        Duration::from_secs(6),
        move |rt| Ok(!hover.get(rt)?),
        move |rt| expired.set(rt, true),
    );
    assert_eq!(rt.next_deadline(), Some(Duration::from_secs(6)));
    rt.tick(Duration::from_secs(4));
    hover.set(&rt, true).unwrap();
    rt.flush();
    assert_eq!(timer.is_running(&rt), Ok(false));
    assert_eq!(rt.next_deadline(), None, "paused: nothing scheduled");
    // Hover for a long time: no expiry.
    rt.tick(Duration::from_secs(60));
    assert_eq!(expired.get(&rt), Ok(false));
    hover.set(&rt, false).unwrap();
    rt.flush();
    // 4 s were counted; 2 s remain.
    assert_eq!(rt.next_deadline(), Some(Duration::from_secs(62)));
    rt.tick(Duration::from_secs(61));
    assert_eq!(expired.get(&rt), Ok(false));
    rt.tick(Duration::from_secs(62));
    assert_eq!(expired.get(&rt), Ok(true));
    assert_eq!(rt.next_deadline(), None, "after fires once");
}

#[test]
fn every_while_repeats_only_while_true() {
    let rt = Runtime::new();
    let visible = rt.signal(true);
    let ticks = rt.signal(0);
    rt.every(
        Duration::from_secs(1),
        move |rt| visible.get(rt),
        move |rt| ticks.update(rt, |n| *n += 1),
    );
    for s in 1..=3 {
        rt.tick(Duration::from_secs(s));
    }
    assert_eq!(ticks.get(&rt), Ok(3));
    visible.set(&rt, false).unwrap();
    rt.flush();
    assert_eq!(rt.next_deadline(), None);
    rt.tick(Duration::from_secs(100));
    assert_eq!(ticks.get(&rt), Ok(3));
    visible.set(&rt, true).unwrap();
    rt.flush();
    rt.tick(Duration::from_secs(101));
    assert_eq!(ticks.get(&rt), Ok(4));
    // A long gap fires once, not a burst.
    rt.tick(Duration::from_secs(200));
    assert_eq!(ticks.get(&rt), Ok(5));
}

#[test]
fn reactive_duration_and_rescale() {
    let rt = Runtime::new();
    let timeout = rt.signal(Duration::from_secs(10));
    let fired = rt.signal(false);
    let old = rt.after_dyn(
        move |rt| timeout.get(rt),
        |_| Ok(true),
        move |rt| fired.set(rt, true),
    );
    rt.tick(Duration::from_secs(5));
    assert_eq!(old.progress(&rt), Ok(0.5));
    // Reload changed `after 10s` to `after 20s`: half the period remains.
    let new = rt.after(Duration::from_secs(20), |_| Ok(true), |_| Ok(()));
    new.rescale_from(&rt, old).unwrap();
    old.dispose(&rt);
    assert_eq!(new.deadline(&rt), Ok(Some(Duration::from_secs(15))));
    // A reactive duration change keeps counted time.
    let t2 = rt.after_dyn(move |rt| timeout.get(rt), |_| Ok(true), |_| Ok(()));
    timeout.set(&rt, Duration::from_secs(3)).unwrap();
    rt.flush();
    assert_eq!(t2.deadline(&rt), Ok(Some(Duration::from_secs(8))));
}

#[test]
fn on_change_after_debounces() {
    // on change audio.sink.volume after 1.2s { shown = false }
    let rt = Runtime::new();
    let volume = rt.signal(50);
    let shown = rt.signal(true);
    rt.on_change_after(
        move |rt| volume.get(rt),
        Duration::from_millis(1200),
        move |rt| shown.set(rt, false),
    );
    rt.flush();
    assert_eq!(rt.next_deadline(), None, "never armed at boot");
    for (t, v) in [(100, 51), (600, 52), (1500, 53)] {
        volume.set(&rt, v).unwrap();
        rt.tick(t * MS);
    }
    assert_eq!(rt.next_deadline(), Some(2700 * MS));
    rt.tick(2699 * MS);
    assert_eq!(shown.get(&rt), Ok(true));
    rt.tick(2700 * MS);
    assert_eq!(shown.get(&rt), Ok(false));
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn timers_die_with_their_owner() {
    let rt = Runtime::new();
    let (scope, _) = rt.scope(|rt| rt.every(Duration::from_secs(1), |_| Ok(true), |_| Ok(())));
    assert!(rt.next_deadline().is_some());
    scope.dispose(&rt);
    assert_eq!(rt.next_deadline(), None);
}

#[test]
fn async_keeps_previous_value() {
    let mut hits: Async<Vec<&str>> = Async::empty();
    assert_eq!(hits.or(vec![]), Vec::<&str>::new());
    let r1 = hits.begin();
    assert!(hits.pending());
    assert!(hits.resolve(r1, Ok(vec!["firefox"])));
    let r2 = hits.begin();
    assert!(hits.pending());
    assert_eq!(hits.value(), Some(&vec!["firefox"]), "kept while loading");
    let r3 = hits.begin();
    // The slow r2 answers after r3 started: ignored.
    assert!(!hits.resolve(r2, Ok(vec!["stale"])));
    assert!(hits.pending());
    assert!(hits.resolve(r3, Err(Error::failed("index gone"))));
    assert!(!hits.pending());
    assert_eq!(hits.error(), Some(&Error::failed("index gone")));
    assert_eq!(hits.value(), Some(&vec!["firefox"]), "kept on error");
    let r4 = hits.begin();
    hits.resolve(r4, Ok(vec!["files"]));
    assert_eq!(hits.error(), None);
}

#[test]
fn async_load_runs_as_a_cancellable_handler() {
    let rt = Runtime::new();
    let hits = rt.signal(Async::<u32>::empty());
    let pending = rt.memo(move |rt| Ok(hits.get(rt)?.pending()));
    rt.watch(pending.id()).unwrap();
    let weak = rt.downgrade();
    hits.load(&rt, async move {
        if let Some(rt) = weak.upgrade() {
            rt.sleep(Duration::from_millis(30)).await;
        }
        Ok(42)
    })
    .unwrap();
    assert_eq!(pending.get(&rt), Ok(true));
    rt.flush();
    rt.tick(30 * MS);
    assert_eq!(hits.get(&rt).unwrap().value(), Some(&42));
    assert_eq!(pending.get(&rt), Ok(false));
    // Unmounting cancels an in-flight load; the value stays.
    let weak = rt.downgrade();
    let (scope, _) = rt.scope(|rt| {
        hits.load(rt, async move {
            if let Some(rt) = weak.upgrade() {
                rt.sleep(Duration::from_secs(5)).await;
            }
            Ok(7)
        })
    });
    rt.flush();
    assert!(rt.next_deadline().is_some());
    scope.dispose(&rt);
    assert_eq!(
        rt.next_deadline(),
        None,
        "a cancelled sleep schedules nothing"
    );
    rt.tick(Duration::from_secs(10));
    assert_eq!(hits.get(&rt).unwrap().value(), Some(&42));
}

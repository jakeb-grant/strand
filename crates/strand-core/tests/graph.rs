//! Push-pull graph: laziness, glitch freedom, equality cut-off, dynamic
//! dependencies, batching, effect order, watches, idle and cycles.

use std::cell::RefCell;
use std::rc::Rc;

use strand_core::{Error, MAX_RUNS_PER_FLUSH, Runtime};

#[test]
fn memo_is_lazy_and_cached() {
    let rt = Runtime::new();
    let a = rt.signal(1);
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    let double = rt.memo(move |rt| {
        *r.borrow_mut() += 1;
        Ok(a.get(rt)? * 2)
    });
    assert_eq!(*runs.borrow(), 0, "not computed until read");
    assert_eq!(double.get(&rt), Ok(2));
    assert_eq!(double.get(&rt), Ok(2));
    assert_eq!(*runs.borrow(), 1, "cached");
    a.set(&rt, 5).unwrap();
    assert_eq!(*runs.borrow(), 1, "a write does not compute");
    assert_eq!(double.get(&rt), Ok(10));
    assert_eq!(*runs.borrow(), 2);
}

#[test]
fn diamond_is_glitch_free() {
    // a -> b, a -> c, (b, c) -> d: d must never see new b with old c.
    let rt = Runtime::new();
    let a = rt.signal(1);
    let b = rt.memo(move |rt| Ok(a.get(rt)? + 1));
    let c = rt.memo(move |rt| Ok(a.get(rt)? * 10));
    let d = rt.memo(move |rt| Ok((b.get(rt)?, c.get(rt)?)));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    rt.effect(move |rt| {
        s.borrow_mut().push(d.get(rt)?);
        Ok(())
    });
    rt.flush();
    a.set(&rt, 2).unwrap();
    rt.flush();
    a.set(&rt, 3).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![(2, 10), (3, 20), (4, 30)]);
}

#[test]
fn equality_cut_off_stops_propagation() {
    let rt = Runtime::new();
    let a = rt.signal(3);
    let parity = rt.memo(move |rt| Ok(a.get(rt)? % 2));
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    let label = rt.memo(move |rt| {
        *r.borrow_mut() += 1;
        Ok(if parity.get(rt)? == 0 { "even" } else { "odd" })
    });
    let effect_runs = Rc::new(RefCell::new(0));
    let e = effect_runs.clone();
    rt.effect(move |rt| {
        label.get(rt)?;
        *e.borrow_mut() += 1;
        Ok(())
    });
    rt.flush();
    assert_eq!((*runs.borrow(), *effect_runs.borrow()), (1, 1));
    a.set(&rt, 5).unwrap(); // still odd
    let tick = rt.flush();
    assert_eq!(*runs.borrow(), 1, "parity unchanged: label not recomputed");
    assert_eq!(*effect_runs.borrow(), 1);
    assert_eq!(tick.effects_run, 0);
    a.set(&rt, 6).unwrap();
    rt.flush();
    assert_eq!((*runs.borrow(), *effect_runs.borrow()), (2, 2));
    // Writing an equal value is a no-op.
    a.set(&rt, 6).unwrap();
    assert!(rt.is_idle());
}

#[test]
fn dynamic_dependencies_switch() {
    let rt = Runtime::new();
    let use_a = rt.signal(true);
    let a = rt.signal(1);
    let b = rt.signal(100);
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    let pick = rt.memo(move |rt| {
        *r.borrow_mut() += 1;
        if use_a.get(rt)? { a.get(rt) } else { b.get(rt) }
    });
    assert_eq!(pick.get(&rt), Ok(1));
    b.set(&rt, 200).unwrap();
    assert_eq!(pick.get(&rt), Ok(1));
    assert_eq!(*runs.borrow(), 1, "b is not a dependency yet");
    use_a.set(&rt, false).unwrap();
    assert_eq!(pick.get(&rt), Ok(200));
    a.set(&rt, 7).unwrap();
    assert_eq!(pick.get(&rt), Ok(200));
    assert_eq!(*runs.borrow(), 2, "a was dropped as a dependency");
}

#[test]
fn writes_coalesce_and_effects_run_once_per_tick() {
    let rt = Runtime::new();
    let a = rt.signal(0);
    let b = rt.signal(0);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    rt.effect(move |rt| {
        s.borrow_mut().push((a.get(rt)?, b.get(rt)?));
        Ok(())
    });
    rt.flush();
    for i in 1..=10 {
        a.set(&rt, i).unwrap();
        b.set(&rt, -i).unwrap();
    }
    let tick = rt.flush();
    assert_eq!(*seen.borrow(), vec![(0, 0), (10, -10)]);
    assert_eq!(tick.effects_run, 1);
    assert_eq!(tick.written, {
        let mut w = vec![a.id(), b.id()];
        w.sort();
        w
    });
}

#[test]
fn effects_run_in_creation_order_and_see_writes_of_earlier_effects() {
    let rt = Runtime::new();
    let src = rt.signal(0);
    let derived_cell = rt.signal(0);
    let log = Rc::new(RefCell::new(Vec::new()));
    let l1 = log.clone();
    rt.effect(move |rt| {
        let v = src.get(rt)?;
        l1.borrow_mut().push(format!("first {v}"));
        derived_cell.set(rt, v * 2)
    });
    let l2 = log.clone();
    rt.effect(move |rt| {
        let v = derived_cell.get(rt)?;
        l2.borrow_mut().push(format!("second {v}"));
        Ok(())
    });
    rt.flush();
    log.borrow_mut().clear();
    src.set(&rt, 4).unwrap();
    rt.flush();
    assert_eq!(*log.borrow(), vec!["first 4", "second 8"]);
}

#[test]
fn watch_reports_changed_props_once() {
    let rt = Runtime::new();
    let a = rt.signal(1);
    let b = rt.signal(1);
    let sum = rt.memo(move |rt| Ok(a.get(rt)? + b.get(rt)?));
    let sign = rt.memo(move |rt| Ok(sum.get(rt)? > 0));
    rt.watch(sum.id()).unwrap();
    rt.watch(sign.id()).unwrap();
    rt.watch(a.id()).unwrap();
    assert!(rt.flush().changed.is_empty());
    a.set(&rt, 2).unwrap();
    b.set(&rt, 0).unwrap(); // sum 2 -> 2: cut off
    let tick = rt.flush();
    assert_eq!(tick.changed, vec![a.id()]);
    b.set(&rt, 5).unwrap();
    let tick = rt.flush();
    assert_eq!(tick.changed, vec![sum.id()]);
    a.set(&rt, -100).unwrap();
    let tick = rt.flush();
    assert_eq!(tick.changed, vec![a.id(), sum.id(), sign.id()]);
}

#[test]
fn idle_runtime_does_no_work() {
    let rt = Runtime::new();
    let a = rt.signal(1);
    let m = rt.memo(move |rt| Ok(a.get(rt)? + 1));
    rt.watch(m.id()).unwrap();
    rt.effect(move |rt| m.get(rt).map(|_| ()));
    rt.flush();
    let before = rt.stats();
    for _ in 0..100 {
        assert!(rt.is_idle());
        let tick = rt.flush();
        assert_eq!(tick.effects_run, 0);
        assert!(tick.changed.is_empty() && tick.written.is_empty());
    }
    let after = rt.stats();
    assert_eq!(before.computations, after.computations);
    assert_eq!(before.effect_runs, after.effect_runs);
    assert_eq!(rt.next_deadline(), None, "nothing scheduled: true idle");
}

#[test]
fn memo_cycle_is_an_error_naming_the_path() {
    let rt = Runtime::new();
    let flag = rt.signal(false);
    let cell: Rc<RefCell<Option<strand_core::Memo<i32>>>> = Rc::new(RefCell::new(None));
    let c2 = cell.clone();
    let a = rt.memo(move |rt| {
        if flag.get(rt)? {
            let b = c2.borrow().ok_or(Error::failed("unset"))?;
            b.get(rt)
        } else {
            Ok(1)
        }
    });
    let b = rt.memo(move |rt| Ok(a.get(rt)? + 1));
    *cell.borrow_mut() = Some(b);
    rt.set_name(a.id(), "a");
    rt.set_name(b.id(), "b");
    assert_eq!(b.get(&rt), Ok(2));
    flag.set(&rt, true).unwrap();
    match b.get(&rt) {
        Err(Error::Cycle(path)) => {
            assert_eq!(path.to_string(), "b -> a -> b");
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
    // Breaking the cycle recovers.
    flag.set(&rt, false).unwrap();
    assert_eq!(b.get(&rt), Ok(2));
}

#[test]
fn effect_feedback_loop_is_a_cycle_error() {
    let rt = Runtime::new();
    let x = rt.signal(0);
    let y = rt.signal(0);
    rt.set_name(x.id(), "x");
    rt.set_name(y.id(), "y");
    let e1 = rt.effect(move |rt| {
        let v = x.get(rt)?;
        y.set(rt, v + 1)
    });
    let e2 = rt.effect(move |rt| {
        let v = y.get(rt)?;
        x.set(rt, v + 1)
    });
    rt.set_name(e1.id(), "on_x");
    rt.set_name(e2.id(), "on_y");
    let tick = rt.flush();
    let cycle = tick
        .errors
        .iter()
        .find_map(|(_, e)| match e {
            Error::Cycle(p) => Some(p.to_string()),
            _ => None,
        })
        .expect("a cycle error");
    assert!(
        cycle == "on_x -> y -> on_y -> x -> on_x" || cycle == "on_y -> x -> on_x -> y -> on_y",
        "{cycle}"
    );
    // The flush stopped instead of spinning; the loop is bounded per tick.
    let v = x.get_untracked(&rt).unwrap();
    assert!(v <= 2 * MAX_RUNS_PER_FLUSH as i32 + 2, "{v}");
}

#[test]
fn write_inside_memo_is_an_error() {
    let rt = Runtime::new();
    let a = rt.signal(0);
    let b = rt.signal(0);
    let m = rt.memo(move |rt| {
        b.set(rt, 1)?;
        a.get(rt)
    });
    assert!(matches!(m.get(&rt), Err(Error::WriteInDerived { .. })));
    assert_eq!(b.get_untracked(&rt), Ok(0));
}

#[test]
fn effect_errors_are_values_in_the_tick() {
    let rt = Runtime::new();
    let e = rt.effect(|_| Err(Error::failed("boom")));
    let tick = rt.flush();
    assert_eq!(tick.errors, vec![(e.id(), Error::failed("boom"))]);
}

#[test]
fn on_change_skips_first_value() {
    let rt = Runtime::new();
    let vol = rt.signal(50);
    let shown = rt.signal(false);
    rt.on_change(move |rt| vol.get(rt), move |rt, _| shown.set(rt, true));
    rt.flush();
    assert_eq!(shown.get_untracked(&rt), Ok(false), "never fires at boot");
    vol.set(&rt, 55).unwrap();
    rt.flush();
    assert_eq!(shown.get_untracked(&rt), Ok(true));
}

#[test]
fn reentrant_flush_is_an_error_not_a_panic() {
    let rt = Runtime::new();
    let inner = Rc::new(RefCell::new(None));
    let i = inner.clone();
    rt.effect(move |rt| {
        *i.borrow_mut() = Some(rt.flush());
        Ok(())
    });
    rt.flush();
    let nested = inner.borrow_mut().take().unwrap();
    assert!(nested.errors.iter().any(|(_, e)| *e == Error::Reentrant));
}

#[test]
fn self_normalizing_effect_settles_without_a_cycle_error() {
    // on change level { level = min(level, 100) }
    let rt = Runtime::new();
    let level = rt.signal(50);
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    rt.effect(move |rt| {
        *r.borrow_mut() += 1;
        let v = level.get(rt)?;
        level.set(rt, v.min(100))
    });
    rt.flush();
    *runs.borrow_mut() = 0;
    level.set(&rt, 250).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    assert_eq!(level.get(&rt), Ok(100));
    assert_eq!(*runs.borrow(), 2, "clamps, then sees the clamped value");
}

#[test]
fn effects_created_in_reverse_order_still_settle_in_one_flush() {
    // Effect i copies cell i into cell i + 1, created last-to-first, so
    // every write re-triggers an effect that already ran this flush.
    let rt = Runtime::new();
    let cells: Vec<_> = (0..20).map(|_| rt.signal(0)).collect();
    for i in (0..19).rev() {
        let (from, to) = (cells[i], cells[i + 1]);
        rt.effect(move |rt| {
            let v = from.get(rt)?;
            to.set(rt, v)
        });
    }
    rt.flush();
    cells[0].set(&rt, 7).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    assert_eq!(cells[19].get(&rt), Ok(7));
}

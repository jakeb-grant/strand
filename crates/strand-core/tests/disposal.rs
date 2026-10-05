//! Ownership scopes, disposal and stale handles.

use std::cell::RefCell;
use std::rc::Rc;

use strand_core::{Error, NodeKind, Runtime};

#[test]
fn disposing_a_scope_drops_its_nodes_and_effects() {
    let rt = Runtime::new();
    let outside = rt.signal(0);
    let runs = Rc::new(RefCell::new(0));
    let cleaned = Rc::new(RefCell::new(false));
    let (scope, (inner, memo)) = rt.scope(|rt| {
        let inner = rt.signal(1);
        let memo = rt.memo(move |rt| Ok(outside.get(rt)? + inner.get(rt)?));
        let r = runs.clone();
        rt.effect(move |rt| {
            memo.get(rt)?;
            *r.borrow_mut() += 1;
            Ok(())
        });
        let c = cleaned.clone();
        rt.on_cleanup(move || *c.borrow_mut() = true);
        (inner, memo)
    });
    rt.flush();
    assert_eq!(*runs.borrow(), 1);
    let live = rt.stats().nodes;
    scope.dispose(&rt);
    assert!(*cleaned.borrow());
    assert_eq!(rt.stats().nodes, live - 4, "scope, signal, memo, effect");
    // The effect is gone: writes to the outside signal no longer run it.
    outside.set(&rt, 5).unwrap();
    rt.flush();
    assert_eq!(*runs.borrow(), 1);
    // Stale handles: errors, never panics.
    assert_eq!(inner.get(&rt), Err(Error::Disposed(inner.id())));
    assert_eq!(inner.set(&rt, 3), Err(Error::Disposed(inner.id())));
    assert_eq!(memo.get(&rt), Err(Error::Disposed(memo.id())));
    // The outside signal lost its observer edge.
    assert!(rt.is_idle());
}

#[test]
fn effect_children_are_disposed_before_rerun() {
    let rt = Runtime::new();
    let n = rt.signal(1);
    let created: Rc<RefCell<Vec<strand_core::Signal<i32>>>> = Rc::new(RefCell::new(Vec::new()));
    let cleanups = Rc::new(RefCell::new(0));
    let c = created.clone();
    let k = cleanups.clone();
    let effect = rt.effect(move |rt| {
        let v = n.get(rt)?;
        c.borrow_mut().push(rt.signal(v));
        let k = k.clone();
        rt.on_cleanup(move || *k.borrow_mut() += 1);
        Ok(())
    });
    rt.flush();
    n.set(&rt, 2).unwrap();
    rt.flush();
    let made = created.borrow().clone();
    assert_eq!(made.len(), 2);
    assert!(made[0].get(&rt).is_err(), "first run's child disposed");
    assert_eq!(made[1].get(&rt), Ok(2));
    assert_eq!(*cleanups.borrow(), 1);
    assert_eq!(rt.owner_of(made[1].id()), Ok(Some(effect.id())));
    effect.dispose(&rt);
    assert!(made[1].get(&rt).is_err());
    assert_eq!(*cleanups.borrow(), 2);
}

#[test]
fn nested_scopes_dispose_children_first() {
    let rt = Runtime::new();
    let order = Rc::new(RefCell::new(Vec::new()));
    let o = order.clone();
    let (outer, _) = rt.scope(|rt| {
        let o1 = o.clone();
        rt.on_cleanup(move || o1.borrow_mut().push("outer"));
        rt.scope(|rt| {
            let o2 = o.clone();
            rt.on_cleanup(move || o2.borrow_mut().push("inner"));
        });
    });
    outer.dispose(&rt);
    assert_eq!(*order.borrow(), vec!["inner", "outer"]);
    assert_eq!(rt.stats().nodes, 0);
}

#[test]
fn observers_of_disposed_nodes_rerun_and_see_the_error() {
    let rt = Runtime::new();
    let (scope, inner) = rt.scope(|rt| rt.signal(1));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    rt.effect(move |rt| {
        s.borrow_mut().push(inner.get(rt));
        Ok(())
    });
    rt.flush();
    scope.dispose(&rt);
    rt.flush();
    assert_eq!(
        *seen.borrow(),
        vec![Ok(1), Err(Error::Disposed(inner.id()))]
    );
}

#[test]
fn generational_ids_never_alias() {
    let rt = Runtime::new();
    let a = rt.signal(1);
    a.dispose(&rt);
    // The slot is reused by a new node, but the old handle stays stale.
    let b = rt.signal(2);
    assert_ne!(a.id(), b.id());
    assert_eq!(a.get(&rt), Err(Error::Disposed(a.id())));
    assert_eq!(b.get(&rt), Ok(2));
    assert_eq!(rt.kind(a.id()), Err(Error::Disposed(a.id())));
    assert_eq!(rt.kind(b.id()), Ok(NodeKind::Signal));
}

#[test]
fn stale_reads_of_every_handle_kind_are_errors() {
    let rt = Runtime::new();
    let (scope, (s, m, q, k)) = rt.scope(|rt| {
        let s = rt.signal(1);
        let m = rt.memo(move |rt| s.get(rt));
        let q = rt.events::<u32>();
        let k = rt.keyed(strand_core::KeyedVec::new(|v: &u32| *v));
        (s, m, q, k)
    });
    scope.dispose(&rt);
    use strand_core::KeyedSource;
    assert!(s.get(&rt).is_err());
    assert!(s.update(&rt, |v| *v += 1).is_err());
    assert!(m.get(&rt).is_err());
    assert!(q.emit(&rt, 1).is_err());
    assert!(q.on(&rt, |_, _| Ok(())).is_err());
    assert!(k.push(&rt, 1).is_err());
    assert!(k.snapshot(&rt).is_err());
    assert!(rt.watch(s.id()).is_err());
    assert!(scope.run(&rt, |_| ()).is_err());
    // Disposing twice is harmless.
    scope.dispose(&rt);
    s.dispose(&rt);
}

#[test]
fn shutdown_disposes_everything() {
    let rt = Runtime::new();
    let ran = Rc::new(RefCell::new(false));
    let r = ran.clone();
    rt.on_cleanup(move || *r.borrow_mut() = true);
    let a = rt.signal(1);
    rt.scope(|rt| rt.memo(move |rt| a.get(rt)));
    rt.spawn(std::future::pending());
    rt.shutdown();
    assert!(*ran.borrow());
    assert_eq!(rt.stats().nodes, 0);
}

// ----- nodes that own and read a child (components, `if` branches) -------

#[test]
fn an_effect_that_creates_and_reads_a_child_runs_once_per_tick() {
    let rt = Runtime::new();
    let n = rt.signal(1);
    let runs = Rc::new(RefCell::new(0));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (r, s) = (runs.clone(), seen.clone());
    rt.effect(move |rt| {
        let v = n.get(rt)?;
        let local = rt.memo(move |_| Ok(v * 2));
        let cell = rt.signal(v + 1);
        s.borrow_mut().push(local.get(rt)? + cell.get(rt)?);
        *r.borrow_mut() += 1;
        Ok(())
    });
    let t = rt.flush();
    assert!(t.errors.is_empty(), "{:?}", t.errors);
    for i in 2..12 {
        n.set(&rt, i).unwrap();
        let t = rt.flush();
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        assert_eq!(t.effects_run, 1);
        assert!(rt.is_idle());
    }
    assert_eq!(*runs.borrow(), 11);
    assert_eq!(seen.borrow().last(), Some(&(11 * 2 + 12)));
}

#[test]
fn a_memo_that_creates_and_reads_a_child_caches() {
    let rt = Runtime::new();
    let n = rt.signal(1);
    let outer = rt.memo(move |rt| {
        let v = n.get(rt)?;
        let inner = rt.memo(move |_| Ok(v * 10));
        inner.get(rt)
    });
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    rt.effect(move |rt| {
        outer.get(rt)?;
        *r.borrow_mut() += 1;
        Ok(())
    });
    rt.flush();
    let base = rt.stats().computations;
    assert_eq!(outer.get(&rt), Ok(10));
    assert_eq!(outer.get(&rt), Ok(10));
    assert_eq!(rt.stats().computations, base, "cached: no recompute");
    for i in 2..12 {
        n.set(&rt, i).unwrap();
        let t = rt.flush();
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        assert_eq!(t.effects_run, 1);
        assert!(rt.is_idle());
        let before = rt.stats().computations;
        assert_eq!(outer.get(&rt), Ok(i * 10));
        assert_eq!(rt.stats().computations, before);
    }
    assert_eq!(*runs.borrow(), 11);
}

#[test]
fn a_keyed_derivation_whose_params_own_and_read_a_child_settles() {
    use strand_core::{KeyedOps, KeyedSource, KeyedVec};
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|v: &u32| *v));
    for i in 0..10 {
        xs.push(&rt, i).unwrap();
    }
    let limit = rt.signal(3u32);
    let small = xs.filter_with(
        &rt,
        move |rt| {
            let l = limit.get(rt)?;
            let local = rt.memo(move |_| Ok(l));
            local.get(rt)
        },
        |l, v| v < l,
    );
    let runs = Rc::new(RefCell::new(0));
    let r = runs.clone();
    rt.effect(move |rt| {
        small.snapshot(rt)?;
        *r.borrow_mut() += 1;
        Ok(())
    });
    rt.flush();
    for l in 4..10 {
        limit.set(&rt, l).unwrap();
        let t = rt.flush();
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        assert_eq!(t.effects_run, 1);
        assert!(rt.is_idle());
        assert_eq!(small.snapshot(&rt).unwrap().len(), l as usize);
    }
    assert_eq!(*runs.borrow(), 7);
}

// ----- reparenting (live reload keeps identity) ---------------------------

#[test]
fn a_moved_scope_survives_its_old_parent() {
    use strand_core::{KeyedSource, KeyedVec};
    let rt = Runtime::new();
    let (start, (component, (count, pins))) = rt.scope(|rt| {
        rt.scope(|rt| {
            let pins = rt.keyed(KeyedVec::new(|v: &u32| *v));
            (rt.signal(1), pins)
        })
    });
    assert_eq!(rt.owner_of(component.id()), Ok(Some(start.id())));
    pins.push(&rt, 7).unwrap();
    pins.push(&rt, 8).unwrap();
    let before = pins.snapshot(&rt).unwrap();
    count.set(&rt, 5).unwrap();
    // The component moves from `start` to `end`.
    let (end, _) = rt.scope(|_| ());
    rt.reparent(component.id(), Some(end.id())).unwrap();
    start.dispose(&rt);
    assert_eq!(count.get(&rt), Ok(5), "state kept");
    let after = pins.snapshot(&rt).unwrap();
    assert!(
        after.same_source(&before),
        "same diff log: items keep identity"
    );
    assert_eq!(after.diffs_since(before.version()), Some(vec![]));
    assert_eq!(rt.owner_of(component.id()), Ok(Some(end.id())));
    end.dispose(&rt);
    assert_eq!(count.get(&rt), Err(Error::Disposed(count.id())));
}

#[test]
fn reparent_rejects_ownership_cycles_and_ignores_stale_ids() {
    let rt = Runtime::new();
    let (outer, inner) = rt.scope(|rt| rt.scope(|_| ()).0);
    match rt.reparent(outer.id(), Some(inner.id())) {
        Err(Error::Cycle(path)) => {
            assert_eq!(path.nodes.first(), Some(&outer.id()));
            assert_eq!(path.nodes.last(), Some(&outer.id()));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        rt.reparent(outer.id(), Some(outer.id())),
        Err(Error::Cycle(_))
    ));
    // A move to the root and back.
    rt.reparent(inner.id(), None).unwrap();
    assert!(rt.root_owned().contains(&inner.id()));
    outer.dispose(&rt);
    assert!(rt.exists(inner.id()));
    rt.reparent(inner.id(), Some(inner.id())).unwrap_err();
    inner.dispose(&rt);
    assert_eq!(rt.reparent(inner.id(), None), Ok(()), "no-op when stale");
}

#[test]
fn a_moved_subtree_runs_after_its_new_owner() {
    let rt = Runtime::new();
    let x = rt.signal(0);
    let order = Rc::new(RefCell::new(Vec::new()));
    let o = order.clone();
    let (moved, _) = rt.scope(|rt| {
        rt.effect(move |rt| {
            x.get(rt)?;
            o.borrow_mut().push("moved");
            Ok(())
        })
    });
    let o = order.clone();
    let (target, _) = rt.scope(|rt| {
        rt.effect(move |rt| {
            x.get(rt)?;
            o.borrow_mut().push("new owner");
            Ok(())
        })
    });
    rt.flush();
    rt.reparent(moved.id(), Some(target.id())).unwrap();
    order.borrow_mut().clear();
    x.set(&rt, 1).unwrap();
    rt.flush();
    assert_eq!(*order.borrow(), vec!["new owner", "moved"]);
}

// ----- suspension (a faulted component freezes, state kept) ---------------

#[test]
fn a_suspended_component_freezes_and_resumes() {
    use std::time::Duration;
    let rt = Runtime::new();
    let x = rt.signal(0);
    let seen = rt.signal(0);
    let fired = rt.signal(0);
    let resumed = rt.signal(0);
    let clicks = rt.events::<()>();
    let clicked = rt.signal(0);
    let weak = rt.downgrade();
    let (component, state) = rt.scope(|rt| {
        let state = rt.signal(42);
        rt.effect(move |rt| seen.set(rt, x.get(rt)?));
        rt.after(
            Duration::from_millis(10),
            |_| Ok(true),
            move |rt| fired.set(rt, 1),
        );
        rt.spawn(async move {
            let rt = weak.upgrade().ok_or(Error::Cancelled)?;
            rt.sleep(Duration::from_millis(5)).await;
            resumed.set(&rt, 1)
        });
        clicks
            .on(rt, move |rt, _| clicked.update(rt, |c| *c += 1))
            .unwrap();
        state
    });
    rt.flush();
    rt.suspend(component.id()).unwrap();
    assert!(rt.is_suspended(component.id()));
    x.set(&rt, 1).unwrap();
    clicks.emit(&rt, ()).unwrap();
    rt.tick(Duration::from_millis(20));
    assert_eq!(seen.get(&rt), Ok(0), "effect frozen");
    assert_eq!(fired.get(&rt), Ok(0), "timer frozen");
    assert_eq!(resumed.get(&rt), Ok(0), "task frozen");
    assert_eq!(clicked.get(&rt), Ok(0), "input ignored");
    assert_eq!(state.get(&rt), Ok(42), "state kept");
    assert!(rt.is_idle(), "a frozen subtree is idle");
    assert_eq!(rt.next_deadline(), None, "and schedules nothing");
    // The fixing reload resumes it.
    rt.resume(component.id());
    rt.tick(Duration::from_millis(30));
    assert_eq!(seen.get(&rt), Ok(1));
    assert_eq!(fired.get(&rt), Ok(1));
    assert_eq!(resumed.get(&rt), Ok(1));
    clicks.emit(&rt, ()).unwrap();
    rt.flush();
    assert_eq!(clicked.get(&rt), Ok(1));
}

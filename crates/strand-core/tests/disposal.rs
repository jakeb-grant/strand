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

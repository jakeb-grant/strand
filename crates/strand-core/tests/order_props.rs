//! Property tests for the flush order: effects run in a computed
//! topological order, once per flush, and see final values.
//!
//! Random graphs of primary signals, memos and *handler-written cells* (an
//! effect reads earlier nodes and writes the cell), with reader effects and
//! `on change` handlers on top, all effects created in a random order (so
//! creation order is not topological). After the first flush has learned
//! the write edges:
//!
//! * every effect runs at most once per flush, unless its rank rose during
//!   that flush (a dependency seen for the first time: a `Pick` switching
//!   branches); with static dependencies ranks never rise again, so it is
//!   exactly once;
//! * every reader that runs sees the flush's final values (glitch-free at
//!   the edge), and every reader whose inputs changed ran (never deaf);
//! * `on change` fires at most once per flush, with the final value,
//!   exactly when the value differs from the previous flush's.
//!
//! Declared write edges (`rt.writes_to`) give the same order before any
//! write has been seen.

use std::cell::RefCell;
use std::rc::Rc;

use proptest::prelude::*;
use strand_core::{Effect, Memo, NodeId, Runtime, Signal};

#[derive(Clone, Debug)]
enum Spec {
    /// A primary signal, written by the test.
    Primary,
    /// Sum of earlier nodes mod 7.
    Sum(Vec<usize>),
    /// `if x even { y } else { z }`: dependencies change at run time.
    Pick(usize, usize, usize),
    /// A cell written by an effect: `(sum of inputs + 1) mod 7`.
    Written(Vec<usize>),
}

#[derive(Clone, Debug)]
enum Fx {
    /// Reads nodes and checks them.
    Reader(Vec<usize>),
    /// `on change` of one node.
    OnChange(usize),
    /// The writer of the `Written` cell at this index.
    Writer(usize),
}

#[derive(Clone, Debug)]
struct Case {
    specs: Vec<Spec>,
    /// Effects in creation order (a random permutation).
    effects: Vec<Fx>,
    /// Declare every write edge before the first flush.
    declare: bool,
    /// Ticks: `(primary, value)` writes.
    ticks: Vec<Vec<(usize, i64)>>,
}

impl Case {
    fn dynamic(&self) -> bool {
        self.specs.iter().any(|s| matches!(s, Spec::Pick(..)))
    }
}

fn naive(specs: &[Spec], primary: &[i64]) -> Vec<i64> {
    let mut v: Vec<i64> = Vec::with_capacity(specs.len());
    for (i, s) in specs.iter().enumerate() {
        let x = match s {
            Spec::Primary => primary[i],
            Spec::Sum(ins) => ins.iter().map(|&j| v[j]).sum::<i64>().rem_euclid(7),
            Spec::Pick(c, a, b) => {
                if v[*c].rem_euclid(2) == 0 {
                    v[*a]
                } else {
                    v[*b]
                }
            }
            Spec::Written(ins) => (ins.iter().map(|&j| v[j]).sum::<i64>() + 1).rem_euclid(7),
        };
        v.push(x);
    }
    v
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(
            (0u8..10, prop::collection::vec(any::<usize>(), 1..4)),
            2..24,
        ),
        prop::collection::vec((0u8..3, prop::collection::vec(any::<usize>(), 1..4)), 1..8),
        any::<bool>(),
        any::<bool>(),
        any::<u64>(),
        prop::collection::vec(
            prop::collection::vec((any::<usize>(), -3i64..9), 0..4),
            1..20,
        ),
    )
        .prop_map(|(raw, raw_fx, dynamic, declare, shuffle, raw_ticks)| {
            let mut specs = vec![Spec::Primary];
            for (kind, idx) in raw {
                let avail = specs.len();
                let pick = |k: usize| idx[k % idx.len()] % avail;
                specs.push(match kind {
                    0 | 1 => Spec::Primary,
                    2 | 3 => Spec::Sum(idx.iter().map(|&j| j % avail).collect()),
                    4 if dynamic => Spec::Pick(pick(0), pick(1), pick(2)),
                    _ => Spec::Written(idx.iter().map(|&j| j % avail).collect()),
                });
            }
            let n = specs.len();
            let mut effects: Vec<Fx> = specs
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s, Spec::Written(_)))
                .map(|(i, _)| Fx::Writer(i))
                .collect();
            for (kind, idx) in raw_fx {
                effects.push(match kind {
                    0 | 1 => Fx::Reader(idx.iter().map(|&j| j % n).collect()),
                    _ => Fx::OnChange(idx[0] % n),
                });
            }
            // A deterministic shuffle: creation order is not topological.
            let len = effects.len();
            for i in (1..len).rev() {
                let j = ((shuffle >> (i % 50)) as usize ^ i.wrapping_mul(2_654_435_761)) % (i + 1);
                effects.swap(i, j);
            }
            let primaries: Vec<usize> = specs
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s, Spec::Primary))
                .map(|(i, _)| i)
                .collect();
            let ticks = raw_ticks
                .into_iter()
                .map(|ws| {
                    ws.into_iter()
                        .map(|(p, v)| (primaries[p % primaries.len()], v))
                        .collect()
                })
                .collect();
            Case {
                specs,
                effects,
                declare,
                ticks,
            }
        })
}

#[derive(Copy, Clone)]
enum H {
    S(Signal<i64>),
    M(Memo<i64>),
}

impl H {
    fn get(self, rt: &Runtime) -> Result<i64, strand_core::Error> {
        match self {
            H::S(s) => s.get(rt),
            H::M(m) => m.get(rt),
        }
    }
    fn id(self) -> NodeId {
        match self {
            H::S(s) => s.id(),
            H::M(m) => m.id(),
        }
    }
}

/// Per effect, per flush: runs and what each run saw.
#[derive(Default)]
struct Log {
    runs: Vec<usize>,
    seen: Vec<Vec<Vec<i64>>>,
    fired: Vec<Vec<i64>>,
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let rt = Runtime::new();
    let n = case.specs.len();
    let mut primary = vec![0i64; n];
    let handles: Rc<RefCell<Vec<H>>> = Rc::new(RefCell::new(Vec::new()));
    for spec in &case.specs {
        let h = match spec {
            Spec::Primary | Spec::Written(_) => H::S(rt.signal(0)),
            Spec::Sum(ins) => {
                let hs = handles.clone();
                let ins = ins.clone();
                H::M(rt.memo(move |rt| {
                    let hs = hs.borrow().clone();
                    let mut s = 0;
                    for &j in &ins {
                        s += hs[j].get(rt)?;
                    }
                    Ok(s.rem_euclid(7))
                }))
            }
            Spec::Pick(c, a, b) => {
                let hs = handles.clone();
                let (c, a, b) = (*c, *a, *b);
                H::M(rt.memo(move |rt| {
                    let hs = hs.borrow().clone();
                    if hs[c].get(rt)?.rem_euclid(2) == 0 {
                        hs[a].get(rt)
                    } else {
                        hs[b].get(rt)
                    }
                }))
            }
        };
        handles.borrow_mut().push(h);
    }
    // Written cells start at their settled value for all-zero primaries, so
    // the warm-up flush only learns.
    let start = naive(&case.specs, &primary);
    for (i, s) in case.specs.iter().enumerate() {
        if let (Spec::Written(_), H::S(sig)) = (s, handles.borrow()[i]) {
            sig.set(&rt, start[i]).unwrap();
        }
    }
    let log = Rc::new(RefCell::new(Log::default()));
    let mut ids: Vec<Effect> = Vec::new();
    for (e, fx) in case.effects.iter().enumerate() {
        log.borrow_mut().runs.push(0);
        log.borrow_mut().seen.push(Vec::new());
        log.borrow_mut().fired.push(Vec::new());
        let lg = log.clone();
        let hs = handles.borrow().clone();
        let effect = match fx {
            Fx::Reader(ins) => {
                let ins = ins.clone();
                rt.effect(move |rt| {
                    let mut seen = Vec::new();
                    for &j in &ins {
                        seen.push(hs[j].get(rt)?);
                    }
                    let mut l = lg.borrow_mut();
                    l.runs[e] += 1;
                    l.seen[e].push(seen);
                    Ok(())
                })
            }
            Fx::Writer(i) => {
                let Spec::Written(ins) = &case.specs[*i] else {
                    unreachable!()
                };
                let ins = ins.clone();
                let H::S(cell) = hs[*i] else { unreachable!() };
                let w = rt.effect(move |rt| {
                    let mut s = 0;
                    for &j in &ins {
                        s += hs[j].get(rt)?;
                    }
                    lg.borrow_mut().runs[e] += 1;
                    cell.set(rt, (s + 1).rem_euclid(7))
                });
                if case.declare {
                    rt.writes_to(w.id(), cell.id()).unwrap();
                }
                w
            }
            Fx::OnChange(j) => {
                let j = *j;
                rt.on_change(
                    move |rt| hs[j].get(rt),
                    move |_, v| {
                        let mut l = lg.borrow_mut();
                        l.runs[e] += 1;
                        l.fired[e].push(*v);
                        Ok(())
                    },
                )
            }
        };
        ids.push(effect);
    }
    // Warm-up: first runs, learning write edges.
    let tick = rt.flush();
    prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    let mut prev = naive(&case.specs, &primary);
    for ticks in &case.ticks {
        {
            let mut l = log.borrow_mut();
            for e in 0..case.effects.len() {
                l.runs[e] = 0;
                l.seen[e].clear();
                l.fired[e].clear();
            }
        }
        let ranks_before: Vec<u32> = ids.iter().map(|e| rt.rank(e.id())).collect();
        for &(p, v) in ticks {
            primary[p] = v;
            let H::S(s) = handles.borrow()[p] else {
                unreachable!()
            };
            s.set(&rt, v).unwrap();
        }
        let tick = rt.flush();
        prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        let fin = naive(&case.specs, &primary);
        // The graph agrees with the naive model.
        for (i, h) in handles.borrow().iter().enumerate() {
            prop_assert_eq!(h.get(&rt).unwrap(), fin[i], "node {} ({:?})", i, h.id());
        }
        let l = log.borrow();
        for (e, fx) in case.effects.iter().enumerate() {
            let rose = rt.rank(ids[e].id()) > ranks_before[e];
            prop_assert!(
                !rose || case.dynamic(),
                "a static graph's ranks are settled"
            );
            prop_assert!(
                l.runs[e] <= 1 || rose,
                "effect {} ({:?}) ran {} times in one flush",
                e,
                fx,
                l.runs[e]
            );
            match fx {
                Fx::Reader(ins) => {
                    let want: Vec<i64> = ins.iter().map(|&j| fin[j]).collect();
                    if !rose {
                        // Glitch-free at the edge: every run saw final values.
                        for seen in &l.seen[e] {
                            prop_assert_eq!(seen, &want, "reader {} saw a glitch", e);
                        }
                    }
                    let before: Vec<i64> = ins.iter().map(|&j| prev[j]).collect();
                    if before != want {
                        prop_assert!(l.runs[e] >= 1, "reader {} went deaf", e);
                    }
                }
                Fx::OnChange(j) => {
                    let want: Vec<i64> = if fin[*j] != prev[*j] {
                        vec![fin[*j]]
                    } else {
                        vec![]
                    };
                    prop_assert_eq!(&l.fired[e], &want, "on change {} of node {}", e, j);
                }
                Fx::Writer(_) => {}
            }
        }
        prev = fin;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn effects_run_once_in_topological_order(case in case_strategy()) {
        check(&case)?;
    }
}

#[test]
fn a_declared_writer_orders_its_readers_from_the_first_flush() {
    // reader (created first) -> cell <- writer (created second).
    let rt = Runtime::new();
    let x = rt.signal(1);
    let cell = rt.signal(0);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let reader = rt.effect(move |rt| {
        s.borrow_mut().push(cell.get(rt)?);
        Ok(())
    });
    let writer = rt.effect(move |rt| cell.set(rt, x.get(rt)? * 10));
    rt.writes_to(writer.id(), cell.id()).unwrap();
    assert!(rt.rank(reader.id()) == 0, "no read edge yet");
    rt.flush();
    // The first flush runs the reader before its read edge exists.
    assert_eq!(*seen.borrow(), vec![0, 10]);
    assert!(rt.rank(reader.id()) > rt.rank(writer.id()));
    seen.borrow_mut().clear();
    x.set(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![20], "once, after its writer");
}

#[test]
fn a_feedback_edge_is_not_ranked() {
    let rt = Runtime::new();
    let x = rt.signal(0i64);
    // Normalises `x` to even: it reads what it writes.
    let norm = rt.effect(move |rt| {
        let v = x.get(rt)?;
        x.set(rt, v + v.rem_euclid(2))
    });
    rt.flush();
    let err = rt.writes_to(norm.id(), x.id());
    assert!(matches!(err, Err(strand_core::Error::Cycle(_))), "{err:?}");
    x.set(&rt, 3).unwrap();
    assert!(rt.flush().errors.is_empty());
    assert_eq!(x.get(&rt), Ok(4));
}

#[test]
fn an_owned_effect_runs_after_its_owner() {
    // The owner reads a handler-written cell, so it ranks above zero; what
    // it creates inherits its rank and still runs after it.
    let rt = Runtime::new();
    let x = rt.signal(0);
    let cell = rt.signal(0);
    let order = Rc::new(RefCell::new(Vec::new()));
    let o = order.clone();
    rt.effect(move |rt| {
        let v = cell.get(rt)?;
        o.borrow_mut().push(format!("owner {v}"));
        let o2 = o.clone();
        rt.effect(move |rt| {
            o2.borrow_mut().push(format!("child {}", cell.get(rt)?));
            Ok(())
        });
        Ok(())
    });
    rt.effect(move |rt| cell.set(rt, x.get(rt)?));
    rt.flush();
    order.borrow_mut().clear();
    x.set(&rt, 1).unwrap();
    rt.flush();
    assert_eq!(*order.borrow(), vec!["owner 1", "child 1"]);
}

#[test]
fn a_read_edge_closing_a_loop_unranks_the_write_edge() {
    let rt = Runtime::new();
    let sel = rt.signal(false);
    let a = rt.signal(1i64);
    let c = rt.signal(0i64);
    let m = rt.memo(move |rt| if sel.get(rt)? { c.get(rt) } else { Ok(0) });
    let w = rt.effect(move |rt| {
        let v = a.get(rt)? + m.get(rt)?;
        c.set(rt, v.min(10))
    });
    rt.flush();
    assert!(rt.rank(c.id()) > rt.rank(w.id()), "w -> c learned");
    // Now `w` reads `c` through `m`: the write edge is feedback.
    sel.set(&rt, true).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    assert_eq!(c.get(&rt), Ok(10), "the loop settles at its cut-off");
    assert!(matches!(
        rt.writes_to(w.id(), c.id()),
        Err(strand_core::Error::Cycle(_))
    ));
    // Later writes still settle, with no rank walk going round the loop.
    a.set(&rt, 2).unwrap();
    assert!(rt.flush().errors.is_empty());
    assert_eq!(c.get(&rt), Ok(10));
}

#[test]
fn a_listener_written_cell_is_read_after_the_event_that_fills_it() {
    // reader (created first) reads `count`; a listener writes `count` for
    // every event an effect emits.
    let rt = Runtime::new();
    let x = rt.signal(0);
    let count = rt.signal(0);
    let reads = Rc::new(RefCell::new(Vec::new()));
    let r = reads.clone();
    rt.effect(move |rt| {
        r.borrow_mut().push(count.get(rt)?);
        Ok(())
    });
    let pings = rt.events::<i32>();
    pings
        .on(&rt, move |rt, &v| count.update(rt, |c| *c += v))
        .unwrap();
    rt.effect(move |rt| pings.emit(rt, x.get(rt)?));
    rt.flush();
    reads.borrow_mut().clear();
    for v in 1..4 {
        x.set(&rt, v).unwrap();
        rt.flush();
    }
    // Each flush: the effect emits, the listener writes, the reader runs
    // once with the new total.
    assert_eq!(*reads.borrow(), vec![1, 3, 6]);
}

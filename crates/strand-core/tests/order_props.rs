//! Property tests for the flush order: effects run in a computed
//! topological order, once per flush, and see final values.
//!
//! Random graphs of primary signals, memos, *handler-written cells* (an
//! effect reads earlier nodes and writes the cell) and async memos (`let
//! hits = svc.call(input)`: an internal effect starts a load that a task
//! resolves, here at once), with reader effects and
//! `on change` handlers on top, all effects created in a random order (so
//! creation order is not topological).
//!
//! With every edge declared (`rt.reads_from` with the syntactic read set,
//! every `Pick` branch included, and `rt.writes_to` for every write, as the
//! compiler emits them), from the very first flush on, written cells
//! starting unsettled:
//!
//! * every effect runs at most once per flush, and exactly once in the
//!   first; no rank ever rises;
//! * every reader that runs sees the flush's final values (glitch-free at
//!   the edge), and every reader whose inputs changed ran (never deaf);
//! * `on change` fires at most once per flush, with the final value,
//!   exactly when the value differs from the previous flush's (never in
//!   the first flush).
//!
//! Without declarations (edges learned as they are seen), after a warm-up
//! flush the same holds, except that a reader whose rank rose in a flush
//! (a `Pick` switching to a higher-ranked branch) may run more than once
//! and see an intermediate value; its last run still sees the final
//! values.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use proptest::prelude::*;
use strand_core::{AsyncMemo, Effect, Memo, NodeId, Runtime, Signal};

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
    /// An async memo over the sum of its inputs, resolving at once to
    /// `(sum + 2) mod 7`; read as its value (`-1` before the first).
    Fetched(Vec<usize>),
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
    /// Declare every read and write edge before the first flush.
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
            Spec::Fetched(ins) => (ins.iter().map(|&j| v[j]).sum::<i64>() + 2).rem_euclid(7),
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
                    8 => Spec::Fetched(idx.iter().map(|&j| j % avail).collect()),
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
    A(AsyncMemo<i64>),
}

impl H {
    fn get(self, rt: &Runtime) -> Result<i64, strand_core::Error> {
        match self {
            H::S(s) => s.get(rt),
            H::M(m) => m.get(rt),
            H::A(a) => Ok(a.get(rt)?.value().copied().unwrap_or(-1)),
        }
    }
    fn id(self) -> NodeId {
        match self {
            H::S(s) => s.id(),
            H::M(m) => m.id(),
            H::A(a) => a.id(),
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
    // The syntactic read set of a node or effect (every branch).
    let declare_reads = |id: NodeId, ins: &[usize], hs: &[H]| {
        if case.declare {
            let ids: Vec<NodeId> = ins.iter().map(|&j| hs[j].id()).collect();
            rt.reads_from(id, &ids).unwrap();
        }
    };
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
            Spec::Fetched(ins) => {
                let hs = handles.clone();
                let ins = ins.clone();
                H::A(rt.async_memo(
                    move |rt| {
                        let hs = hs.borrow().clone();
                        let mut s = 0;
                        for &j in &ins {
                            s += hs[j].get(rt)?;
                        }
                        Ok(s)
                    },
                    |s: i64| async move { Ok((s + 2).rem_euclid(7)) },
                ))
            }
        };
        match spec {
            Spec::Sum(ins) => declare_reads(h.id(), ins, &handles.borrow()),
            Spec::Pick(c, a, b) => declare_reads(h.id(), &[*c, *a, *b], &handles.borrow()),
            // The input's reads are declared on the internal effect.
            Spec::Fetched(ins) => {
                let H::A(a) = h else { unreachable!() };
                declare_reads(a.effect_id(), ins, &handles.borrow());
            }
            Spec::Primary | Spec::Written(_) => {}
        }
        handles.borrow_mut().push(h);
    }
    // Without declarations, written cells start at their settled value for
    // all-zero primaries, so the warm-up flush only learns. With them, they
    // start at 0 and the first flush is checked like any other.
    if !case.declare {
        let start = naive(&case.specs, &primary);
        for (i, s) in case.specs.iter().enumerate() {
            if let (Spec::Written(_), H::S(sig)) = (s, handles.borrow()[i]) {
                sig.set(&rt, start[i]).unwrap();
            }
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
                let ins2 = ins.clone();
                let hs2 = hs.clone();
                let r = rt.effect(move |rt| {
                    let mut seen = Vec::new();
                    for &j in &ins2 {
                        seen.push(hs2[j].get(rt)?);
                    }
                    let mut l = lg.borrow_mut();
                    l.runs[e] += 1;
                    l.seen[e].push(seen);
                    Ok(())
                });
                declare_reads(r.id(), ins, &hs);
                r
            }
            Fx::Writer(i) => {
                let Spec::Written(ins) = &case.specs[*i] else {
                    unreachable!()
                };
                let ins2 = ins.clone();
                let H::S(cell) = hs[*i] else { unreachable!() };
                let hs2 = hs.clone();
                let w = rt.effect(move |rt| {
                    let mut s = 0;
                    for &j in &ins2 {
                        s += hs2[j].get(rt)?;
                    }
                    lg.borrow_mut().runs[e] += 1;
                    cell.set(rt, (s + 1).rem_euclid(7))
                });
                declare_reads(w.id(), ins, &hs);
                if case.declare {
                    rt.writes_to(w.id(), cell.id()).unwrap();
                }
                w
            }
            Fx::OnChange(j) => {
                let j = *j;
                let hs2 = hs.clone();
                let oc = rt.on_change(
                    move |rt| hs2[j].get(rt),
                    move |_, v| {
                        let mut l = lg.borrow_mut();
                        l.runs[e] += 1;
                        l.fired[e].push(*v);
                        Ok(())
                    },
                );
                declare_reads(oc.id(), &[j], &hs);
                oc
            }
        };
        ids.push(effect);
    }
    // Without declarations: a warm-up flush (first runs, learning edges).
    // With them, the first flush is checked too (no writes before it).
    let mut prev = naive(&case.specs, &primary);
    let mut flushes: Vec<&[(usize, i64)]> = vec![&[]];
    flushes.extend(case.ticks.iter().map(Vec::as_slice));
    if !case.declare {
        let tick = rt.flush();
        prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        flushes.remove(0);
    }
    for (f, ticks) in flushes.into_iter().enumerate() {
        let first = case.declare && f == 0;
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
                !rose || case.dynamic() && !case.declare,
                "declared or static ranks are settled: effect {} rose",
                e
            );
            prop_assert!(
                l.runs[e] <= 1 || rose,
                "effect {} ({:?}) ran {} times in one flush",
                e,
                fx,
                l.runs[e]
            );
            if first && !matches!(fx, Fx::OnChange(_)) {
                prop_assert_eq!(l.runs[e], 1, "effect {} ({:?}) runs once at first", e, fx);
            }
            match fx {
                Fx::Reader(ins) => {
                    let want: Vec<i64> = ins.iter().map(|&j| fin[j]).collect();
                    if rose {
                        // Learned in this flush: only its last run is
                        // ordered after every writer.
                        if let Some(last) = l.seen[e].last() {
                            prop_assert_eq!(last, &want, "reader {} ended stale", e);
                        }
                    } else {
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
                    let want: Vec<i64> = if fin[*j] != prev[*j] && !first {
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
    // reader (created first) -> cell <- writer (created second). Only the
    // write edge is declared: the reader has never run and declares
    // nothing, so its first run waits for the ranked work.
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
    let tick = rt.flush();
    assert_eq!(*seen.borrow(), vec![10], "once, with the written value");
    assert_eq!(tick.effects_run, 2);
    assert!(rt.rank(reader.id()) > rt.rank(writer.id()));
    seen.borrow_mut().clear();
    x.set(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![20], "once, after its writer");
}

#[test]
fn declared_reads_rank_a_reader_before_it_runs() {
    // reader (created first) reads `m`, which reads `cell` only on one
    // branch; a writer created later fills `cell`. With the syntactic read
    // sets declared, the reader runs once per flush with final values, on
    // the first flush and when the branch switches.
    let rt = Runtime::new();
    let x = rt.signal(1);
    let sel = rt.signal(false);
    let cell = rt.signal(0);
    let m = rt.memo(move |rt| {
        if sel.get(rt)? {
            cell.get(rt)
        } else {
            x.get(rt)
        }
    });
    rt.reads_from(m.id(), &[sel.id(), cell.id(), x.id()])
        .unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let reader = rt.effect(move |rt| {
        s.borrow_mut().push(m.get(rt)?);
        Ok(())
    });
    rt.reads_from(reader.id(), &[m.id()]).unwrap();
    let writer = rt.effect(move |rt| cell.set(rt, x.get(rt)? * 10));
    rt.reads_from(writer.id(), &[x.id()]).unwrap();
    rt.writes_to(writer.id(), cell.id()).unwrap();
    assert!(
        rt.rank(reader.id()) > rt.rank(writer.id()),
        "ranked up front"
    );
    rt.flush();
    assert_eq!(*seen.borrow(), vec![1]);
    seen.borrow_mut().clear();
    // Switch to the written branch and change what the writer writes in
    // the same tick: one run, with the written value.
    sel.set(&rt, true).unwrap();
    x.set(&rt, 2).unwrap();
    let before = rt.rank(reader.id());
    rt.flush();
    assert_eq!(*seen.borrow(), vec![20]);
    assert_eq!(rt.rank(reader.id()), before, "no rank rose");
}

#[test]
fn a_sink_that_declares_nothing_runs_after_ranked_work_on_its_first_run() {
    let rt = Runtime::new();
    let x = rt.signal(1);
    let cell = rt.signal(0);
    let order = Rc::new(RefCell::new(Vec::new()));
    let o = order.clone();
    // Created first, declares nothing.
    rt.effect(move |rt| {
        o.borrow_mut().push(format!("plain {}", cell.get(rt)?));
        Ok(())
    });
    let o = order.clone();
    let w = rt.effect(move |rt| {
        o.borrow_mut().push("writer".to_string());
        cell.set(rt, x.get(rt)? + 1)
    });
    rt.reads_from(w.id(), &[x.id()]).unwrap();
    rt.writes_to(w.id(), cell.id()).unwrap();
    let o = order.clone();
    rt.on_change(
        move |rt| cell.get(rt),
        move |_, v| {
            o.borrow_mut().push(format!("change {v}"));
            Ok(())
        },
    );
    rt.flush();
    assert_eq!(*order.borrow(), vec!["writer", "plain 2"]);
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
    // Feedback, not an error: a self-normalising handler is valid.
    assert_eq!(
        rt.writes_to(norm.id(), x.id()),
        Ok(strand_core::WriteEdge::Feedback)
    );
    x.set(&rt, 3).unwrap();
    assert!(rt.flush().errors.is_empty());
    assert_eq!(x.get(&rt), Ok(4));
}

#[test]
fn a_self_normalising_handler_declared_in_either_order_is_the_same() {
    // `on change x { if x > 10 { x = 10 } }` and two handlers normalising
    // each other's cells: the compiler may declare reads and writes in any
    // order; the edge ends up feedback (not an error) with the same ranks.
    use strand_core::WriteEdge;
    #[derive(Debug, PartialEq)]
    struct Outcome {
        edges: Vec<Option<WriteEdge>>,
        ranks: Vec<u32>,
        x: i64,
        b: i64,
    }
    let run = |reads_first: bool| {
        let rt = Runtime::new();
        let src = rt.signal(0i64);
        let x = rt.signal(0i64);
        let b = rt.signal(0i64);
        // An ordinary ranked writer upstream, so ranks are not all zero.
        let feed = rt.effect(move |rt| x.set(rt, src.get(rt)?));
        let norm = rt.effect(move |rt| {
            if x.get(rt)? > 10 {
                x.set(rt, 10)?;
            }
            Ok(())
        });
        // Two handlers normalising each other: b = min(x, 5), and x never
        // above b once b is set.
        let ab = rt.effect(move |rt| {
            let v = x.get(rt)?.min(5);
            if b.get(rt)? != v {
                b.set(rt, v)?;
            }
            Ok(())
        });
        let ba = rt.effect(move |rt| {
            let v = b.get(rt)?;
            if v > 0 && x.get(rt)? > v {
                x.set(rt, v)?;
            }
            Ok(())
        });
        let mut edges = Vec::new();
        let reads = |rt: &Runtime| {
            rt.reads_from(feed.id(), &[src.id()]).unwrap();
            rt.reads_from(norm.id(), &[x.id()]).unwrap();
            rt.reads_from(ab.id(), &[x.id(), b.id()]).unwrap();
            rt.reads_from(ba.id(), &[x.id(), b.id()]).unwrap();
        };
        let writes = |rt: &Runtime, edges: &mut Vec<Result<WriteEdge, strand_core::Error>>| {
            edges.push(rt.writes_to(feed.id(), x.id()));
            edges.push(rt.writes_to(norm.id(), x.id()));
            edges.push(rt.writes_to(ab.id(), b.id()));
            edges.push(rt.writes_to(ba.id(), x.id()));
        };
        if reads_first {
            reads(&rt);
            writes(&rt, &mut edges);
        } else {
            writes(&rt, &mut edges);
            reads(&rt);
        }
        assert!(edges.iter().all(Result::is_ok), "never an error: {edges:?}");
        rt.flush();
        src.set(&rt, 30).unwrap();
        let tick = rt.flush();
        assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        let nodes = [src.id(), x.id(), b.id()];
        let sinks = [feed.id(), norm.id(), ab.id(), ba.id()];
        Outcome {
            edges: vec![
                rt.write_edge(feed.id(), x.id()),
                rt.write_edge(norm.id(), x.id()),
                rt.write_edge(ab.id(), b.id()),
                rt.write_edge(ba.id(), x.id()),
            ],
            ranks: nodes.iter().chain(&sinks).map(|&n| rt.rank(n)).collect(),
            x: x.get(&rt).unwrap(),
            b: b.get(&rt).unwrap(),
        }
    };
    let a = run(true);
    let b = run(false);
    assert_eq!(a, b);
    assert_eq!(a.edges[0], Some(WriteEdge::Ranked));
    assert_eq!(a.edges[1], Some(WriteEdge::Feedback));
    assert_eq!((a.x, a.b), (5, 5));
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
    assert_eq!(
        rt.writes_to(w.id(), c.id()),
        Ok(strand_core::WriteEdge::Feedback)
    );
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

#[test]
fn a_declared_reader_of_an_async_memo_runs_once_per_flush() {
    // `let hits = apps.search(q)` with `for h in hits`: the reader is
    // created first and declares its reads; the load resolves at once.
    let rt = Runtime::new();
    let q = rt.signal(1i64);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let hits: Rc<RefCell<Option<AsyncMemo<i64>>>> = Rc::new(RefCell::new(None));
    let h = hits.clone();
    let reader = rt.effect(move |rt| {
        let Some(m) = *h.borrow() else { return Ok(()) };
        let a = m.get(rt)?;
        s.borrow_mut().push((a.pending(), a.value().copied()));
        Ok(())
    });
    let m = rt.async_memo(move |rt| q.get(rt), |q: i64| async move { Ok(q * 10) });
    *hits.borrow_mut() = Some(m);
    rt.reads_from(m.effect_id(), &[q.id()]).unwrap();
    rt.reads_from(reader.id(), &[m.id()]).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![(false, Some(10))], "once, resolved");
    seen.borrow_mut().clear();
    q.set(&rt, 2).unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![(false, Some(20))], "once per query");
}

#[test]
fn a_declared_debounced_writer_orders_its_readers() {
    // `on change x after 100ms { cell = x * 2 }` with a reader of `cell`
    // created before it: the timer body's write edge is declared on
    // `Debounced::timer`, the input's read on `Debounced::effect`.
    let rt = Runtime::new();
    let x = rt.signal(0i64);
    let cell = rt.signal(0i64);
    let other = rt.signal(0i64);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let reader = rt.effect(move |rt| {
        s.borrow_mut().push((cell.get(rt)?, other.get(rt)?));
        Ok(())
    });
    let d = rt.on_change_after(
        move |rt| x.get(rt),
        Duration::from_millis(100),
        move |rt| cell.set(rt, x.get_untracked(rt)? * 2),
    );
    // Another declared writer the reader also reads, fed by the cell.
    let follow = rt.effect(move |rt| other.set(rt, cell.get(rt)? + 1));
    rt.reads_from(d.effect.id(), &[x.id()]).unwrap();
    rt.reads_from(d.timer.id(), &[]).unwrap();
    rt.writes_to(d.timer.id(), cell.id()).unwrap();
    rt.reads_from(follow.id(), &[cell.id()]).unwrap();
    rt.writes_to(follow.id(), other.id()).unwrap();
    rt.reads_from(reader.id(), &[cell.id(), other.id()])
        .unwrap();
    rt.flush();
    assert_eq!(*seen.borrow(), vec![(0, 1)]);
    seen.borrow_mut().clear();
    x.set(&rt, 4).unwrap();
    rt.tick(Duration::from_millis(10));
    rt.tick(Duration::from_millis(110));
    assert_eq!(*seen.borrow(), vec![(8, 9)], "once, with final values");
}

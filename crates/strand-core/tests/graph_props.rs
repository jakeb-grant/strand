//! Property tests: random graphs and random write sequences give the same
//! values as naive recomputation.
//!
//! * Every memo computation and every effect run checks, at that moment,
//!   that what it read equals naive recomputation from the current signal
//!   values: no glitch is ever observed, by memos or effects.
//! * Effects that write: "copy" effects write a sink signal that later memos
//!   read, and "normalizer" effects read some node (possibly for the first
//!   time, possibly downstream of the cell) and then write a primary signal
//!   upstream. After every flush each effect has seen the final values, so
//!   no effect ever goes deaf.
//! * Scopes holding memos are disposed mid-sequence; reads through their
//!   stale handles and of their dependents are error values, never panics.
//! * Without writing effects, effects run at most once per tick and exactly
//!   when an input changed, and watches report the changed memos (exactly,
//!   unless the tick pulled values mid-way or something was disposed).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use proptest::prelude::*;
use strand_core::{Memo, NodeId, Runtime, Scope, Signal};

#[derive(Clone, Debug)]
enum Spec {
    /// Sum of inputs mod 7 (collapses values: exercises the cut-off).
    Sum(Vec<usize>),
    /// `if x even { y } else { z }` (dynamic dependencies).
    Pick(usize, usize, usize),
    /// `min(x, c)` (cut-off).
    Clamp(usize, i64),
    /// A signal written by an effect that copies node `i` (earlier) into it.
    Sink(usize),
    /// `(x + 1) mod 7` through a memo the memo creates (and owns) on every
    /// run and reads right away (a component's local `let`).
    Local(usize),
}

#[derive(Clone, Debug)]
enum Op {
    Write(usize, i64),
    Read(usize),
    /// Dispose the scope holding memo group `g`.
    Dispose(usize),
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

/// Memos are created in scopes of this many consecutive items.
const GROUP: usize = 3;

/// Naive values of every node, given the value of each signal (primary or
/// sink) and the set of disposed memo items.
fn naive(
    n_sig: usize,
    specs: &[Spec],
    signal: &dyn Fn(usize) -> i64,
    disposed: &HashSet<usize>,
) -> Vec<Option<i64>> {
    let mut v: Vec<Option<i64>> = (0..n_sig).map(|i| Some(signal(i))).collect();
    for (k, spec) in specs.iter().enumerate() {
        let idx = n_sig + k;
        let x = if disposed.contains(&idx) {
            None
        } else {
            match spec {
                Spec::Sum(ins) => ins
                    .iter()
                    .map(|&i| v[i])
                    .sum::<Option<i64>>()
                    .map(|s| s.rem_euclid(7)),
                Spec::Pick(c, a, b) => {
                    v[*c].and_then(|c| if c.rem_euclid(2) == 0 { v[*a] } else { v[*b] })
                }
                Spec::Clamp(x, c) => v[*x].map(|x| x.min(*c)),
                Spec::Sink(_) => Some(signal(idx)),
                Spec::Local(x) => v[*x].map(|x| (x + 1).rem_euclid(7)),
            }
        };
        v.push(x);
    }
    v
}

/// What a sink holds when its source has no value (disposed).
const NO_VALUE: i64 = -100;

/// What a flush settles on: normalizer targets become even, then each sink
/// copies its source.
fn settle(
    n_sig: usize,
    specs: &[Spec],
    sigs: &mut [i64],
    normalizers: &[(usize, usize)],
    disposed: &HashSet<usize>,
) {
    for &(_, j) in normalizers {
        if sigs[j].rem_euclid(2) == 1 {
            sigs[j] += 1;
        }
    }
    for (k, spec) in specs.iter().enumerate() {
        if let Spec::Sink(i) = spec {
            let snapshot = sigs.to_vec();
            let now = naive(n_sig, specs, &|j| snapshot[j], disposed);
            sigs[n_sig + k] = now[*i].unwrap_or(NO_VALUE);
        }
    }
}

#[derive(Clone, Debug)]
struct Case {
    n_sig: usize,
    specs: Vec<Spec>,
    /// Read-only effects: the nodes each reads.
    effects: Vec<Vec<usize>>,
    /// `(read, target)`: read node `read`, then make primary `target` even.
    normalizers: Vec<(usize, usize)>,
    ticks: Vec<Vec<Op>>,
}

impl Case {
    fn writers(&self) -> bool {
        !self.normalizers.is_empty() || self.specs.iter().any(|s| matches!(s, Spec::Sink(_)))
    }
}

fn graph_strategy() -> impl Strategy<Value = Case> {
    (
        1usize..5,
        prop::collection::vec(
            (
                0u8..10,
                prop::collection::vec(any::<usize>(), 1..4),
                -2i64..6,
            ),
            1..30,
        ),
        prop::collection::vec(prop::collection::vec(any::<usize>(), 1..4), 0..5),
        prop::collection::vec((any::<usize>(), any::<usize>()), 0..3),
        any::<bool>(),
        prop::collection::vec(
            prop::collection::vec((0u8..10, any::<usize>(), -3i64..8), 0..5),
            1..25,
        ),
    )
        .prop_map(
            |(n_sig, raw_specs, raw_effects, raw_norm, writers, raw_ticks)| {
                let mut specs = Vec::new();
                for (i, (kind, idx, c)) in raw_specs.into_iter().enumerate() {
                    let avail = n_sig + i;
                    let pick = |k: usize| idx[k % idx.len()] % avail;
                    specs.push(match kind {
                        0..=2 => Spec::Sum(idx.iter().map(|&j| j % avail).collect()),
                        3..=5 => Spec::Pick(pick(0), pick(1), pick(2)),
                        6 | 7 => Spec::Clamp(pick(0), c),
                        9 => Spec::Local(pick(0)),
                        _ if writers => Spec::Sink(pick(0)),
                        _ => Spec::Clamp(pick(0), c),
                    });
                }
                let total = n_sig + specs.len();
                let effects = raw_effects
                    .into_iter()
                    .map(|e| e.into_iter().map(|j| j % total).collect())
                    .collect();
                let normalizers = if writers {
                    raw_norm
                        .into_iter()
                        .map(|(r, j)| (r % total, j % n_sig))
                        .collect()
                } else {
                    Vec::new()
                };
                let groups = specs.len().div_ceil(GROUP);
                let ticks = raw_ticks
                    .into_iter()
                    .map(|ops| {
                        ops.into_iter()
                            .map(|(w, j, v)| match w {
                                0..=4 => Op::Write(j % n_sig, v),
                                5..=8 => Op::Read(j % total),
                                _ => Op::Dispose(j % groups),
                            })
                            .collect()
                    })
                    .collect();
                Case {
                    n_sig,
                    specs,
                    effects,
                    normalizers,
                    ticks,
                }
            },
        )
}

/// 512 cases by default; `PROPTEST_CASES` overrides for stress runs.
fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn graph_matches_naive_recomputation(case in graph_strategy()) {
        check(&case)?;
    }
}

/// Shared with closures, so they can check themselves against naive
/// recomputation at the moment they run.
struct Model {
    n_sig: usize,
    specs: Vec<Spec>,
    /// Signal handle per node index (primary and sink signals).
    signals: RefCell<Vec<Option<Signal<i64>>>>,
    disposed: RefCell<HashSet<usize>>,
    violations: RefCell<Vec<String>>,
}

impl Model {
    fn naive_now(&self, rt: &Runtime) -> Vec<Option<i64>> {
        let signals = self.signals.borrow().clone();
        let disposed = self.disposed.borrow().clone();
        naive(
            self.n_sig,
            &self.specs,
            &|i| {
                signals[i]
                    .and_then(|s| s.get_untracked(rt).ok())
                    .unwrap_or(0)
            },
            &disposed,
        )
    }
}

type Log = Rc<RefCell<Vec<Vec<Option<i64>>>>>;

fn check(case: &Case) -> Result<(), TestCaseError> {
    let Case {
        n_sig,
        specs,
        effects,
        normalizers,
        ticks,
    } = case;
    let n_sig = *n_sig;
    let rt = Runtime::new();
    let model = Rc::new(Model {
        n_sig,
        specs: specs.clone(),
        signals: RefCell::new(Vec::new()),
        disposed: RefCell::new(HashSet::new()),
        violations: RefCell::new(Vec::new()),
    });
    // Signal values as the test expects them (primaries, then one slot
    // per item: used by sinks only).
    // Primaries start odd, so normalizers write on their very first run,
    // right after reading their nodes for the first time.
    let mut sigs = vec![0i64; n_sig + specs.len()];
    sigs[..n_sig].fill(1);
    let mut nodes: Vec<H> = Vec::new();
    for _ in 0..n_sig {
        let s = rt.signal(1);
        model.signals.borrow_mut().push(Some(s));
        nodes.push(H::S(s));
    }
    let groups = specs.len().div_ceil(GROUP);
    let scopes: Vec<Scope> = (0..groups).map(|_| rt.scope(|_| ()).0).collect();
    for (k, spec) in specs.iter().enumerate() {
        let idx = n_sig + k;
        if let Spec::Sink(i) = *spec {
            let sink = rt.signal(0);
            model.signals.borrow_mut().push(Some(sink));
            let src = nodes[i];
            rt.effect(move |rt| sink.set(rt, src.get(rt).unwrap_or(NO_VALUE)));
            nodes.push(H::S(sink));
            continue;
        }
        model.signals.borrow_mut().push(None);
        let spec = spec.clone();
        let inputs: Vec<H> = nodes.clone();
        let m2 = model.clone();
        let m = scopes[k / GROUP]
            .run(&rt, |rt| {
                rt.memo(move |rt| {
                    let r = match &spec {
                        Spec::Sum(ins) => {
                            let mut s = 0;
                            for &i in ins {
                                s += inputs[i].get(rt)?;
                            }
                            s.rem_euclid(7)
                        }
                        Spec::Pick(c, a, b) => {
                            if inputs[*c].get(rt)?.rem_euclid(2) == 0 {
                                inputs[*a].get(rt)?
                            } else {
                                inputs[*b].get(rt)?
                            }
                        }
                        Spec::Clamp(x, c) => inputs[*x].get(rt)?.min(*c),
                        Spec::Local(x) => {
                            let input = inputs[*x];
                            let local = rt.memo(move |rt| Ok(input.get(rt)? + 1));
                            local.get(rt)?.rem_euclid(7)
                        }
                        Spec::Sink(_) => unreachable!(),
                    };
                    // Glitch check: this computation saw a consistent view.
                    let expect = m2.naive_now(rt)[idx];
                    if expect != Some(r) {
                        m2.violations
                            .borrow_mut()
                            .push(format!("memo {idx} computed {r}, naive {expect:?}"));
                    }
                    Ok(r)
                })
            })
            .unwrap();
        nodes.push(H::M(m));
    }
    // Watch every memo.
    let memo_idx: Vec<usize> = (n_sig..nodes.len())
        .filter(|&i| matches!(nodes[i], H::M(_)))
        .collect();
    for &i in &memo_idx {
        rt.watch(nodes[i].id()).unwrap();
    }
    // Read-only effects record what they saw and check it.
    let logs: Vec<Log> = effects
        .iter()
        .enumerate()
        .map(|(e, reads)| {
            let log: Log = Rc::new(RefCell::new(Vec::new()));
            let l = log.clone();
            let hs: Vec<(usize, H)> = reads.iter().map(|&i| (i, nodes[i])).collect();
            let m2 = model.clone();
            rt.effect(move |rt| {
                // Odd effects read through local memos they own and
                // recreate on every run (an `if` branch's local `let`).
                let seen: Vec<Option<i64>> = hs
                    .iter()
                    .map(|&(_, h)| {
                        if e % 2 == 1 {
                            rt.memo(move |rt| h.get(rt)).get(rt).ok()
                        } else {
                            h.get(rt).ok()
                        }
                    })
                    .collect();
                let now = m2.naive_now(rt);
                let expect: Vec<Option<i64>> = hs.iter().map(|(i, _)| now[*i]).collect();
                if seen != expect {
                    m2.violations
                        .borrow_mut()
                        .push(format!("effect {e} saw {seen:?}, naive {expect:?}"));
                }
                l.borrow_mut().push(seen);
                Ok(())
            });
            log
        })
        .collect();
    // Normalizers: read a node, then the target through a memo (never the
    // target itself, so the write below reaches the effect only through a
    // memo it may be reading for the first time), then write the target.
    for &(r, j) in normalizers {
        let read = nodes[r];
        let H::S(target) = nodes[j] else {
            unreachable!()
        };
        let through = rt.memo(move |rt| target.get(rt));
        rt.effect(move |rt| {
            let _ = read.get(rt);
            let v = through.get(rt)?;
            if v.rem_euclid(2) == 1 {
                target.set(rt, v + 1)?;
            }
            Ok(())
        });
    }

    let tick = rt.flush();
    prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
    settle(n_sig, specs, &mut sigs, normalizers, &HashSet::new());
    let mut prev = naive(n_sig, specs, &|i| sigs[i], &HashSet::new());
    for (i, h) in nodes.iter().enumerate() {
        prop_assert_eq!(h.get(&rt).ok(), prev[i], "node {} after the first flush", i);
    }

    for ops in ticks {
        let runs_before: Vec<usize> = logs.iter().map(|l| l.borrow().len()).collect();
        let written_any = ops
            .iter()
            .any(|op| matches!(op, Op::Write(s, v) if sigs[*s] != *v))
            || ops.iter().any(|op| matches!(op, Op::Dispose(_)));
        // Mid-tick pulls may report an intermediate value. Once something
        // was disposed, a memo may switch from one `Disposed` error to
        // another (the model only knows "no value").
        let inexact = ops.iter().any(|op| matches!(op, Op::Read(_)))
            || !model.disposed.borrow().is_empty()
            || ops.iter().any(|op| matches!(op, Op::Dispose(_)));
        for op in ops {
            match *op {
                Op::Write(s, v) => {
                    sigs[s] = v;
                    let H::S(sig) = nodes[s] else { unreachable!() };
                    sig.set(&rt, v).unwrap();
                }
                Op::Read(i) => {
                    // Mid-tick pulls are consistent with the writes so far;
                    // stale handles read as errors.
                    let disposed = model.disposed.borrow().clone();
                    let now = naive(n_sig, specs, &|j| sigs[j], &disposed);
                    prop_assert_eq!(nodes[i].get(&rt).ok(), now[i], "read {}", i);
                }
                Op::Dispose(g) => {
                    scopes[g].dispose(&rt);
                    let mut d = model.disposed.borrow_mut();
                    for (k, spec) in specs.iter().enumerate().skip(g * GROUP).take(GROUP) {
                        if !matches!(spec, Spec::Sink(_)) {
                            d.insert(n_sig + k);
                        }
                    }
                }
            }
        }
        let tick = rt.flush();
        prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
        let violations = std::mem::take(&mut *model.violations.borrow_mut());
        prop_assert!(violations.is_empty(), "{:?}", violations);
        let disposed = model.disposed.borrow().clone();
        settle(n_sig, specs, &mut sigs, normalizers, &disposed);
        let now = naive(n_sig, specs, &|i| sigs[i], &disposed);
        for (i, h) in nodes.iter().enumerate() {
            prop_assert_eq!(h.get(&rt).ok(), now[i], "node {}", i);
        }
        // Every effect has seen the final values: none went deaf.
        for (e, reads) in effects.iter().enumerate() {
            let expect: Vec<Option<i64>> = reads.iter().map(|&i| now[i]).collect();
            let last = logs[e].borrow().last().cloned();
            prop_assert_eq!(last, Some(expect), "effect {} is stale", e);
        }
        // Watches report the memos whose value changed (each once).
        let expect_changed: Vec<NodeId> = memo_idx
            .iter()
            .filter(|&&i| prev[i] != now[i] && !disposed.contains(&i))
            .map(|&i| nodes[i].id())
            .collect();
        if case.writers() || inexact {
            for id in &expect_changed {
                prop_assert!(tick.changed.contains(id), "missed change of {:?}", id);
            }
        } else {
            prop_assert_eq!(&tick.changed, &expect_changed);
        }
        if !case.writers() {
            // Effects ran at most once, always when an input changed. (A
            // value written and restored within one tick may re-run an
            // effect; see docs/decisions.md.)
            for (e, reads) in effects.iter().enumerate() {
                let changed = reads.iter().any(|&i| prev[i] != now[i]);
                let ran = logs[e].borrow().len() - runs_before[e];
                prop_assert!(ran <= 1, "effect {} ran {} times", e, ran);
                prop_assert!(ran >= usize::from(changed), "effect {} missed a change", e);
                if !written_any {
                    prop_assert_eq!(ran, 0, "effect {} ran without writes", e);
                }
            }
        }
        prev = now;
        prop_assert!(rt.is_idle());
    }
    Ok(())
}

/// Shrunk failures found by stress runs, kept as regressions.
#[test]
fn regressions() {
    use Op::{Read, Write};
    let mut specs = vec![
        Spec::Clamp(0, 4),
        Spec::Sum(vec![0]),
        Spec::Sum(vec![0]),
        Spec::Clamp(0, 0),
        Spec::Sum(vec![2, 1]),
        Spec::Sum(vec![5]),
    ];
    specs.extend((0..12).map(|_| Spec::Sum(vec![0])));
    let ticks = vec![
        vec![Write(0, 2)],
        vec![Write(0, 7), Read(6), Write(0, 0), Read(6)],
    ];
    check(&Case {
        n_sig: 1,
        specs,
        effects: Vec::new(),
        normalizers: Vec::new(),
        ticks,
    })
    .unwrap();
}

/// The deaf-effect report: an effect that reads a memo for the first time
/// and then writes the memo's source in the same run.
#[test]
fn normalizer_reading_downstream_for_the_first_time() {
    use Op::Write;
    check(&Case {
        n_sig: 1,
        specs: vec![Spec::Sum(vec![0, 0])],
        effects: Vec::new(),
        normalizers: vec![(1, 0)],
        ticks: vec![vec![Write(0, 1)], vec![Write(0, 3)], vec![Write(0, 4)]],
    })
    .unwrap();
}

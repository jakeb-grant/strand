//! Property tests: random graphs and random write sequences give the same
//! values as naive recomputation, effects never observe a glitch, effects
//! run exactly when what they read changed, and watches report exactly the
//! changed nodes.

use std::cell::RefCell;
use std::rc::Rc;

use proptest::prelude::*;
use strand_core::{Memo, NodeId, Runtime, Signal};

#[derive(Clone, Debug)]
enum Spec {
    /// Sum of inputs mod 7 (collapses values: exercises the cut-off).
    Sum(Vec<usize>),
    /// `if x even { y } else { z }` (dynamic dependencies).
    Pick(usize, usize, usize),
    /// `min(x, c)` (cut-off).
    Clamp(usize, i64),
}

#[derive(Clone, Debug)]
enum Op {
    Write(usize, i64),
    Read(usize),
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

fn naive(signals: &[i64], specs: &[Spec]) -> Vec<i64> {
    let mut v: Vec<i64> = signals.to_vec();
    for spec in specs {
        let x = match spec {
            Spec::Sum(ins) => ins.iter().map(|&i| v[i]).sum::<i64>().rem_euclid(7),
            Spec::Pick(c, a, b) => {
                if v[*c].rem_euclid(2) == 0 {
                    v[*a]
                } else {
                    v[*b]
                }
            }
            Spec::Clamp(x, c) => v[*x].min(*c),
        };
        v.push(x);
    }
    v
}

/// Signal count, memo specs, effect read sets, ticks of operations.
type Case = (usize, Vec<Spec>, Vec<Vec<usize>>, Vec<Vec<Op>>);

fn graph_strategy() -> impl Strategy<Value = Case> {
    (
        1usize..5,
        prop::collection::vec(
            (
                0u8..3,
                prop::collection::vec(any::<usize>(), 1..4),
                -2i64..6,
            ),
            1..30,
        ),
        prop::collection::vec(prop::collection::vec(any::<usize>(), 1..4), 0..5),
        prop::collection::vec(
            prop::collection::vec((any::<bool>(), any::<usize>(), -3i64..8), 0..5),
            1..25,
        ),
    )
        .prop_map(|(n_sig, raw_specs, raw_effects, raw_ticks)| {
            let mut specs = Vec::new();
            for (i, (kind, idx, c)) in raw_specs.into_iter().enumerate() {
                let avail = n_sig + i;
                let pick = |k: usize| idx[k % idx.len()] % avail;
                specs.push(match kind {
                    0 => Spec::Sum(idx.iter().map(|&j| j % avail).collect()),
                    1 => Spec::Pick(pick(0), pick(1), pick(2)),
                    _ => Spec::Clamp(pick(0), c),
                });
            }
            let total = n_sig + specs.len();
            let effects = raw_effects
                .into_iter()
                .map(|e| e.into_iter().map(|j| j % total).collect())
                .collect();
            let ticks = raw_ticks
                .into_iter()
                .map(|ops| {
                    ops.into_iter()
                        .map(|(w, j, v)| {
                            if w {
                                Op::Write(j % n_sig, v)
                            } else {
                                Op::Read(j % total)
                            }
                        })
                        .collect()
                })
                .collect();
            (n_sig, specs, effects, ticks)
        })
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
    fn graph_matches_naive_recomputation((n_sig, specs, effects, ticks) in graph_strategy()) {
        check(n_sig, &specs, &effects, &ticks)?;
    }
}

fn check(
    n_sig: usize,
    specs: &[Spec],
    effects: &[Vec<usize>],
    ticks: &[Vec<Op>],
) -> Result<(), TestCaseError> {
    {
        let rt = Runtime::new();
        let mut sig_vals = vec![0i64; n_sig];
        let mut nodes: Vec<H> = (0..n_sig).map(|_| H::S(rt.signal(0))).collect();
        for spec in specs {
            let spec = spec.clone();
            let inputs: Vec<H> = nodes.clone();
            let m = rt.memo(move |rt| {
                Ok(match &spec {
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
                })
            });
            nodes.push(H::M(m));
        }
        // Watch every memo.
        for h in &nodes[n_sig..] {
            rt.watch(h.id()).unwrap();
        }
        // Effects record what they saw.
        let logs: Vec<Rc<RefCell<Vec<Vec<i64>>>>> = effects
            .iter()
            .map(|reads| {
                let log = Rc::new(RefCell::new(Vec::new()));
                let l = log.clone();
                let hs: Vec<H> = reads.iter().map(|&i| nodes[i]).collect();
                rt.effect(move |rt| {
                    let mut seen = Vec::new();
                    for h in &hs {
                        seen.push(h.get(rt)?);
                    }
                    l.borrow_mut().push(seen);
                    Ok(())
                });
                log
            })
            .collect();
        rt.flush();
        let mut prev = naive(&sig_vals, specs);
        for (e, reads) in effects.iter().enumerate() {
            let expect: Vec<i64> = reads.iter().map(|&i| prev[i]).collect();
            prop_assert_eq!(logs[e].borrow().clone(), vec![expect]);
        }

        for ops in ticks {
            let runs_before: Vec<usize> = logs.iter().map(|l| l.borrow().len()).collect();
            // Did any write differ from the value at the start of the tick?
            let written_any = ops
                .iter()
                .any(|op| matches!(op, Op::Write(s, v) if sig_vals[*s] != *v));
            for op in ops {
                match *op {
                    Op::Write(s, v) => {
                        sig_vals[s] = v;
                        let H::S(sig) = nodes[s] else { unreachable!() };
                        sig.set(&rt, v).unwrap();
                    }
                    Op::Read(i) => {
                        // Mid-tick pulls are consistent with the writes so far.
                        let now = naive(&sig_vals, specs);
                        if std::env::var("DBG").is_ok() {
                            let got: Vec<i64> = (0..nodes.len())
                                .map(|j| nodes[j].get(&rt).unwrap())
                                .collect();
                            eprintln!("read {i}: now {now:?}\n got {got:?}");
                        }
                        prop_assert_eq!(nodes[i].get(&rt).unwrap(), now[i]);
                    }
                }
            }
            let tick = rt.flush();
            prop_assert!(tick.errors.is_empty(), "{:?}", tick.errors);
            let now = naive(&sig_vals, specs);
            for (i, h) in nodes.iter().enumerate() {
                prop_assert_eq!(h.get(&rt).unwrap(), now[i], "node {}", i);
            }
            // Watches report exactly the memos whose value changed.
            let expect_changed: Vec<NodeId> = (n_sig..nodes.len())
                .filter(|&i| prev[i] != now[i])
                .map(|i| nodes[i].id())
                .collect();
            prop_assert_eq!(&tick.changed, &expect_changed);
            // Effects ran at most once, always when an input changed, and
            // saw a consistent (glitch-free) view. (A value written and
            // restored within one tick may re-run an effect; see
            // docs/decisions.md.)
            for (e, reads) in effects.iter().enumerate() {
                let log = logs[e].borrow();
                let changed = reads.iter().any(|&i| prev[i] != now[i]);
                let ran = log.len() - runs_before[e];
                prop_assert!(ran <= 1, "effect {} ran {} times", e, ran);
                prop_assert!(ran >= usize::from(changed), "effect {} missed a change", e);
                if !written_any {
                    prop_assert_eq!(ran, 0, "effect {} ran without writes", e);
                }
                if ran == 1 {
                    let expect: Vec<i64> = reads.iter().map(|&i| now[i]).collect();
                    prop_assert_eq!(log.last().unwrap(), &expect);
                }
            }
            prev = now;
            prop_assert!(rt.is_idle());
        }
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
    check(1, &specs, &[], &ticks).unwrap();
}

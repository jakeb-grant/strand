//! Keyed collections: incremental `filter`/`map`/`take`/`sort_by` against
//! naive recomputation, keyed diffs, and the reactive layer end to end.

use std::cmp::Ordering;

use proptest::prelude::*;
use strand_core::keyed::ops::{Filter, IncrementalOp, Map, SortBy, Take};
use strand_core::{KeyedOps, KeyedSource, KeyedVec, Runtime, VecDiff, keyed_diff};

type Item = (u8, i64);
type List = Vec<(u8, Item)>;

#[derive(Clone, Debug)]
enum Mut {
    Push(u8, i64),
    Insert(usize, u8, i64),
    Remove(usize),
    Move(usize, usize),
    Update(usize, i64),
    Replace(Vec<(u8, i64)>),
}

fn mut_strategy() -> impl Strategy<Value = Mut> {
    prop_oneof![
        3 => (any::<u8>(), -20i64..20).prop_map(|(k, v)| Mut::Push(k % 32, v)),
        3 => (any::<usize>(), any::<u8>(), -20i64..20).prop_map(|(i, k, v)| Mut::Insert(i, k % 32, v)),
        2 => any::<usize>().prop_map(Mut::Remove),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Mut::Move(a, b)),
        3 => (any::<usize>(), -20i64..20).prop_map(|(i, v)| Mut::Update(i, v)),
        1 => prop::collection::vec((any::<u8>(), -20i64..20), 0..12)
            .prop_map(|xs| Mut::Replace(xs.into_iter().map(|(k, v)| (k % 32, v)).collect())),
    ]
}

/// Apply a mutation to a KeyedVec; returns the diffs (empty when the
/// mutation was invalid, e.g. a duplicate key).
fn mutate(v: &mut KeyedVec<u8, Item>, m: &Mut) -> Vec<VecDiff<u8, Item>> {
    let len = v.len();
    let key_at = |i: usize| v.items()[i % len].0;
    let r = match m {
        Mut::Push(k, x) => v.push((*k, *x)).map(|d| vec![d]),
        Mut::Insert(i, k, x) => v.insert(i % (len + 1), (*k, *x)).map(|d| vec![d]),
        Mut::Remove(i) if len > 0 => {
            let k = key_at(*i);
            v.remove_key(&k).map(|d| vec![d])
        }
        Mut::Move(i, j) if len > 0 => {
            let k = key_at(*i);
            v.move_key(&k, j % len).map(|d| d.into_iter().collect())
        }
        Mut::Update(i, x) if len > 0 => {
            let k = key_at(*i);
            v.update(&k, |item| item.1 = *x)
                .map(|d| d.into_iter().collect())
        }
        Mut::Replace(xs) => {
            let mut seen = std::collections::HashSet::new();
            let xs: Vec<Item> = xs
                .iter()
                .copied()
                .filter(|(k, _)| seen.insert(*k))
                .collect();
            v.replace_all(xs)
        }
        _ => Ok(Vec::new()),
    };
    r.unwrap_or_default()
}

fn pred(_: &u8, v: &Item) -> bool {
    v.1 % 3 != 0
}

fn cmp(a: &Item, b: &Item) -> Ordering {
    (a.1 / 4).cmp(&(b.1 / 4))
}

fn naive_filter(src: &List) -> List {
    src.iter().filter(|(k, v)| pred(k, v)).cloned().collect()
}
fn naive_map(src: &List) -> Vec<(u8, i64)> {
    src.iter().map(|(k, v)| (*k, v.1 * 10)).collect()
}
fn naive_take(src: &List, n: usize) -> List {
    src.iter().take(n).cloned().collect()
}
fn naive_sort(src: &List) -> List {
    let mut s = src.clone();
    s.sort_by(|a, b| cmp(&a.1, &b.1));
    s
}

/// Drive an operator with the source diffs and check its mirror.
fn check_op<O, U>(
    muts: &[Mut],
    mut op: O,
    naive: impl Fn(&List) -> Vec<(u8, U)>,
) -> Result<(), TestCaseError>
where
    O: IncrementalOp<u8, Item, Out = U>,
    U: Clone + PartialEq + std::fmt::Debug,
{
    let mut src = KeyedVec::new(|v: &Item| v.0);
    let mut mirror: Vec<(u8, U)> = Vec::new();
    for m in muts {
        for d in mutate(&mut src, m) {
            let mut out = Vec::new();
            op.apply(&d, &mut out)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            for o in &out {
                // Keys in every emitted diff must match the mirror.
                o.apply(&mut mirror)
                    .map_err(|e| TestCaseError::fail(format!("{e}: {o:?}")))?;
            }
        }
        prop_assert_eq!(&mirror, &naive(&src.items().to_vec()));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn filter_is_incremental(muts in prop::collection::vec(mut_strategy(), 0..40)) {
        check_op(&muts, Filter::new(pred), naive_filter)?;
    }

    #[test]
    fn map_is_incremental(muts in prop::collection::vec(mut_strategy(), 0..40)) {
        check_op(&muts, Map::new(|_: &u8, v: &Item| v.1 * 10), naive_map)?;
    }

    #[test]
    fn take_is_incremental(muts in prop::collection::vec(mut_strategy(), 0..40), n in 0usize..6) {
        check_op(&muts, Take::new(n), |s| naive_take(s, n))?;
    }

    #[test]
    fn sort_by_is_incremental_and_stable(muts in prop::collection::vec(mut_strategy(), 0..40)) {
        check_op(&muts, SortBy::new(cmp), naive_sort)?;
    }

    #[test]
    fn keyed_diff_transforms_old_into_new(
        old in prop::collection::btree_map(0u8..20, -5i64..5, 0..12),
        new in prop::collection::btree_map(0u8..20, -5i64..5, 0..12),
        shuffle in any::<u64>(),
    ) {
        let old: Vec<(u8, i64)> = old.into_iter().collect();
        let mut new: Vec<(u8, i64)> = new.into_iter().collect();
        // Deterministic shuffle so order changes too.
        let n = new.len();
        if n > 1 {
            for i in 0..n {
                let j = ((shuffle >> (i % 60)) as usize + i * 7) % n;
                new.swap(i, j);
            }
        }
        let diffs = keyed_diff(&old, &new);
        let mut cur = old.clone();
        for d in &diffs {
            d.apply(&mut cur).unwrap();
        }
        prop_assert_eq!(&cur, &new);
        // Surviving keys are never removed and re-inserted.
        for d in &diffs {
            if let VecDiff::Remove { key, .. } = d {
                prop_assert!(!new.iter().any(|(k, _)| k == key));
            }
            if let VecDiff::Insert { key, .. } = d {
                prop_assert!(!old.iter().any(|(k, _)| k == key));
            }
        }
    }

    #[test]
    fn reactive_chain_matches_naive(
        muts in prop::collection::vec(mut_strategy(), 0..30),
        flips in prop::collection::vec(any::<bool>(), 30),
        n in 0usize..6,
    ) {
        let rt = Runtime::new();
        let xs = rt.keyed(KeyedVec::new(|v: &Item| v.0));
        let threshold = rt.signal(0i64);
        // xs.filter(v => v.1 >= threshold).sort_by(cmp).take(n).map(v => v.1)
        let filtered = xs.filter_with(&rt, move |rt| threshold.get(rt), |t, v: &Item| v.1 >= *t);
        let sorted = filtered.sort_by(&rt, cmp);
        let top = sorted.take(&rt, n);
        let values = top.map(&rt, |v: &Item| v.1);
        rt.watch(values.id()).unwrap();
        // A consumer that follows diffs, like the scene emitter.
        let mut mirror: Vec<(u8, i64)> = Vec::new();
        let mut seen: Option<u64> = None;
        let mut local = KeyedVec::new(|v: &Item| v.0);
        for (i, m) in muts.iter().enumerate() {
            let diffs = mutate(&mut local, m);
            xs.apply(&rt, &diffs).unwrap();
            if flips[i] {
                threshold.update(&rt, |t| *t = if *t == 0 { 5 } else { 0 }).unwrap();
            }
            let tick = rt.flush();
            prop_assert!(tick.errors.is_empty());
            let t = threshold.get_untracked(&rt).unwrap();
            let mut expect: List = local.items().iter().filter(|(_, v)| v.1 >= t).cloned().collect();
            expect.sort_by(|a, b| cmp(&a.1, &b.1));
            let expect: Vec<(u8, i64)> = expect.into_iter().take(n).map(|(k, v)| (k, v.1)).collect();
            let snap = values.snapshot(&rt).unwrap();
            prop_assert_eq!(snap.items(), &expect[..]);
            let before = mirror.clone();
            for d in snap.diffs_or_reset(seen) {
                d.apply(&mut mirror).unwrap();
            }
            seen = Some(snap.version());
            prop_assert_eq!(&mirror, &expect);
            if before != expect {
                prop_assert!(tick.changed.contains(&values.id()));
            }
        }
        // Only the first build and filter-parameter changes rebuild; every
        // other update flowed through the operators as diffs.
        let flipped = flips[..muts.len()].iter().filter(|&&f| f).count() as u64;
        prop_assert_eq!(rt.stats().rebuilds, 4 + flipped);
    }
}

#[test]
fn keyed_vec_rejects_bad_operations() {
    use strand_core::KeyedError;
    let mut v = KeyedVec::new(|x: &Item| x.0);
    v.push((1, 10)).unwrap();
    assert_eq!(v.push((1, 11)), Err(KeyedError::DuplicateKey));
    assert_eq!(v.remove_key(&9), Err(KeyedError::MissingKey));
    assert_eq!(
        v.insert(5, (2, 0)),
        Err(KeyedError::IndexOutOfRange { index: 5, len: 1 })
    );
    assert_eq!(v.update(&1, |x| x.0 = 2), Err(KeyedError::KeyChanged));
    assert_eq!(v.get(&1), Some(&(1, 10)), "reverted");
    assert_eq!(v.update(&1, |x| x.1 = 10), Ok(None), "unchanged value");
    assert_eq!(v.move_key(&1, 0), Ok(None));
}

#[test]
fn reactive_ops_publish_keyed_diffs_and_cut_off() {
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::from_values(|v: &Item| v.0, [(1, 1), (2, 2), (3, 3)]).unwrap());
    let odd = xs.filter(&rt, |v: &Item| v.1 % 2 == 1);
    rt.watch(odd.id()).unwrap();
    let s0 = odd.snapshot(&rt).unwrap();
    assert_eq!(s0.items(), &[(1, (1, 1)), (3, (3, 3))]);
    // A change the filter drops: no diff, no report.
    xs.update(&rt, &2, |v| v.1 = 4).unwrap();
    let tick = rt.flush();
    assert!(tick.changed.is_empty());
    assert_eq!(odd.snapshot(&rt).unwrap().version(), s0.version());
    // Moving a kept item publishes a Move with its key.
    xs.move_key(&rt, &3, 0).unwrap();
    let tick = rt.flush();
    assert_eq!(tick.changed, vec![odd.id()]);
    let s1 = odd.snapshot(&rt).unwrap();
    assert_eq!(
        s1.diffs_since(s0.version()).unwrap(),
        vec![VecDiff::Move {
            from: 1,
            to: 0,
            key: 3
        }]
    );
}

#[test]
fn param_change_rebuild_keeps_keys() {
    let rt = Runtime::new();
    let xs =
        rt.keyed(KeyedVec::from_values(|v: &Item| v.0, (0..6).map(|i| (i, i as i64))).unwrap());
    let dnd = rt.signal(false);
    let shown = xs.filter_with(&rt, move |rt| dnd.get(rt), |dnd, v: &Item| !dnd || v.1 >= 4);
    let s0 = shown.snapshot(&rt).unwrap();
    assert_eq!(s0.len(), 6);
    dnd.set(&rt, true).unwrap();
    let s1 = shown.snapshot(&rt).unwrap();
    let diffs = s1.diffs_since(s0.version()).unwrap();
    // Four removals by key, no reset: the survivors keep identity.
    assert_eq!(diffs.len(), 4);
    assert!(diffs.iter().all(|d| matches!(d, VecDiff::Remove { .. })));
}

#[test]
fn lagging_consumer_gets_a_reset() {
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::new(|v: &Item| v.0));
    let s0 = xs.snapshot(&rt).unwrap();
    for i in 0..(strand_core::keyed::reactive::LOG_CAPACITY + 10) {
        xs.push(&rt, ((i % 256) as u8, i as i64)).unwrap();
        xs.remove_key(&rt, &((i % 256) as u8)).unwrap();
    }
    let _ = &s0;
    let s1 = xs.snapshot(&rt).unwrap();
    assert!(s1.diffs_since(s0.version()).is_none());
    assert!(matches!(
        s1.diffs_or_reset(Some(s0.version()))[..],
        [VecDiff::Reset { .. }]
    ));
}

#[test]
fn a_malformed_service_batch_keeps_derived_collections_consistent() {
    let rt = Runtime::new();
    let xs = rt.keyed(KeyedVec::from_values(|v: &Item| v.0, [(1, 1), (2, 2), (3, 3)]).unwrap());
    let all = xs.filter(&rt, |_| true);
    let s0 = all.snapshot(&rt).unwrap();
    let r = xs.apply(
        &rt,
        &[
            VecDiff::Insert {
                index: 3,
                key: 4,
                value: (4, 4),
            },
            VecDiff::Remove { index: 9, key: 9 },
        ],
    );
    assert!(r.is_err());
    rt.flush();
    // The prefix that applied is published, so every consumer agrees.
    let src = xs.snapshot(&rt).unwrap();
    let derived = all.snapshot(&rt).unwrap();
    assert_eq!(src.len(), 4);
    assert_eq!(derived.items(), src.items());
    let mut mirror = s0.items().to_vec();
    for d in derived.diffs_since(s0.version()).unwrap() {
        d.apply(&mut mirror).unwrap();
    }
    assert_eq!(&mirror[..], src.items());
}

proptest! {
    /// A comparator that is not a total order (NaN from service data)
    /// never panics, on the reset path or the incremental one, and the
    /// output always holds every key exactly once.
    #[test]
    fn sort_by_with_nan_never_panics(
        initial in prop::collection::vec((any::<u8>(), prop::option::of(-5.0f64..5.0)), 0..40),
        muts in prop::collection::vec((any::<u8>(), any::<usize>(), prop::option::of(-5.0f64..5.0)), 0..40),
    ) {
        type F = (u8, f64);
        let f = |v: Option<f64>| v.unwrap_or(f64::NAN);
        let rt = Runtime::new();
        let mut seen = std::collections::HashSet::new();
        let values: Vec<F> = initial
            .into_iter()
            .filter(|(k, _)| seen.insert(*k))
            .map(|(k, v)| (k, f(v)))
            .collect();
        let xs = rt.keyed(KeyedVec::from_values(|v: &F| v.0, values).unwrap());
        let sorted = xs.sort_by(&rt, |a: &F, b: &F| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
        sorted.snapshot(&rt).unwrap();
        for (k, i, v) in muts {
            let cur = xs.get_untracked(&rt).unwrap();
            if cur.is_empty() || k % 3 == 0 {
                let _ = xs.push(&rt, (k, f(v)));
            } else {
                let key = cur.items()[i % cur.len()].0;
                if k % 3 == 1 {
                    xs.update(&rt, &key, |x| x.1 = f(v)).unwrap();
                } else {
                    xs.remove_key(&rt, &key).unwrap();
                }
            }
            let out = sorted.snapshot(&rt).unwrap();
            let mut got: Vec<u8> = out.keys().copied().collect();
            let mut want: Vec<u8> = xs.get_untracked(&rt).unwrap().items().iter().map(|(k, _)| *k).collect();
            got.sort_unstable();
            want.sort_unstable();
            prop_assert_eq!(got, want);
        }
    }
}

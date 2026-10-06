//! Scaling guards for keyed collections: `keyed_diff` and `keyed_memo` do
//! O(1) key work per row (hash index, no pairwise scans), the index work of
//! a reordering diff is O(n log n) (longest increasing subsequence and a
//! Fenwick tree, not quadratic), and a key lookup in a `KeyedVec` is O(1),
//! not a scan. Correctness lives in `keyed_props.rs`; these fail if the
//! work regresses to linear key scans or an O(n²) diff, at 2,000 rows (the
//! benchmark's size, `docs/benchmarks.md`) and 16,000.

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use strand_core::{KeyedVec, Runtime, VecDiff, keyed_diff};

thread_local! {
    static HASHES: Cell<u64> = const { Cell::new(0) };
    static EQS: Cell<u64> = const { Cell::new(0) };
}

/// A key that counts the hashing and comparing done with it.
#[derive(Clone, Debug)]
struct Key(u32);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        EQS.with(|c| c.set(c.get() + 1));
        self.0 == other.0
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        HASHES.with(|c| c.set(c.get() + 1));
        self.0.hash(state);
    }
}

/// Key operations (hashes plus comparisons) done by `f`.
fn key_ops<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = HASHES.with(Cell::get) + EQS.with(Cell::get);
    let r = f();
    let after = HASHES.with(Cell::get) + EQS.with(Cell::get);
    (r, after - before)
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn list(n: u32) -> Vec<(Key, u32)> {
    (0..n).map(|i| (Key(i), i)).collect()
}

fn shuffled(n: u32, seed: u64) -> Vec<(Key, u32)> {
    let mut v = list(n);
    let mut rng = Rng(seed);
    for i in (1..v.len()).rev() {
        let j = rng.below(i + 1);
        v.swap(i, j);
    }
    v
}

/// The shapes a diff meets: an update, an insert plus a removal in the
/// middle, a reversal and a full shuffle.
fn shapes(n: u32) -> Vec<(&'static str, Vec<(Key, u32)>)> {
    let mid = n as usize / 2;
    let mut update = list(n);
    update[mid].1 += 1;
    let mut edit = list(n);
    edit.remove(mid);
    edit.insert(mid / 2, (Key(n + 1), 0));
    let mut reversed = list(n);
    reversed.reverse();
    vec![
        ("update", update),
        ("insert+remove", edit),
        ("reversed", reversed),
        ("shuffled", shuffled(n, 0x5eed_0001)),
    ]
}

fn apply(old: &[(Key, u32)], diffs: &[VecDiff<Key, u32>]) -> Vec<u32> {
    let mut v: Vec<u32> = old.iter().map(|(k, _)| k.0).collect();
    for d in diffs {
        match d {
            VecDiff::Insert { index, key, .. } => v.insert(*index, key.0),
            VecDiff::Remove { index, .. } => {
                v.remove(*index);
            }
            VecDiff::Move { from, to, .. } => {
                let k = v.remove(*from);
                v.insert(*to, k);
            }
            VecDiff::Update { .. } => {}
            VecDiff::Reset { items } => v = items.iter().map(|(k, _)| k.0).collect(),
        }
    }
    v
}

/// Key work per row of `keyed_diff` is bounded by a constant at 2,000 and
/// 16,000 rows for every shape: an O(n²) pairwise match would do ~n/2
/// comparisons per row (1,000 at 2k, 8,000 at 16k).
#[test]
fn keyed_diff_does_constant_key_work_per_row() {
    for n in [2_000u32, 16_000] {
        let old = list(n);
        for (name, new) in shapes(n) {
            let (diffs, ops) = key_ops(|| keyed_diff(&old, &new));
            let want: Vec<u32> = new.iter().map(|(k, _)| k.0).collect();
            assert_eq!(apply(&old, &diffs), want, "{name} at {n}");
            assert!(
                !diffs.iter().any(|d| matches!(d, VecDiff::Reset { .. })),
                "{name} at {n}: a reset"
            );
            let per_row = ops as f64 / f64::from(n);
            assert!(
                per_row <= 8.0,
                "{name} at {n}: {per_row:.1} key ops per row"
            );
        }
    }
}

/// The best of `runs` timings of `f`.
fn best(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// The index work of a reordering diff is O(n log n): shuffling 8x the rows
/// costs about 8 x 1.3 = 10x the time; a quadratic diff (LIS by dynamic
/// programming, index by linear search) costs 64x. Best of several runs, so
/// a busy machine only makes both slower.
#[test]
fn a_shuffled_keyed_diff_scales_n_log_n() {
    let (small, large) = (2_000u32, 16_000u32);
    let time = |n: u32| {
        let old = list(n);
        let new = shuffled(n, 0x5eed_0002);
        best(7, || {
            std::hint::black_box(keyed_diff(&old, &new));
        })
    };
    // Warm up allocations and caches once.
    time(small);
    let ratio = time(large).as_secs_f64() / time(small).as_secs_f64().max(1e-9);
    assert!(ratio < 32.0, "16k / 2k rows: {ratio:.1}x");
}

/// Every lookup by key in a `KeyedVec` of 2,000 or 16,000 rows does a
/// constant amount of key work: fresh, after an insert at the front (every
/// index entry stale by one) and after a removal. A scan would compare n/2
/// keys per lookup on average.
#[test]
fn key_lookups_do_constant_key_work() {
    for n in [2_000u32, 16_000] {
        let mut v = KeyedVec::from_values(|x: &u32| Key(*x), 0..n).unwrap();
        let check = |v: &KeyedVec<Key, u32>, what: &str| {
            let keys: Vec<Key> = v.items().iter().map(|(k, _)| k.clone()).collect();
            for (i, k) in keys.iter().enumerate() {
                let (at, ops) = key_ops(|| v.index_of(k));
                assert_eq!(at, Some(i), "{what} at {n}");
                assert!(ops <= 10, "{what} at {n}: index_of did {ops} key ops");
                let (got, ops) = key_ops(|| v.get(k).copied());
                assert_eq!(got, Some(k.0), "{what} at {n}");
                assert!(ops <= 10, "{what} at {n}: get did {ops} key ops");
                let (has, ops) = key_ops(|| v.contains_key(k));
                assert!(has && ops <= 3, "{what} at {n}: contains_key did {ops}");
            }
            let (missing, ops) = key_ops(|| v.index_of(&Key(u32::MAX)));
            assert!(
                missing.is_none() && ops <= 3,
                "{what} at {n}: miss did {ops}"
            );
        };
        check(&v, "fresh");
        v.insert(0, n + 10).unwrap();
        check(&v, "after an insert at the front");
        v.remove_key(&Key(n / 2)).unwrap();
        check(&v, "after a removal");
    }
}

/// A `keyed_memo` over a plain list re-diffs the whole list when one row
/// changes: constant key work per row, at 2,000 and 16,000 rows.
#[test]
fn keyed_memo_does_constant_key_work_per_row() {
    for n in [2_000u32, 16_000] {
        let rt = Runtime::new();
        let src = rt.signal((0..n).collect::<Vec<u32>>());
        let xs = rt.keyed_memo(|x: &u32| Key(*x), move |rt| src.get(rt));
        let shown = rt.memo(move |rt| xs.with(rt, |v| v.len()));
        assert_eq!(shown.get(&rt), Ok(n as usize));
        // One row replaced in the middle.
        let ((), ops) = key_ops(|| {
            src.update(&rt, |v| v[n as usize / 2] = n + 1).unwrap();
            rt.flush();
            assert_eq!(shown.get(&rt), Ok(n as usize));
        });
        let per_row = ops as f64 / f64::from(n);
        assert!(per_row <= 8.0, "{n} rows: {per_row:.1} key ops per row");
        // Reversed: a reordering re-diff is still constant key work per row.
        let ((), ops) = key_ops(|| {
            src.update(&rt, |v| v.reverse()).unwrap();
            rt.flush();
            assert_eq!(shown.get(&rt), Ok(n as usize));
        });
        let per_row = ops as f64 / f64::from(n);
        assert!(
            per_row <= 8.0,
            "{n} rows reversed: {per_row:.1} key ops per row"
        );
    }
}

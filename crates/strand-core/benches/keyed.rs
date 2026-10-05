//! Keyed collections at 2,000 rows (M4's exit is smooth 2,000-row
//! scrolling): single-row writes through a derived `filter → sort_by →
//! take` chain with the emitter holding snapshots, a filter query change
//! (rebuild + keyed diff of the outputs), `keyed_memo` over a plain list,
//! `keyed_diff` shapes and key lookups.
//!
//! Run: `cargo bench -p strand-core --bench keyed`. Results go in
//! `docs/benchmarks.md`.

use std::hint::black_box;
use std::rc::Rc;
use std::time::Duration;

use criterion::Criterion;
use strand_core::{KeyedOps, KeyedSource, KeyedVec, Runtime, Snapshot, keyed_diff};

/// Rows in every case.
const ROWS: usize = 2_000;

/// A row as a VM value would carry it: a key, a shared string and a number.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    id: u32,
    title: Rc<str>,
    score: i64,
}

fn row(id: u32, score: i64) -> Row {
    Row {
        id,
        title: Rc::from(format!("item {id}")),
        score,
    }
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

fn rows(n: usize) -> Vec<Row> {
    (0..n as u32)
        .map(|i| row(i, i64::from(i * 7919 % 1000)))
        .collect()
}

/// What the scene emitter does each tick: read the snapshot, take the diffs
/// since the version it last saw, keep the snapshot.
struct Emitter<T> {
    last: Option<Snapshot<u32, T>>,
}

impl<T: Clone> Emitter<T> {
    fn consume(&mut self, snap: Snapshot<u32, T>) -> usize {
        let n = snap
            .diffs_or_reset(self.last.as_ref().map(Snapshot::version))
            .len();
        self.last = Some(snap);
        n
    }
}

fn chain_case(c: &mut Criterion) {
    let rt = Runtime::new();
    let cell = rt.keyed(KeyedVec::from_values(|r: &Row| r.id, rows(ROWS)).unwrap());
    let query = rt.signal(0i64);
    let shown = cell
        .filter_with(&rt, move |rt| query.get(rt), |q, r: &Row| r.score % 3 != *q)
        .sort_by(&rt, |a, b| a.score.cmp(&b.score))
        .take(&rt, 50);
    let titles = cell.map(&rt, |r: &Row| r.title.clone());
    let mut e_cell = Emitter { last: None };
    let mut e_shown = Emitter { last: None };
    let mut e_titles = Emitter { last: None };
    let mut tick = |rt: &Runtime| {
        rt.flush();
        e_cell.consume(cell.snapshot(rt).unwrap())
            + e_shown.consume(shown.snapshot(rt).unwrap())
            + e_titles.consume(titles.snapshot(rt).unwrap())
    };
    tick(&rt);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    let mut g = c.benchmark_group("keyed 2k");
    g.measurement_time(Duration::from_secs(4));
    g.bench_function(
        "update one row + flush (cell, filter→sort→take, map)",
        |b| {
            b.iter(|| {
                let id = rng.below(ROWS) as u32;
                let score = rng.below(1000) as i64;
                cell.update(&rt, &id, |r| r.score = score).unwrap();
                black_box(tick(&rt))
            });
        },
    );
    let mut next = ROWS as u32;
    g.bench_function("remove one row + push one + flush", |b| {
        b.iter(|| {
            // A clone shares the key index; drop it before writing.
            let victim = {
                let items = cell.get_untracked(&rt).unwrap();
                items.items()[rng.below(items.len())].0
            };
            cell.remove_key(&rt, &victim).unwrap();
            cell.push(&rt, row(next, rng.below(1000) as i64)).unwrap();
            next += 1;
            black_box(tick(&rt))
        });
    });
    g.bench_function("move one row + flush", |b| {
        b.iter(|| {
            let (key, to) = {
                let items = cell.get_untracked(&rt).unwrap();
                let n = items.len();
                (items.items()[rng.below(n)].0, rng.below(n))
            };
            cell.move_key(&rt, &key, to).unwrap();
            black_box(tick(&rt))
        });
    });
    g.bench_function("filter query change + flush (rebuild, keyed diff)", |b| {
        let mut q = 0;
        b.iter(|| {
            q = (q + 1) % 3;
            query.set(&rt, q).unwrap();
            black_box(tick(&rt))
        });
    });
    g.finish();
}

fn memo_case(c: &mut Criterion) {
    let rt = Runtime::new();
    let src = rt.signal(rows(ROWS));
    let list = rt.keyed_memo(|r: &Row| r.id, move |rt| src.get(rt));
    let mut emitter = Emitter { last: None };
    rt.flush();
    emitter.consume(list.snapshot(&rt).unwrap());
    let mut rng = Rng(7);
    let mut g = c.benchmark_group("keyed 2k");
    g.measurement_time(Duration::from_secs(4));
    g.bench_function("keyed_memo: one row changed + flush", |b| {
        b.iter(|| {
            let i = rng.below(ROWS);
            let score = rng.below(1000) as i64;
            src.update(&rt, |v| v[i].score = score).unwrap();
            rt.flush();
            black_box(emitter.consume(list.snapshot(&rt).unwrap()))
        });
    });
    g.bench_function("keyed_memo: list rotated by one + flush", |b| {
        b.iter(|| {
            src.update(&rt, |v| v.rotate_left(1)).unwrap();
            rt.flush();
            black_box(emitter.consume(list.snapshot(&rt).unwrap()))
        });
    });
    g.finish();
}

fn diff_case(c: &mut Criterion) {
    let base: Vec<(u32, Row)> = rows(ROWS).into_iter().map(|r| (r.id, r)).collect();
    let mut rng = Rng(42);
    let mut shuffled = base.clone();
    for i in (1..shuffled.len()).rev() {
        let j = rng.below(i + 1);
        shuffled.swap(i, j);
    }
    let reversed: Vec<_> = base.iter().rev().cloned().collect();
    let mut one_move = base.clone();
    let item = one_move.remove(10);
    one_move.insert(1_900, item);
    let mut churn = base.clone();
    for k in 0..10 {
        churn.remove(k * 150);
        churn.insert(k * 170, (100_000 + k as u32, row(100_000 + k as u32, 0)));
    }
    let mut g = c.benchmark_group("keyed_diff 2k");
    g.measurement_time(Duration::from_secs(4));
    for (name, new) in [
        ("identical", &base),
        ("one move", &one_move),
        ("10 removes + 10 inserts", &churn),
        ("reversed", &reversed),
        ("shuffled", &shuffled),
    ] {
        g.bench_function(name, |b| b.iter(|| black_box(keyed_diff(&base, new).len())));
    }
    g.finish();
}

fn lookup_case(c: &mut Criterion) {
    let v = KeyedVec::from_values(|r: &Row| r.id, rows(ROWS)).unwrap();
    let mut rng = Rng(3);
    let mut g = c.benchmark_group("keyed 2k");
    g.bench_function("get by key", |b| {
        b.iter(|| black_box(v.get(&(rng.below(ROWS) as u32)).map(|r| r.score)));
    });
    let mut owned = v.clone();
    g.bench_function("update by key (KeyedVec, no snapshot alive)", |b| {
        b.iter(|| {
            let id = rng.below(ROWS) as u32;
            black_box(owned.update(&id, |r| r.score += 1).unwrap())
        });
    });
    g.finish();
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    lookup_case(&mut c);
    diff_case(&mut c);
    chain_case(&mut c);
    memo_case(&mut c);
    c.final_summary();
}

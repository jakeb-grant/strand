//! Persisted cells at 2,000 rows: mounting and unmounting a list whose
//! items each persist a field under an instance-qualified path
//! (`list[<key>].x`), with the files present, as a live reload or a long
//! list scrolled in and out would. Unmount releases every claim (and
//! queues the live value only if it changed); mount reads every file.
//!
//! Run: `cargo bench -p strand-core --bench persist`. Results go in
//! `docs/benchmarks.md`.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::Criterion;
use strand_core::{PersistStore, Runtime, Scope};

const ROWS: usize = 2_000;

fn mount(rt: &Runtime, store: &PersistStore, rows: usize) -> Scope {
    rt.scope(|rt| {
        for i in 0..rows {
            let p = rt.persisted_value(store, &format!("list[{i}].x"), 0i64);
            black_box(p.signal);
        }
    })
    .0
}

fn churn(c: &mut Criterion) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("strand-bench-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = PersistStore::new(&dir);
    // Seed one file per row.
    {
        let rt = Runtime::new();
        let scope = rt.scope(|rt| {
            (0..ROWS)
                .map(|i| rt.persisted_value(&store, &format!("list[{i}].x"), 0i64))
                .collect::<Vec<_>>()
        });
        rt.flush();
        for (i, p) in scope.1.iter().enumerate() {
            p.signal.set(&rt, i as i64 + 1).unwrap();
        }
        rt.flush();
        rt.shutdown();
        assert!(store.sync(Duration::from_secs(30)));
    }
    let rt = Runtime::new();
    let mut g = c.benchmark_group("persist 2k");
    g.sample_size(20);
    g.measurement_time(Duration::from_secs(6));
    for rows in [500, 1_000, ROWS] {
        g.bench_function(format!("mount {rows} persisted rows + flush"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let scope = mount(&rt, &store, rows);
                    rt.flush();
                    total += start.elapsed();
                    scope.dispose(&rt);
                    rt.flush();
                }
                total
            });
        });
        g.bench_function(format!("unmount {rows} persisted rows + flush"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let scope = mount(&rt, &store, rows);
                    rt.flush();
                    let start = Instant::now();
                    scope.dispose(&rt);
                    rt.flush();
                    total += start.elapsed();
                }
                total
            });
        });
    }
    g.finish();
    rt.shutdown();
    drop(rt);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let mut c = Criterion::default().configure_from_args();
    churn(&mut c);
    c.final_summary();
}

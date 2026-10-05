//! M0 benchmark: a 10k-node reactive graph (mixed fan-in/fan-out, depth
//! 20). Measures single-write propagation, full fan-out, a narrow path (one
//! chain of 20 memos, what a clock tick does), an equality cut-off (a write
//! stopped after one memo) and an idle flush, the same flushes once a
//! declared handler write has populated the rank map (`ranked_*`), and
//! reports memory per node through a counting global allocator.
//!
//! Run: `cargo bench -p strand-core --bench graph`. Results go in
//! `docs/benchmarks.md`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BatchSize, Criterion};

#[path = "support/graph_builder.rs"]
mod graph_builder;
use graph_builder::{NODES, Rng, build};
use strand_core::{Memo, Runtime, Signal};

/// `src -> first -> 19 more memos -> watch`: a chain of 20 memos.
fn chain(rt: &Runtime, first: impl Fn(i64) -> i64 + 'static) -> Signal<i64> {
    let src = rt.signal(0i64);
    let mut prev: Memo<i64> = rt.memo(move |rt| src.get(rt).map(&first));
    for _ in 1..20 {
        let p = prev;
        prev = rt.memo(move |rt| p.get(rt).map(|v| v.wrapping_add(1)));
    }
    rt.watch(prev.id()).unwrap();
    rt.flush();
    src
}

/// Counts live bytes and allocation calls.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call to `System` unchanged; the counters are
// atomics and never affect the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn report_memory() {
    let live0 = LIVE.load(Ordering::SeqCst);
    let allocs0 = ALLOCS.load(Ordering::SeqCst);
    let g = build(1);
    let live = LIVE.load(Ordering::SeqCst) - live0;
    let allocs = ALLOCS.load(Ordering::SeqCst) - allocs0;
    // Subtract the builder's own bookkeeping (layers, flat, inputs), which
    // a real program does not keep.
    let words = std::mem::size_of::<usize>();
    let node = std::mem::size_of_val(&g.flat[0]);
    let bookkeeping = g.flat.capacity() * node
        + g.layers.iter().map(|l| l.capacity() * node).sum::<usize>()
        + g.inputs
            .iter()
            .map(|i| i.capacity() * words + 3 * words)
            .sum::<usize>();
    let graph = live as usize - bookkeeping;
    let nodes = g.rt.stats().nodes;
    println!(
        "memory: {graph} live bytes for {nodes} nodes ({NODES} signals+memos, {} watches)",
        g.watches
    );
    // The benchmark's own closure captures (a `Vec<Node>` of inputs per
    // memo), which stand in for the VM's compiled bindings.
    let captures: usize = g.inputs.iter().map(|i| i.len() * node + 3 * words).sum();
    println!(
        "memory: {:.1} bytes/node, {:.2} allocations/node during build",
        graph as f64 / nodes as f64,
        allocs as f64 / nodes as f64
    );
    println!(
        "memory: {:.1} bytes/node excluding closure captures",
        (graph - captures) as f64 / nodes as f64
    );
    // Propagation reach, for context.
    let rt = &g.rt;
    let before = rt.stats().computations;
    g.signals[10].set(rt, 12345).unwrap();
    let tick = rt.flush();
    println!(
        "single write: {} memos recomputed, {} watched leaves changed",
        rt.stats().computations - before,
        tick.changed.len()
    );
    let before = rt.stats().computations;
    g.root.set(rt, 999).unwrap();
    let tick = rt.flush();
    println!(
        "full fan-out: {} memos recomputed, {} watched leaves changed",
        rt.stats().computations - before,
        tick.changed.len()
    );
    // Sanity: the graph being measured computes the right values.
    let naive = g.naive();
    assert!(g.flat.iter().zip(&naive).all(|(n, v)| n.get(rt) == *v));
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph10k");
    group.measurement_time(Duration::from_secs(4));

    let g = build(1);
    let mut rng = Rng::new(42);
    let mut v = 0i64;
    group.bench_function("single_write_flush", |b| {
        b.iter(|| {
            let s = g.signals[rng.below(g.signals.len())];
            v += 1;
            s.set(&g.rt, v).unwrap();
            black_box(g.rt.flush());
        })
    });

    let mut v = 0i64;
    group.bench_function("full_fanout_flush", |b| {
        b.iter(|| {
            v += 1;
            g.root.set(&g.rt, v).unwrap();
            black_box(g.rt.flush());
        })
    });

    // Localized propagation inside the same 10k-node runtime: one path of
    // 20 memos (a clock tick), and a write cut off after one memo.
    let path = chain(&g.rt, |v| v);
    let cut = chain(&g.rt, |v| v / 1_000_000_000);
    for (src, expect) in [(path, 20), (cut, 1)] {
        let before = g.rt.stats().computations;
        src.set(&g.rt, 1).unwrap();
        g.rt.flush();
        assert_eq!(g.rt.stats().computations - before, expect);
    }
    let mut v = 1i64;
    group.bench_function("narrow_path_flush", |b| {
        b.iter(|| {
            v += 1;
            path.set(&g.rt, v).unwrap();
            black_box(g.rt.flush());
        })
    });
    let mut v = 1i64;
    group.bench_function("cutoff_flush", |b| {
        b.iter(|| {
            v += 1;
            cut.set(&g.rt, v).unwrap();
            black_box(g.rt.flush());
        })
    });

    group.bench_function("idle_flush", |b| b.iter(|| black_box(g.rt.flush())));
    group.bench_function("idle_check", |b| {
        b.iter(|| black_box((g.rt.is_idle(), g.rt.next_deadline())))
    });

    // The same graph once something declares a write edge: the rank map is
    // no longer empty, so every recompute that changes its sources ranks
    // the node after them. One handler writes a layer-0 signal (declared
    // with `writes_to`, as the compiler does) and 100 effects read the
    // written cell, so ranks reach into the graph.
    let r = build(1);
    let trigger = r.rt.signal(0i64);
    let written = r.signals[0];
    let writer = r.rt.effect(move |rt| written.set(rt, trigger.get(rt)?));
    r.rt.reads_from(writer.id(), &[trigger.id()]).unwrap();
    r.rt.writes_to(writer.id(), written.id()).unwrap();
    for _ in 0..100 {
        let e = r.rt.effect(move |rt| written.get(rt).map(|_| ()));
        r.rt.reads_from(e.id(), &[written.id()]).unwrap();
    }
    r.rt.flush();
    assert!(r.rt.rank(writer.id()) < r.rt.rank(written.id()));
    let mut rng = Rng::new(42);
    let mut v = 0i64;
    group.bench_function("ranked_single_write_flush", |b| {
        b.iter(|| {
            let s = r.signals[1 + rng.below(r.signals.len() - 1)];
            v += 1;
            s.set(&r.rt, v).unwrap();
            black_box(r.rt.flush());
        })
    });
    let mut v = 0i64;
    group.bench_function("ranked_full_fanout_flush", |b| {
        b.iter(|| {
            v += 1;
            r.root.set(&r.rt, v).unwrap();
            black_box(r.rt.flush());
        })
    });
    // The handler write: the logic clock moves 100 ms per iteration, so
    // the handler stays under the 30 writes/s guard (else this measures
    // the throttled path, which writes nothing).
    let mut v = 0i64;
    let mut t = r.rt.now();
    group.bench_function("ranked_handler_write_flush", |b| {
        b.iter(|| {
            v += 1;
            t += Duration::from_millis(100);
            trigger.set(&r.rt, v).unwrap();
            black_box(r.rt.tick(t));
        })
    });
    let before = r.rt.stats();
    trigger.set(&r.rt, -1).unwrap();
    r.rt.tick(t + Duration::from_millis(100));
    let after = r.rt.stats();
    assert_eq!(r.rt.untrack(|rt| written.get(rt)).unwrap(), -1);
    assert!(
        after.effect_runs - before.effect_runs > 100,
        "the readers ran"
    );
    assert_eq!(after.reruns, before.reruns, "declared: every sink once");
    assert_eq!(after.learned_edges, 0);

    group.sample_size(10);
    group.bench_function("build", |b| {
        b.iter_batched(|| (), |()| black_box(build(3)), BatchSize::PerIteration)
    });
    group.finish();
}

fn main() {
    report_memory();
    let mut c = Criterion::default().configure_from_args();
    benches(&mut c);
    c.final_summary();
}

//! M0 benchmark: a 10k-node reactive graph (mixed fan-in/fan-out, depth
//! 20). Measures single-write propagation, full fan-out and an idle flush,
//! and reports memory per node through a counting global allocator.
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

    group.bench_function("idle_flush", |b| b.iter(|| black_box(g.rt.flush())));
    group.bench_function("idle_check", |b| {
        b.iter(|| black_box((g.rt.is_idle(), g.rt.next_deadline())))
    });

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

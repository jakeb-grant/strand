# Benchmarks

Numbers measured on the dev container. Re-run and update the table when a
change touches a hot path; note the commit.

## Reactive graph, 10k nodes (M0)

`cargo bench -p strand-core --bench graph` (release profile: thin LTO, one
codegen unit). The graph is built by `crates/strand-core/benches/support/graph_builder.rs`
and its shape and values are checked by `crates/strand-core/tests/bench_graph.rs`.

**Graph.** 10,000 nodes: 100 signals, then 19 layers of memos (depth 20).
Each memo reads 1–4 inputs: 70% from the previous layer, 15% from one of
that layer's 4 hub nodes (fan-out up to several hundred readers), 15% from
any earlier node. Layer 1 also reads a global `root` signal, so everything
depends on it. Memo values are wrapping sums (no equality cut-off, the worst
case). The 522 last-layer nodes are watched, as the scene emitter would.

Measured 2026-10-05, Intel Xeon @ 2.10 GHz (4 vCPUs shared with another
build), criterion medians:

| Case | Work per iteration | Time |
| --- | --- | --- |
| Single write + flush | one random signal; ~5,400 memos recompute (this random graph is very connected), ~516 leaves change | 1.44 ms (~265 ns per recompute) |
| Full fan-out + flush | `root` write; ~8,200 memos recompute, 521 leaves change | 2.00 ms (~245 ns per recompute) |
| Idle flush | nothing written | 39 ns |
| Idle check | `is_idle()` + `next_deadline()` | 22 ns |
| Build | create 10k nodes + 522 watches, first compute | 3.4 ms |

**Memory** (counting global allocator in the bench, live bytes after build,
the builder's own bookkeeping subtracted):

| Measure | Value |
| --- | --- |
| Per node, total | 349 B |
| Per node, excluding the benchmark's closure captures | 298 B |
| Allocations per node during build | 5.6 |

Where the ~300 B goes: a 96 B slotmap slot (kind, colour, flags, creation
order, payload pointer, source and observer lists, owner), the reference
counted payload (memo closure box + cached `Result<T, Error>`, ~72 B), and
the heap parts of the source and observer lists (~60 B at this fan-in),
plus 8 B in the owner's list.

**Idle.** An idle flush touches no node (asserted by
`tests/graph.rs::idle_runtime_does_no_work`: zero computations, zero effect
runs, `next_deadline() == None` across 100 idle flushes).

**Notes.**

- Propagation cost is linear in the nodes that actually change; a shell's
  real graphs are far less connected than this random one (a clock tick
  touches a handful of nodes).
- Recomputing allocates nothing in steady state: source lists come from a
  pool and the push phase reuses one stack.
- Not yet tried: inline small vectors for sources/observers (would remove
  ~2 allocations and ~40 B per node).

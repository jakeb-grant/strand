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

Measured 2026-10-05 (core review round 2), Intel Xeon @ 2.10 GHz (4 vCPUs
shared with another build), criterion medians:

| Case | Work per iteration | Time |
| --- | --- | --- |
| Single write + flush | one random signal; ~5,400 memos recompute (this random graph is very connected), ~516 leaves change | 1.47 ms (~270 ns per recompute) |
| Full fan-out + flush | `root` write; ~8,200 memos recompute, 521 leaves change | 1.98 ms (~240 ns per recompute) |
| Narrow path + flush | one signal feeding a chain of 20 memos and one watch, in the same 10k-node runtime (a clock tick) | 1.61 µs (~75 ns per recompute) |
| Equality cut-off + flush | a write whose first memo recomputes to the same value; the 19 memos below it and the watch do no work | 0.68 µs |
| Idle flush | nothing written | 59 ns (round 1: 45 ns; now also drains diagnostics into the `Tick` and checks for a task left ready, to call the wake hook) |
| Idle check | `is_idle()` + `next_deadline()` | 20 ns |
| Build | create 10k nodes + 522 watches, first compute | 3.4 ms |

Round 3 spot check (late `on change` phase, logic-step epoch, released
suspensions): single write 1.45 ms, narrow path 1.63 µs, idle flush 61 ns,
idle check 22 ns, memory 347 B / 5.5 allocations per node: unchanged within
noise. Late `on change` handlers live in a side set, not a node flag, so the
node slot stays the same size.

The "single write" case recomputes about half of this deliberately
over-connected graph, so it measures fan-out twice; the narrow-path and
cut-off rows are what real shell writes look like (the bench asserts that
they recompute exactly 20 and 1 memos).

**Memory** (counting global allocator in the bench, live bytes after build,
the builder's own bookkeeping subtracted):

| Measure | Value |
| --- | --- |
| Per node, total | 347 B |
| Per node, excluding the benchmark's closure captures | 296 B |
| Allocations per node during build | 5.5 |

Watches compare a per-node change counter, not a copy of the watched value,
so a watched prop costs no second copy of its value (text, paints).

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

**Known costs (not yet optimised; revisit with M4's 2,000-row lists).**

- `KeyedVec` finds keys by linear scan, and a mutation copies the whole
  item vector while any `Snapshot` of it is alive (the scene emitter keeps
  one). So `update`/`remove_key` on an n-row list is O(n) twice, and a
  derived `sort_by`/`filter` does O(n) index bookkeeping per diff. Fine for
  shell lists of tens to a few hundred rows; a key→index map plus a chunked
  or persistent vector is the fix for thousands.
- `keyed_diff` (used by `replace_all` and by derived-collection rebuilds) is
  O(n²) in the worst case; an LIS-based diff would make it O(n log n).
- Dependency tracking dedups a computation's reads with a linear scan, so a
  memo reading k distinct nodes costs O(k²) per run (k is 1–4 here; a
  `sum` over hundreds of cells would feel it). Observer and owner lists are
  unlinked by a reverse scan (`swap_remove`), cheap for recent nodes
  (finished handlers) and O(observers) on hubs.

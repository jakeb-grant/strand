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

Wave 1 round 3 spot check (9d93dac: late `on change` phase, logic-step
epoch, released suspensions): single write 1.45 ms, narrow path 1.63 µs,
idle flush 61 ns, idle check 22 ns, memory 347 B / 5.5 allocations per
node: unchanged within noise. Late `on change` handlers live in a side
set, not a node flag, so the node slot stays the same size.

Wave 2 (8a6c5ab, core: sinks from a rank-ordered queue, woken tasks and
events checked before each sink, ranks in a side map), same machine,
medians:

| Case | Time |
| --- | --- |
| Single write + flush | 1.50 ms |
| Full fan-out + flush | 2.07 ms |
| Narrow path + flush | 1.68 µs |
| Equality cut-off + flush | 0.72 µs |
| Idle flush | 49 ns (a lock-free check for woken tasks) |
| Idle check | 21 ns |
| Build | 3.47 ms |
| Memory | 347 B and 5.54 allocations per node (unchanged: no rank is stored for a rank-0 node) |

That is within 2–5% of round 3 on the propagation cases (the cost of a
heap instead of one sort per batch, and of looking for woken handlers
between sinks), with this machine's noise of a few percent.

Wave 2 review round 1 (915a084: declared reads and the provisional phase, foreign
wakes kept apart, write edges in hash sets, held writes indexed, persist
IO thread), same machine, medians (another agent shares the 4 CPUs; runs
vary by up to 10–25% on the µs cases, the numbers are from quiet runs):

| Case | Time |
| --- | --- |
| Single write + flush | 1.56 ms |
| Full fan-out + flush | 2.06–2.19 ms |
| Narrow path + flush | 1.65 µs |
| Equality cut-off + flush | 0.73–0.76 µs |
| Idle flush | 41 ns (the woken-task and persist-failure checks are lock-free atomics) |
| Idle check | 6.7 ns (`is_idle` no longer locks the ready queue) |
| Build | 3.52 ms |
| Memory | 347 B and 5.54 allocations per node (unchanged: "never ran" and declared reads live in side sets) |

Review rounds 2 and 3 (77cf7be, 2eb240e) did not touch the flush hot path
(settings files, persist hand-over, feedback-edge bookkeeping on
`writes_to`/`reads_from`, and `is_idle`, measured above), so the round 1
numbers stood for them.

Wave 2 review round 4 (822905c, which adds the `ranked_*`
cases: `Stats::reruns` / `learned_edges` counting, and `set_sources`
skipping the rank walk when a source list did not change). Same machine,
medians, measured while another agent was building on the shared CPUs:
everything ran about 5–15% slower than the quiet round 1 runs. An A/B on
this commit with the two new counters removed gave the same numbers
within noise (single write 1.76 vs 1.66 ms, full fan-out 2.28 vs
2.35 ms), so the difference is the machine and not the change.

| Case | Time |
| --- | --- |
| Single write + flush | 1.68 ms |
| Full fan-out + flush | 2.45 ms |
| Narrow path + flush | 2.00 µs |
| Equality cut-off + flush | 0.81 µs |
| Idle flush | 44 ns |
| Idle check | 7.4 ns |
| Build | 4.0 ms |
| Memory | 347 B and 5.54 allocations per node |

**Ranked.** The cases above run with an empty rank map, the state before
any handler declares or performs a write. Every real program leaves that
state on its first handler write. `ranked_*` builds the same graph,
then adds one effect that writes a layer-0 signal (declared with
`writes_to`, as the compiler does) and 100 effects that read the written
cell (declared with `reads_from`), so ranks reach into the graph.
Measured in the same run:

| Case | Time | vs. unranked |
| --- | --- | --- |
| Single write + flush (a signal other than the written one) | 1.71 ms | +1.5% |
| Full fan-out + flush | 2.47 ms | +1.1% |
| Handler write + tick (the writer, its 100 readers, ~6,900 memos and 520 changed watches; the clock moves 100 ms per iteration to stay under the 30 writes/s guard) | 2.16 ms | n/a |

The bench asserts that the handler write runs every sink once
(`reruns` unchanged) and learns no edge. Before `set_sources` skipped the
rank walk for an unchanged source list, review round 4 measured the
ranked fan-out at 3–10% over unranked (2.33–2.42 ms vs 2.18–2.35 ms).

Wave 2 review round 6 (4440bd2: event deliveries and woken tasks are
items of the ranked queue instead of being checked before every sink;
`set_reloaded` marks; strict-edge reporting off by default). Full run,
same machine, medians, under load from the other agent:

| Case | Time |
| --- | --- |
| Single write + flush | 1.72 ms |
| Full fan-out + flush | 2.38 ms |
| Narrow path + flush | 2.21 µs |
| Equality cut-off + flush | 0.93 µs |
| Idle flush | 52 ns |
| Idle check | 7.9 ns |
| Ranked single write + flush | 1.92 ms |
| Ranked full fan-out + flush | 2.53 ms |
| Ranked handler write + tick | 2.21 ms |
| Build | 3.96 ms |
| Memory | 347 B and 5.54 allocations per node (unchanged) |

An A/B on a quieter machine right after (criterion baseline of the
previous commit, d3e29a5, then this one): narrow path 2.02 → 1.99 µs and
cut-off 815 → 823 ns (unchanged within noise), idle flush 44.9 → 50.0 ns
(+5 ns: the flush now drains the ready and event lists into its heap and
clears the reload marks; still far below a frame).

Wave 2 review round 7 (2ecfb76: each listener delivered at its own
rank, handler reads tracked; b2a677a: persist queue indexed by file). The review asked about the
ranked cases of round 6 (1.92 ms ranked single write against 1.72 ms
unranked, +12%, where round 4 had +1.5%), which round 6's A/B had not
covered. One session, back to back, criterion baseline of d3e29a5, then
round 6 (5542c70) and this round against it, medians:

| Case | d3e29a5 | Round 6 | Round 7 |
| --- | --- | --- | --- |
| Single write + flush | 1.75 ms | 1.75 ms | 1.83 ms |
| Full fan-out + flush | 2.38 ms | 2.47 ms | 2.42 ms |
| Narrow path + flush | 1.94 µs | 2.06 µs | 2.00 µs |
| Equality cut-off + flush | 802 ns | 834 ns | 838 ns |
| Idle flush | 45.3 ns | 49.1 ns | 49.1 ns |
| Idle check | 8.0 ns | 7.6 ns | 11.1 ns |
| Ranked single write + flush | 1.78 ms | 1.84 ms | 1.78 ms |
| Ranked full fan-out + flush | 2.36 ms | 2.54 ms | 2.39 ms |
| Ranked handler write + tick | 2.13 ms | 1.99 ms | 2.10 ms |
| Build | 4.11 ms | 4.18 ms | 4.40 ms |
| Memory | 347 B, 5.54 allocations per node | | 347 B, 5.54 allocations per node |

Ranked against unranked in the same run: +1.5% (d3e29a5), +5.7%
(round 6), −2.7% (round 7), so ranks cost what round 4 measured and the
+12% of round 6's table was the load at the time: the ranked heap holding
deliveries and tasks costs nothing measurable when there are none (the
bench has no listeners), and round 6's own ranked cases moved by −6% to
+7% against d3e29a5, both ways, with the noise of the shared machine.
Round 7's idle check is about 3 ns slower (`is_idle` also looks at the
listeners waiting for delivery).

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

**Known costs.**

- Dependency tracking dedups a computation's reads with a linear scan, so a
  memo reading k distinct nodes costs O(k²) per run (k is 1–4 here; a
  `sum` over hundreds of cells would feel it). Observer and owner lists are
  unlinked by a reverse scan (`swap_remove`), cheap for recent nodes
  (finished handlers) and O(observers) on hubs.

## Keyed collections, 2,000 rows (wave 2)

`cargo bench -p strand-core --bench keyed` (`crates/strand-core/benches/keyed.rs`).
Rows are `{ id: u32, title: Rc<str>, score: i64 }` (a VM value's shape:
cheap to clone). The chain case is a 2,000-row cell feeding
`filter_with(query) → sort_by(score) → take(50)` and `map(title)`, with an
emitter that holds the last snapshot of each and reads `diffs_since` every
tick (so every write to a list copies it once: the snapshot shares it).

Measured 2026-10-05 (wave 2, core, 1cf4c18), same machine, criterion medians.
"Before" is wave 1 (linear key scans, O(n²) `keyed_diff`):

| Case | Before | After |
| --- | --- | --- |
| `KeyedVec::get` by key | 361 ns | 9.8 ns |
| `KeyedVec::update` by key (no snapshot alive) | 338 ns | 24.5 ns |
| `keyed_diff`, identical keys | 58.6 µs | 4.7 µs |
| `keyed_diff`, one item moved across the list | 1.35 ms | 76 µs |
| `keyed_diff`, 10 removes + 10 inserts | 76.5 µs | 38 µs |
| `keyed_diff`, reversed | 1.49 ms | 91 µs |
| `keyed_diff`, shuffled | 1.47 ms | 157 µs |
| Update one row + flush (chain) | 20.5 µs | 20.0 µs |
| Remove one row + push one + flush (chain) | 24.8 µs | 25.7 µs |
| Move one row + flush (chain) | 24.7 µs | 25.3 µs |
| Filter query change + flush (2,000 → 1,333 rows reshuffled) | 706 µs | 346 µs |
| `keyed_memo`, one row of a plain list changed + flush | 105 µs | 50 µs |
| `keyed_memo`, plain list rotated by one + flush | 1.47 ms | 122 µs |

Review round 1 re-run (915a084: held keyed copies rebase instead of being
superseded; the in-place path is unchanged), same machine: get by key
10.2 ns, update by key 24.9 ns, `keyed_diff` identical 5.2 µs / one move
76 µs / 10+10 39 µs / reversed 97 µs / shuffled 172 µs, chain update
20.0 µs, remove + push 26.0 µs, move 25.7 µs, filter query change 358 µs,
`keyed_memo` one row 52 µs, rotated 131 µs: unchanged within noise.

The bench is not gated in CI; the scaling is. `crates/strand-core/tests/keyed_scaling.rs`
runs in `cargo test` and fails if lookups or diffs regress to scans:
hash plus `eq` calls per row of `keyed_diff` and `keyed_memo` at 2,000 and
16,000 rows (update, insert + remove, reversed, shuffled: 1.0-4.1 per row
measured, gate 8), key work per `get`/`index_of`/`contains_key` (mean gate 8, 8 and 2.5, each lookup at most 24,
also with every index entry stale by one), and a shuffled `keyed_diff`'s
time at 16,000 rows over 2,000 (best of 7: about 10x measured, gate 32x;
O(n²) is 64x). A diff is O(n) in key work for every shape and O(n) in
index work when no survivor changes order; a reordering costs O(n log n)
index work (longest increasing subsequence, Fenwick tree).

Review round 6 re-run (4440bd2; the review asked for a re-run after the
write-path changes of 77cf7be and 822905c, which this includes), as an
A/B against d3e29a5 on the same machine, back to back, medians (before →
after): get by key 11.2 → 11.5 ns, update by key 26.6 → 26.7 ns, chain
update 22.9 → 22.7 µs, remove + push 28.6 → 29.3 µs, move 28.3 →
29.1 µs, filter query change 410 → 401 µs, `keyed_memo` one row 58.5 →
58.4 µs, rotated 144 → 143 µs; handler writing 500 / 1,000 / 2,000 /
4,000 cells 43 / 88 / 184 / 429 µs → 42 / 84 / 183 / 445 µs, throttled
60 / 130 / 287 / 798 µs → 59 / 122 / 311 / 822 µs. Unchanged within
noise (changes go both ways, −12% to +12%, on cases this round did not
touch). Against the round 1 table the machine itself is slower today:
`keyed_diff`, whose code has not changed since 915a084, measured
5.6 µs identical / 102 µs one move / 69 µs 10+10 / 127 µs reversed /
210 µs shuffled in the same session (round 1: 5.2 / 76 / 39 / 97 /
172 µs).

### One handler writing a cell per row

`for r in rows { r.seen = tick }`: one effect writing n signals per flush
(`handler writing n cells` in `benches/keyed.rs`), after its write edges
are learned. "Write + tick" ticks at 10 Hz of logic time (under the
guard); "throttled" never moves the clock, so past 30 writes a second
every write is held.

| n | Write + tick | Throttled write + flush |
| --- | --- | --- |
| 500 | 38 µs | 55 µs |
| 1,000 | 76 µs | 110 µs |
| 2,000 | 175 µs | 265 µs |
| 4,000 | 389 µs | 761 µs |

Linear. Before review round 1 every write checked the writer's known
edges with a linear `Vec::contains` (the review measured 95 µs, 261 µs,
922 µs and 3.1 ms for 500 to 4,000 cells, about 3.4x per doubling), and a
throttled handler's held writes were a list scanned per write (9.7 ms at
2,000, 39 ms at 4,000 in this bench). Write edges are now hash sets, the
learn queue is deduplicated with a set, held writes are indexed by
`(cell, writer)`, and a rate window keeps only its newest 31 attempts.

What changed:

- `KeyedVec` keeps a key → position map. Inserts and removals never
  re-index the items after them: an entry may go stale, keys are unique so
  `items[p].0 == key` proves it right, and a stale one is found by
  searching outward (one step per insert or removal before the item) and
  fixed. A lookup is O(1) for a fresh entry, else O(its drift since it
  was last looked up), bounded by the list length; a mutation is an O(n)
  `memmove`.
  The single-row chain cases are dominated by copying the 2,000-row lists
  the emitter's snapshots share (about 4 µs per list) and `sort_by`'s O(n)
  index loops. A `KeyedVec` clone shares the map; writing while a clone is
  alive copies it (O(n)), so the VM reads through `KeyedSignal::with`,
  `with_untracked` or `get_key` instead of holding a clone.
- `keyed_diff` trims the common prefix and suffix, then matches keys with a
  hash map (foldhash), keeps the longest increasing run of survivors in
  place (LIS) and moves every other survivor once, with a Fenwick tree for
  indices: O(n) without reordering, O(n log n) with it, and the fewest
  moves (property-tested against the LIS length).
- A derived collection that receives a bulk change (more than 128 diffs and
  at least a quarter of its source) rebuilds and publishes a keyed diff of
  its output instead of feeding each diff through `sort_by`.

Every case is far inside a 16 ms frame; scrolling itself writes nothing to
the list.

## Persisted cells, 2,000 rows (wave 2 review round 7)

`cargo bench -p strand-core --bench persist`
(`crates/strand-core/benches/persist.rs`): a list whose items each persist
a field under an instance-qualified path (`list[<i>].x`), files present,
mounted and unmounted in one runtime (a live reload, or a long list
scrolled in and out). Mount creates the cells (reading every file) and
flushes; unmount disposes them and flushes. Medians, review round 7
(b2a677a), A/B against the previous commit (2ecfb76) on the same machine:

| Rows | Mount before | Mount after | Unmount before | Unmount after |
| --- | --- | --- | --- | --- |
| 500 | 22.7 ms | 2.9 ms | 10.3 ms | 0.62 ms |
| 1,000 | 65.7 ms | 5.9 ms | 36.9 ms | 1.36 ms |
| 2,000 | 132 ms | 13.9 ms | 128 ms | 3.1 ms |

Before, every load, claim, release and write scanned the store's queue
(a list holding a touch per claimed file) under the lock the IO thread
takes: quadratic, 8 frames to unmount 2,000 rows. The queue is now
indexed by file and a claim or release no longer queues a touch for a
file the store touched or wrote in the last day: linear. Mount is now
dominated by reading 2,000 files on the logic thread (about 7 µs each),
a frame for 2,000 rows; unmount is under a frame.

## Glitch-report rings (report)

`CI_JOB=budgets scripts/container/run.sh ci` (release), 2026-10-20, on
the report branch's binary (recording on, the default, in every run):

| Measure | Figure |
| --- | --- |
| The rings' size (`strand_core::flight`, input 155,648 B + changes 106,496 B; replayfix 2026-10-21, from 180,224 + 81,920) | 262,144 B |
| `the_replay_rings_cost_at_most_their_size`: anonymous PSS (`Pss_Anon`) on minus off, both rings wrapped by clicks | 264 to 268 kB over five runs (bound 288 KiB); 260 kB with replayfix's sizes (2026-10-21, one run) |
| The same on the whole PSS (not gated: shared libraries' share moves) | 224 to 541 kB |
| Design bar, two 2560×1440 outputs, warm device (lavapipe) | 200,251 kB (ceiling 247,808) |
| Design bar, cold | 41,269 kB (ceiling 49,152) |
| Full shell, launcher, toasts and OSD up, warm | 270,680 kB |
| Full shell with 161 desktop entries, cold | 96,134 kB (target 77,824 warns; ceiling 106,496) |
| `.text`, default build (GPU backend) | 26,033,559 B (gate 26,214,400) |
| `.text`, CPU-only | 22,045,783 B (gate 22,282,240) |

The gates were raised on 2026-10-22 at the owner's word, after this
report: `main` at 091c7f64 measured 26,207,698 B (GPU) and 22,218,514 B
(CPU-only) in GitHub CI's budgets job (run 39691736543), and the gates
are 26,738,688 and 22,806,528 B (decisions.md growth).

No idle-wakeup window in budgets.rs changed: nothing records on a timer,
and the `strand-report` thread lives only while a report is written.

## Animation frame cost (perf-bench)

`crates/strand-render/tests/anim_bench.rs` (`#[ignore]`d; its header has
the scenes, the env and the run), the timing profile, on the owner's
laptop (Intel Core Ultra X7 358H: performance cores 0–3, efficiency
cores 4–15; Intel Graphics, Mesa ANV 25.2.8), in the dev container,
2026-10-23, at `main` d10f1e4e plus the bench's spans. Each scene: a
warm-up cycle, then its cycles, three repeats on a fresh renderer; a
frame's cost is the render thread's work on the CPU leg, and on the GPU
leg the larger of that and the GPU thread's part (scene build, render,
readback) of the frame. The budget is 8.33 ms (120 Hz). CPU 1× and the
Intel legs ran pinned to the performance cores (`taskset -c 0-3`); CPU
1.5× ran unpinned. The host was busy (load about 5), and repeats of
the same work differ by up to 2×, so the figures are each repeat's
range: p50 / p95 in ms, the lowest repeat first.

| Scene | Size @ scale | CPU 1× | CPU 1.5× | Intel GPU 1× | Intel GPU 1.5× |
| --- | --- | --- | --- | --- | --- |
| morph-card | 940×640 | 4.5–5.3 / 19 | 5.6–10 / 28–42 | 1.7 / 3.5–4.2 | 1.4–2.0 / 2.6–8.0 |
| shared-morph | 940×640 | 6.1–12 / 20–26 | 24–26 / 83–110 | 1.4–1.7 / 2.1 | 1.4–1.7 / 2.0 |
| panel-open | screen | 7.1 / 9.5–24 | 34 / 46 | 2.1–2.8 / 3.3 | 3.2–3.8 / 4.2–5.1 |
| resize-shadow | 940×640 | 3.0–6.7 / 8.2–8.7 | 4.7–13 / 10–16 | 1.6–1.8 / 2.2–2.7 | 1.8 / 2.5 |
| resize-radius | 940×640 | 3.2–9.0 / 11–12 | 21–22 / 28–32 | 1.7–2.5 / 3.8–4.2 | 2.8–6.4 / 7.5–8.5 |
| backdrop-blur | screen | 5.5–5.9 / 8.4–10 | 21–46 / 27–55 | 2.5–3.7 / 11.4–11.7 | 25.3 / 27–72 |
| glass | 940×640 | 3.9–4.0 / 15 | 2.6–3.8 / 12 | 1.4–2.0 / 6.7 | 1.7–2.2 / 14.5–15 |
| crossfade | screen | 8.3–8.9 / 27 (max 65) | 13.4–13.6 / 40 (max 94) | 2.5–3.4 / 4.1–4.6 (max 37) | 3.3–4.4 / 5.3–5.9 (max 52) |
| theme-spring | 940×640 | 1.7–4.5 / 3.2–7.0 | 4.0–4.7 / 14–15 | 1.5–1.8 / 2.1–2.4 | 1.8–1.9 / 2.3–2.4 |
| list-scroll | 940×640 | 2.4 / 7.8 | 4.3 / 11–14 | 1.8–2.1 / 2.5–2.8 | 2.2–2.3 / 2.8–3.0 |
| shader | 940×640 | (GPU only) | | 1.0–1.1 / 1.4–1.6 | 1.1–1.5 / 1.7–1.9 |
| goo | 940×640 | 4.1 / 4.4 | 8.9–12 / 9.8–14 | 3.3 / 4.7–6.2 | 6.9–7.0 / 8.0–8.2 |
| letters | 940×640 | 0.48 / 0.49 | 1.1–1.2 / 1.2 | 1.0–1.2 / 1.3–1.6 | 1.3 / 1.6 |
| stagger | screen | 4.1–4.2 / 5.7–9.6 | 9.2 / 14–18 | 1.8–2.5 / 2.6–3.0 | 3.1–3.3 / 3.9–4.0 |
| fade | 940×640 | 2.5–7.8 / 6.8–8.7 | 5.6–5.7 / 18–20 | 2.0–2.1 / 2.6–2.8 | 2.3–2.8 / 2.9–3.5 |
| flip | 940×640 | 4.8–7.1 / 11.4–11.5 | 7.2–8.2 / 21–23 | 1.2–1.8 / 1.9–2.2 | 1.7–2.2 / 2.5–3.0 |
| cover-turn | 940×640 | 0.21 / 0.43 | 0.51–0.53 / 0.62–0.64 | 1.2–1.3 / 1.7–1.8 | 1.8 / 2.1–2.2 |
| cover-mask | 940×640 | 6.7 / 13.3 | 13.3 / 13.3 | 1.8–1.9 / 3.6–3.7 | 3.1–3.3 / 4.9–6.1 |

"Screen" is 1920×1200 at 1 and 2560×1440 at 1.5; a panel at 1.5 is
1410×960 px. cover-mask ran with two repeats. On lavapipe (CI's
device) the GPU leg's figures are the emulated GPU's run (the frame
waits for it): panel-open 10–11 / 14 at 1×, backdrop-blur 25 / 38 and
fade 43–53 / 71–80 at 1.5×; they say what CI's GPU tier costs, not a
GPU's.

Where the time goes (the stage means the table prints):

- **Masks.** A masked group's mask is computed pixel by pixel over the
  whole cell it is drawn in (`layers::cell_mask`; a shape's 120-edge
  outline tested at each of a 256×64 cell's pixels): 6.5 ms a cell
  whatever the shape, so a 40 px cover turning costs 6.7 ms a frame
  (one cell) to 13.3 (two); without the mask, 0.16 ms. The GPU lowers a
  masked group as a CPU pixmap, 1.8 ms (1×) and 3.1 ms (1.5×) of
  lowering a frame. Its damage is under 0.02 Mpx, so it never promotes.
- **Gradients under a transform** are drawn as each cell's part, built
  every frame (`render_gradient_part`): most of stagger's encoding (2.7
  ms at 1×, 5.9 at 1.5×, with 0.07–0.15 Mpx of damage, so it stays on
  the CPU) and shared-morph's.
- **Backdrop and glass on the GPU** are drawn on the CPU and uploaded
  every frame: 17.6 ms of lowering a frame at 1.5× (backdrop-blur).
- **A crossfade's first frame on a GPU surface** repaints the whole
  surface on the CPU for its snapshot and uploads it (25–28 ms and
  2.3–3.7 Mpx): the max.
- **Corner tiles under a radius spring** are built every frame (3.5
  builds a frame; 2.0 ms of preparing at 1×, 6.7 at 1.5×; on the GPU 3.6
  ms of lowering at 1.5×, built and uploaded).
- **Goo** contours its field on the CPU every frame (flatten 2.3 ms at
  1×, 3.5–6.8 on the GPU leg, 30–64 k allocations a frame) and the GPU
  uploads its 0.3–0.7 Mpx raster.
- **Large CPU frames** are vello_cpu's fine raster on one thread (crossfade
  9.3 ms, panel-open 6–12.7 ms of fine): a surface with a device moves
  to the GPU at its first large frame, so this is what the first frame
  and a machine without a GPU pay.
- **First frames** (each scene's warm-up cycle on a fresh renderer): up
  to 35–92 ms on the Intel GPU (pipelines, textures, uploads), 10–66 ms
  on the CPU.

Known costs checked: a resizing box with a shadow damages 0.50 of its
0.60 Mpx; a radius spring builds corner tiles each frame; a GPU frame
sends full damage; an animation whose frames damage under 0.2 Mpx stays
on the CPU however slow (stagger, glass at 1×, cover-mask); text under
scale and opacity springs is not re-shaped or re-rasterised (its glyphs
come from the atlas, sampled; text wait is 0) but its group is
re-composited every frame.

## Animation frame cost after perf-cpu-raster

The same bench and host as above, 2026-10-24: `laptop/perf` d85e5f96
(before) against perf-cpu-raster's head (after, with the review's
fixes), each built with the `timing` profile. Repeats on this host are
bimodal: the stages this branch does not touch (apply, layout, diff)
move by up to 3× between runs of the same binary, and a single
before-and-after pair once inverted the wins. So the legs are
interleaved, before, after, before, after (two pairs, three repeats
each), every leg pinned to the performance cores (`taskset -c 0-3`, so
a large paint draws on four threads), and each cell is p50 / p95 in ms,
before → after, the range across all six repeats. The untouched stages
are the control: within 1.5× between the legs of each pair, except at
1.5× in morph-card, resize-shadow, resize-radius and flip, where they
are 0.04–0.25 ms a frame and higher after in both pairs (a steady
difference, not a host state flipping). lavapipe is CI's emulated GPU
on the same four cores; Intel is the laptop's GPU through `gpu.sh`.

| Scene | CPU 1× | CPU 1.5× | lavapipe 1× | Intel GPU 1× |
| --- | --- | --- | --- | --- |
| morph-card | 5.2–5.8 / 16–18 → 2.4–2.5 / 6.0–6.7 | 5.4–5.5 / 34–42 → 2.3–2.5 / 14–15 | 4.4–4.5 / 12–13 → 4.3–4.4 / 12–13 | 2.0–2.1 / 3.4–4.0 → 2.0–2.1 / 3.0–3.5 |
| shared-morph | 11 / 23–25 → 1.2–1.3 / 4.2–4.4 | 19–24 / 32–33 → 2.6–2.7 / 8.5–9.0 | 4.0–4.3 / 12–13 → 4.0–4.2 / 12–13 | 1.8–2.0 / 2.2–2.4 → 1.8–2.0 / 2.3–2.4 |
| panel-open | 7.0–7.1 / 22–25 → 4.5–4.7 / 5.7–6.0 | 12 / 34–40 → 2.7–2.8 / 9.7–13 | 6.0–6.5 / 20 → 6.3–6.4 / 20–21 | 3.4–3.5 / 4.0–4.5 → 3.4–3.5 / 4.0–4.6 |
| resize-shadow | 5.6–6.4 / 7.3–9.5 → 2.1–2.2 / 3.2–3.5 | 7.4–9.8 / 15–16 → 4.2–4.3 / 5.9–6.2 | 5.2–6.1 / 8.7–8.9 → 5.4–5.7 / 8.6–9.2 | 2.0–2.1 / 2.4–2.8 → 2.0–2.2 / 2.3–2.5 |
| resize-radius | 7.5–8.0 / 10–11 → 2.9–3.1 / 3.9–4.4 | 6.9–7.0 / 21–22 → 4.4–5.4 / 7.2–8.7 | 4.2–4.5 / 14–15 → 4.1–4.5 / 14–15 | 3.0–3.2 / 3.9–4.1 → 2.4–2.5 / 2.8–3.0 |
| backdrop-blur | 5.0–5.3 / 7.7–9.2 → 1.9–2.1 / 9.5–13 | 16 / 45–49 → 14 / 40–43 | 11–12 / 30–32 → 11–12 / 27–32 | 3.3–3.5 / 11–12 → 3.4–3.5 / 7.7–8.8 |
| glass | 3.5–3.6 / 13–15 → 1.3–1.5 / 11–13 | 5.3–5.5 / 10 → 2.1–2.2 / 8.8–11 | 4.8–4.9 / 16–17 → 4.8–5.0 / 15–17 | 2.2–2.3 / 17–21 → 2.1–2.3 / 13–14 |
| crossfade | 8.8–8.9 / 23–25 → 3.4 / 9.2–11 | 14 / 31–33 → 4.9–5.0 / 14 | 21–22 / 24–26 → 21–22 / 24–26 | 3.9–4.2 / 4.8–5.1 → 3.7–3.8 / 4.8–6.7 |
| theme-spring | 4.5–4.8 / 5.8–7.1 → 1.8–1.9 / 2.5–3.0 | 3.8–3.9 / 12–13 → 3.7–3.8 / 4.6–5.2 | 3.8–4.7 / 7.5–8.4 → 3.9–5.0 / 7.4–8.3 | 1.9–2.0 / 2.2–2.5 → 1.9–2.0 / 2.2–2.5 |
| list-scroll | 2.4 / 6.3–6.7 → 0.85–0.87 / 3.0–3.2 | 4.2–4.3 / 10.0–10 → 1.3–1.4 / 4.6–4.8 | 3.5–3.6 / 7.2–7.5 → 3.6 / 7.2–7.6 | 2.0–2.1 / 2.2–2.4 → 2.0–2.1 / 2.2–2.4 |
| goo | 4.0–4.1 / 4.4 → 2.9 / 3.2 | 8.8–9.3 / 9.6–11 → 6.0–6.1 / 6.7–6.8 | 3.2–3.3 / 3.6–3.8 → 3.1–3.2 / 3.7–3.8 | 3.1–3.2 / 3.4–4.0 → 2.2–2.3 / 2.6–3.5 |
| letters | 0.48 / 0.48–0.49 → 0.48 / 0.48–0.49 | 0.85 / 0.86 → 0.38–0.42 / 0.42–0.45 | 1.3 / 1.5–1.6 → 1.3–1.4 / 1.5–1.6 | 1.5–1.6 / 1.7–1.8 → 1.5–1.6 / 1.6–1.8 |
| stagger | 4.2 / 8.9–10 → 1.6–1.7 / 2.2–2.3 | 9.0–9.1 / 13 → 1.0–1.1 / 3.1–3.2 | 4.1–4.3 / 7.4–7.9 → 4.1–4.3 / 7.5–7.8 | 3.2–3.3 / 3.5–4.0 → 3.2–3.3 / 3.4–3.7 |
| fade | 6.4–6.5 / 7.8–8.4 → 2.6–2.7 / 3.5–3.8 | 5.3–5.5 / 17–18 → 2.8–3.8 / 6.2–6.9 | 4.5–4.9 / 15 → 4.5–4.7 / 14–15 | 2.3–2.5 / 2.7–3.3 → 2.4–2.5 / 2.8–3.6 |
| flip | 6.2–6.6 / 10–11 → 2.4–2.6 / 5.5–6.2 | 6.8–9.2 / 16–21 → 4.0–4.2 / 7.9–8.9 | 5.2–5.6 / 8.0–9.0 → 5.2–5.8 / 8.3–8.7 | 2.1 / 2.5–2.6 → 2.1 / 2.5–2.7 |
| cover-turn | 0.20–0.21 / 0.42–0.44 → 0.21 / 0.42–0.44 | 0.51–0.52 / 0.63–0.64 → 0.48–0.50 / 0.51–0.53 | 5.6–5.7 / 7.2–7.8 → 5.6–5.8 / 7.5–7.8 | 1.5–2.1 / 1.7–2.4 → 1.4–1.7 / 1.6–2.3 |
| cover-mask | 6.7 / 13 → 0.26–0.27 / 0.33 | 13 / 13 → 0.42 / 0.43 | 4.9–5.2 / 5.8–6.2 → 5.0–5.1 / 6.1–6.2 | 1.9–2.0 / 2.6–3.9 → 2.2 / 2.3–2.4 |
| shader | (GPU only) | (GPU only) | 3.6–3.7 / 4.1–4.5 → 3.6–3.7 / 4.1–4.4 | 1.0–2.0 / 1.2–2.2 → 1.0–1.6 / 1.2–1.8 |

What moved:

- **Masks:** cover-mask's mask is computed only within its shape's reach
  (13 → 0.33 ms p95 at 1×, 13 → 0.43 at 1.5×).
- **Parallel cells:** the fine raster of a large paint on four threads
  (crossfade p95 23–25 → 9.2–11 ms at 1×, panel-open 22–25 → 5.7–6.0,
  goo 4.4 → 3.2); a paint under 8 cells stays on the render thread.
- **Gradients under a transform** draw each cell's part from the cached
  pixmap (stagger p50 9.0–9.1 → 1.0–1.1 ms at 1.5×; shared-morph's p95
  at 1.5× 32–33 → 8.5–9.0).
- **Corner tiles** under a radius spring build on a thread each
  (resize-radius p95 21–22 → 7.2–8.7 ms at 1.5×; on Intel 3.9–4.1 →
  2.8–3.0 at 1×).
- **Goo** allocates 428 times a frame, not 29,254 (the bench now counts
  the raster's helper threads' allocations too; the other scenes'
  counts rise by about 30 a frame, the threads' start); its frame is
  4.0–4.1 → 2.9 ms p50 at 1×, mostly the parallel raster.
- The GPU legs barely move (a GPU frame's CPU raster is a masked group's
  pixmap or nothing): lavapipe is within its spread everywhere, Intel
  improves where a frame still rasterises on the CPU (goo, resize-radius,
  glass's and backdrop-blur's p95).

Still over 8.33 ms at p95: backdrop-blur at 1.5× on the CPU (its blur:
40–43 ms), glass (an offscreen group rebuilt now and then: 11–13 ms
frames at 1×, 8.8–11 at 1.5×), crossfade at 1.5× (14 ms, its fine
raster on four threads), morph-card at 1.5× (14–15), panel-open at
1.5× (9.7–13), and the first frame of a scene; flip at 1.5× is at the
line (7.9–8.9).
backdrop-blur at 1× moved the other way at p95, 7.7–9.2 → 9.5–13 ms in
both pairs, while its p50 fell 5.1 → 2.0: its slowest frames are its
offscreen group's rebuilds (5.6–19 ms of `offscreen`, serial and not
touched here), which the raster's 7–8 ms frames hid before. The memory
the threads keep is freed when idle (decisions.md perf-cpu-raster,
review): the cold two-monitor bar measured 41,870–42,097 kB, its base's
range.

## Animation frame cost after perf-gpu-retain

The same bench at `laptop/perf-gpu-retain` 5b9d0a1b (2026-10-23, same
laptop, pinned to cores 0–3, the bench lock held): a GPU frame draws
its shadows (`Op::BlurredRect`), its backdrops (`Op::Backdrop`, the pass
over a texture of what is behind, kept while what is behind is
unchanged) and its crossfade snapshot (`Op::Snapshot`, the last
presented frame copied on the GPU) itself, and the device's warm-up
draws one of each. Intel GPU leg, p50 / p95 in ms (each repeat's range),
max and the cold max (the warm-up cycle's worst frame):

| Scene | Scale | Before | After |
| --- | --- | --- | --- |
| backdrop-blur | 1× | 2.5–3.7 / 11.4–11.7, max 37, cold 45 | 2.8–3.0 / 3.5–4.1, max 4.8, cold 7.2 |
| backdrop-blur | 1.5× | 25.3 / 27–72, max 90, cold 92 | 4.4–4.5 / 6.1–6.8, max 8.2, cold 10 |
| glass | 1× | 1.4–2.0 / 6.7, max 21, cold 27 | 2.2–2.3 / 2.7–3.1, max 6.9, cold 6.2 |
| glass | 1.5× | 1.7–2.2 / 14.5–15, max 54, cold 39 | 2.3–2.4 / 3.0–3.2, max 4.2, cold 4.7 |
| crossfade | 1× | 2.5–3.4 / 4.1–4.6, max 37, cold 52 | 3.7–4.0 / 4.7–5.3, max 6.3, cold 8.7 |
| crossfade | 1.5× | 3.3–4.4 / 5.3–5.9, max 52, cold 68 | 4.8–4.9 / 5.9–6.7, max 8.4, cold 20 (85 once) |
| resize-radius | 1× | 1.7–2.5 / 3.8–4.2, max 5.3 | 1.1–1.2 / 1.3–1.4, max 2.1 |
| resize-radius | 1.5× | 2.8–6.4 / 7.5–8.5, max 13 | 2.3 / 2.7–3.0, max 3.7 |
| resize-shadow | 1.5× | 1.8–1.9 / 2.5–2.6 | 1.5–2.2 / 1.8–2.4 |
| morph-card | 1.5× | 1.4–2.0 / 2.6–8.0, max 17, cold 20 | 2.1–2.2 / 2.5, max 4.8, cold 4.3 |
| panel-open | 1.5× | 3.2–3.8 / 4.2–5.1, cold 52 | 2.6–4.4 / 4.7–4.9, cold 16 |
| shared-morph | 1.5× | 1.4–1.7 / 2.0–2.2, cold 57 | 1.9–2.0 / 2.5–2.9, cold 4.9 |
| goo | 1.5× | 6.9–7.0 / 8.0–8.2, max 15 | 5.3–5.4 / 6.2–8.8, max 11 |

Every Intel scene's p95 is now under 7 ms at both scales but goo's at
1.5× (one repeat 8.8; its contour is the CPU's flatten, not this
stream's), and every cold max under 21 ms but the first crossfade at
1.5× (85 ms once, 20 in the other repeats: its first frame is the CPU's,
before the surface promotes). The scenes not listed moved within the
host's noise (±0.5 ms). A backdrop's frame got 0.3–0.6 ms dearer at its
median where it was cheap before: the old path drew the pass on its own
thread and showed the frame before's result, so its cost was never in
the frame (and its pixels were a frame late).

On lavapipe (CI's software device; the frame waits for it) drawing the
blur in the frame shows its whole cost: backdrop-blur 58–72 / 61–90 at
1× and 182–184 / 196–200 at 1.5× (was 11–12 / 18 and 25 / 38–39 with
the pass off the frame), crossfade 1.5× 47–50 / 60–61 (21–32 / 43–48),
panel-open's p95 20–21 / 35 (14–15 / 21–24, its shadows now blurred by
the device). Elsewhere lavapipe is the same or faster: fade 1.5× 43–53
/ 71–80 → 9.0–9.1 / 27–28, flip 1.5× 20–21 / 30–35 → 4.9–5.3 / 15–16,
cover-turn 1.5× 15–40 / 17–80 → 17–18 / 20–21, cold maxima of the fades
318–377 → 147–177 ms. No figure gates on it.

The CPU leg draws through none of this; its run at this head is within
the host's noise of the table above (crossfade at 1×, 8.3–8.5 / 8.5–8.6
max 17, ran on a quieter host than the baseline's 27 / 65).

### After the perf-gpu-retain review

The review's fixes (2026-10-23; decisions.md, perf-gpu-retain: the
review's six findings): a backdrop's key hashes the groups around what
is behind it, a small backdrop blur is drawn in the frame at the CPU's
reduction (a quarter at 1×'s 520×380 card, which is under 0.2 Mpx; the
1.5× card is over it and stays full size), and a backdrop's pass steps
draw into the surface's texture pool. Same laptop, Intel GPU leg
(`scripts/container/gpu.sh`, `STRAND_STRICT_GPU=1`), cores 0–3, the
bench lock held exclusively; before is the perf-bench baseline
d85e5f96, rerun in the same session. p50 / p95 in ms (each repeat's
range), max and cold max:

| Scene | Scale | Before (d85e5f96) | After the review |
| --- | --- | --- | --- |
| backdrop-blur | 1× | 3.4–3.5 / 11.2–11.6, max 51, cold 44 | 3.1–3.3 / 4.1, max 5.0, cold 7.5 |
| backdrop-blur | 1.5× | 25.3–25.5 / 43–52, max 102, cold 95 | 4.1–4.5 / 5.3–6.7, max 7.8, cold 13 |
| glass | 1× | 2.1–2.2 / 16.4–17.5, max 32, cold 33 | 1.9–2.2 / 2.6–2.7, max 3.6, cold 5.2 |
| glass | 1.5× | 2.3–2.7 / 14.6–14.7, max 47, cold 54 | 2.3–2.6 / 3.1–3.5, max 4.9, cold 7.1 |
| crossfade | 1× | 3.9 / 5.0, max 34, cold 48 | 3.0–4.2 / 3.8–5.5, max 8.5, cold 8.7 |
| crossfade | 1.5× | 4.7–5.8 / 6.6–7.2, max 53, cold 65 | 4.5–5.5 / 5.4–7.0, max 7.6, cold 20 |
| resize-radius | 1× | 1.9–3.1 / 3.8–4.2, max 5.4 | 1.8–1.9 / 2.1–2.4, max 3.1 |
| resize-radius | 1.5× | 2.4–6.2 / 7.1–8.0, max 12 | 2.2–2.4 / 2.8–3.2, max 4.1 |
| cover-turn | 1.5× | 1.7–2.2 / 2.2–2.5, max 2.9 | 1.6–2.0 / 1.8–2.5, max 2.9 |

The 1.5× column is two runs of three repeats, interleaved with two of
the head before the review (3e4b7c67), which measured the same within
the host's noise (backdrop-blur 4.4–4.5 / 6.3–6.7, max 7.6; crossfade
max 8.2–8.9): the review's fixes cost nothing measurable at 1.5×, and at
1× the quarter-scale blur trims backdrop-blur's tail (p95 4.1, max 5.0,
against 3.7–6.6 and 7.3 for 3e4b7c67 in the review's run). A third
1.5× run on a slow spell of the host was 20–50% slower in every scene,
resize-radius (which none of this touches) included, and is left out.

Open: a crossfade's worst warm frame is still about the 8.33 ms budget
at both scales (7.0–8.9 ms over these runs, one frame over in some),
and its cold first frame at 1.5× is 15–20 ms, 85 ms once. The other
scenes here keep every frame under the budget, warm and cold, but
backdrop-blur's cold first frame at 1.5× (11–13 ms).

## Animation frame cost after the review (perf)

Both streams merged on `laptop/perf` (perf-cpu-raster, then
perf-gpu-retain on top; decisions.md perf, the merge), 2026-10-24.
Before is d85e5f96 (`main` d10f1e4e and the bench, no change to what
it measures; built with this head's bench file, which only counts the
raster's helper threads too). Both built with the `timing` profile,
interleaved as two pairs (before, after; then after, before), three
repeats each, every leg pinned to cores 0–3 with the bench lock held
exclusively: the CPU and lavapipe in `scripts/container/run.sh`, the
Intel GPU (PTL, Mesa 25.2.8) through `gpu.sh` with
`STRAND_STRICT_GPU=1`. Each cell is the p95 in ms, before → after, the
range over the six repeats (a frame's cost: the render thread's work on
the CPU, the longer of the two threads' on a GPU). The untouched stages
(apply, layout, diff) stayed within 1.5× between the two trees in 205
of 212 scene-pair cells; the 7 others are the 1.5× CPU runs of
morph-card, resize-shadow, resize-radius and flip, 0.04–0.27 ms a frame,
as perf-cpu-raster found.

| Scene | CPU 1× | CPU 1.5× | Intel 1× | Intel 1.5× | lavapipe 1× | lavapipe 1.5× |
|---|---|---|---|---|---|---|
| backdrop-blur | 8.1–11 → 9.2–13 | 46–51 → 40–44 | 11–12 → 4.0–4.5 | 57–64 → 6.2–6.8 | 23–31 → 24–32 | 28–29 → 174–175 |
| cover-mask | 13 → 0.33 | 13 → 0.42–0.43 | 2.1–4.3 → 2.2–2.4 | 3.4–4.6 → 3.0–3.1 | 5.8–6.0 → 6.3–6.6 | 11 → 12 |
| cover-turn | 0.42–0.44 → 0.43–0.44 | 0.63–0.64 → 0.51–0.52 | 1.9–2.5 → 1.3–1.7 | 1.9–2.6 → 1.9–2.0 | 7.5–7.7 → 7.2–7.7 | 16–19 → 15–19 |
| crossfade | 22–25 → 9.3–10 | 31–35 → 14 | 4.9–6.3 → 4.7–5.1 | 6.2–6.8 → 5.5–6.4 | 25–26 → 25–26 | 42–50 → 41–53 |
| fade | 8.1–10 → 3.5–3.9 | 17–18 → 6.2–7.4 | 2.9–3.1 → 2.7–3.3 | 3.3–3.9 → 3.5–3.9 | 14–15 → 15–16 | 25–26 → 27–28 |
| flip | 10–12 → 5.4–6.0 | 18–19 → 8.1–9.4 | 2.4–3.2 → 2.5–2.9 | 2.9–3.4 → 3.0–3.3 | 8.3–8.8 → 8.1–8.9 | 14–16 → 14–16 |
| glass | 13 → 11–12 | 10 → 8.8–10 | 17–18 → 2.6–3.5 | 14–15 → 3.1–3.9 | 16–18 → 23 | 29–31 → 25–27 |
| goo | 4.4 → 3.2 | 9.6–9.7 → 6.7–6.8 | 3.4–3.6 → 2.6–2.9 | 7.6–7.8 → 5.5–5.7 | 3.7–3.8 → 3.6–3.9 | 7.6–7.9 → 5.9–6.1 |
| letters | 0.49 → 0.49 | 0.86–0.87 → 0.42–0.46 | 1.7–1.8 → 1.8 | 1.6–1.9 → 1.6–1.9 | 1.5–1.6 → 1.5–1.7 | 2.4 → 2.4 |
| list-scroll | 6.4–6.9 → 3.0–3.1 | 10.0–10 → 4.5–4.8 | 2.2–2.6 → 2.3–2.4 | 2.9–3.2 → 2.9–3.1 | 7.0–7.6 → 7.1–7.6 | 7.0–7.6 → 7.0–7.3 |
| morph-card | 16–17 → 6.1–6.5 | 34–37 → 14 | 3.4–4.0 → 2.4–2.7 | 7.3–8.6 → 3.0–3.9 | 12–14 → 13 | 21–24 → 20 |
| panel-open | 22–24 → 5.8–6.6 | 34–39 → 10.0–10 | 4.0–4.7 → 4.0–5.5 | 5.5–5.7 → 5.2–6.3 | 19–21 → 19–21 | 30–35 → 32–33 |
| resize-radius | 10–12 → 4.0–5.9 | 21–23 → 7.4–9.2 | 3.9–4.2 → 2.2–2.4 | 7.4–8.0 → 2.8–3.3 | 13–14 → 9.4–10.0 | 20–21 → 16–17 |
| resize-shadow | 7.2–7.9 → 2.8–3.2 | 15–16 → 5.1–5.9 | 2.4–2.7 → 2.4–2.6 | 2.8–3.3 → 3.1–3.5 | 8.4–9.1 → 9.6–11 | 15–16 → 16–18 |
| shader | — | — | 1.3–1.8 → 1.5–2.2 | 1.5–1.8 → 1.6–1.7 | 4.0–4.4 → 4.1–4.4 | 7.9–12 → 8.0–12 |
| shared-morph | 23–25 → 4.0–4.5 | 30–35 → 8.7–9.1 | 2.2–2.5 → 2.1–2.3 | 2.6–2.9 → 2.7–3.6 | 12–13 → 12 | 12–15 → 14–15 |
| stagger | 9.0–10 → 2.1–2.4 | 13 → 3.1–3.2 | 3.4–3.5 → 3.5–3.9 | 4.8–5.0 → 4.7–5.2 | 7.4–7.7 → 7.7–8.3 | 12–14 → 12–13 |
| theme-spring | 5.8–6.7 → 2.6–3.7 | 12–13 → 4.6–5.6 | 2.4–2.7 → 2.2–2.4 | 2.8–3.3 → 2.8–3.1 | 7.8–8.5 → 7.4–8.1 | 13–15 → 13–14 |

On the Intel GPU the worst warm frame fell where the review aimed:
backdrop-blur 34–40 → 5.2–5.8 ms at 1× and 76–89 → 7.0–8.1 at 1.5×,
crossfade 30–35 → 5.7–6.8 and 47–52 → 6.3–9.0, glass 21–24 → 3.2–5.2
and 44–50 → 3.6–5.8, morph-card at 1.5× 13–17 → 3.8–4.3, resize-radius
at 1.5× 8.5–15 → 3.3–4.5. The cold first frames (the warm-up cycle's
worst): backdrop-blur 41–48 → 5.3–6.8 ms at 1× and 86–92 → 8.3–9.5 at
1.5×, crossfade 44–47 → 8.4–10 and 60–77 → 17–84, glass 24–49 → 3.8–4.4
and 50–55 → 4.2–5.5, shared-morph 27–31 → 4.0–4.7 and 54–61 → 4.7–5.7,
panel-open 32–39 → 8.4–18 and 67–73 → 9.4–17.

Still over the 8.33 ms budget after the review:

- **Intel GPU**, where a large animation is drawn: no scene's p95. Single
  frames: crossfade at 1.5× (1 of 972, max 9.0), panel-open at 1.5× (1
  of 990, max 8.7), goo at 1.5× (14 of 1,800, max 12: its contour is
  the CPU's flatten); the cold first frames of crossfade (8.4–10 ms at
  1×, 17–84 at 1.5×), panel-open (8.4–18 and 9.4–17) and backdrop-blur
  at 1.5× (8.3–9.5).
- **CPU**, where a small animation, or every one without a GPU, is
  drawn: backdrop-blur (p95 9.2–13 at 1×, 40–44 at 1.5×: its blur and
  its offscreen group's rebuilds, serial), glass (11–12 and 8.8–10: an
  offscreen group rebuilt now and then), crossfade (9.3–10 and 14),
  morph-card at 1.5× (14), panel-open at 1.5× (10), flip at 1.5×
  (8.1–9.4), shared-morph at 1.5× (8.7–9.1) and resize-radius at 1.5×
  (7.4–9.2). Their frames damage 0.2 Mpx or more, so where a device
  exists they promote and the Intel columns are what the owner sees, but
  for glass at 1×: its frames damage 0.11 Mpx and its median is 1.4 ms,
  so it stays on the CPU with a device too, its slow frames (an offscreen
  group's rebuild, 11–12 ms at p95) included. That is the deferred
  promotion of small slow animations (decisions.md perf).
- **lavapipe** (CI's software device, never used outside tests): most
  scenes at 1.5× and several at 1×, as before. backdrop-blur at 1.5× is
  now 174–175 ms (28–29 before) because the frame draws its blur instead
  of showing the pass a frame late (decisions.md perf-gpu-retain); glass
  at 1× 16–18 → 23 likewise. No gate reads lavapipe's timings.

Memory, `cargo test --release -p strand --test budgets` in the
container, two runs of each tree interleaved, kB PSS (before → after):
the cold two-monitor bar 41,686 / 41,961 → 41,998 / 41,891; the cold
full shell (161 entries, the launcher open, toasts and the OSD up)
86,745 / 76,153 → 86,926 / 83,490 (over its 77,824 kB target and under
its ceiling on both trees, as on `main`); the warm bar on lavapipe
233,340 / 233,876 → 242,085 / 241,679, over its old 232 MiB target, so
re-based to 242 / 252 MiB (decisions.md perf, warm bar); the warm full
shell 309,282 / 313,226 → 279,623 / 282,392 (161 entries). The warm
bar's 8 MB is lavapipe's compiled code for the pipelines the warm start
now builds (`[anon]` +5.6 MB, the device's `memfd` +2.2 MB); on the
owner's Intel GPU through `gpu.sh` the same bar measured 63,601 →
65,172 kB.

The CPU wins are gated in CI's `timing` job by
`animated_cpu_frames_hold_their_gates` (decisions.md perf-gates).
Unpinned in the container, as CI runs it, five runs: cover-mask median
0.26–0.27 / p95 0.33–0.34 ms at 1× and 0.41–0.42 / 0.43 at 1.5×,
encoding 0.09–0.18 ms; shared-morph 1.54–1.61 / 4.49–4.61 at 1×
(3.2–3.7 / 9.4–10.1 at 1.5×, its frames not gated), encoding
0.11–0.18; stagger 1.90–2.03 / 2.58–2.84 at 1× and 1.50–1.80 /
3.66–3.82 at 1.5×, encoding 0.13–0.14; list-scroll 0.99–1.00 /
3.06–3.11; theme-spring 1.84–1.99 / 2.32–2.93. The same test on
d85e5f96 misses 12 of its gates (cover-mask's encoding 8.5 and 13 ms,
shared-morph's median 9.2 ms at 1×, stagger's median 9.1 ms at 1.5×).

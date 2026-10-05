# M0 exit report

Measured 2026-10-05 on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another build), headless sway 1.x with the pixman renderer,
release build of `strand run --demo` (thin LTO, one codegen unit,
mimalloc). Reproduce with `scripts/m0-exit.sh` (about 3 minutes plus the
benchmark; `--no-bench` skips it). The script exits non-zero when a gate
fails.

## Result

| Gate | Budget | Measured | |
| --- | --- | --- | --- |
| PSS, bar on 2 × 2560×1440 (scales 1.0 and 1.25) | ≤ 34 MB | **21.3–21.4 MB** (21,818 / 21,905 kB in two runs) after two ticks; 20.7 MB right after boot | pass |
| Wakeups between minute ticks | 0 | **0** context switches over :03 → :57, in each of four measured minutes (two runs) | pass |
| Damage per clock tick | ≤ 2,000 px² | **444–456 px²** at 1.0 (37–38×12), **705 px²** at 1.25 (47×15); at most 1,161 px² for both outputs together | pass |

Every frame after boot, including the first tick (age 1, 444 / 705 px²),
is within the damage gate (`target/m0-exit/strand.log` after a run lists them).

![The demo bar at scale 1.0](images/m0-bar.png)

`docs/images/m0-bar.png` is the bar on HEADLESS-1 (1.0, 2560×32);
`docs/images/m0-bar-125.png` the one on HEADLESS-2 (1.25, 2560×40
physical). Both are grim captures from the script run.

## What runs

`strand run --demo` is the hello bar of `design.md` (split; the centre is
`clock.format("%H:%M")`; start and end are static placeholders until the
window and battery services of M3), one layer surface per output, threaded
as `architecture.md` says:

- **Logic thread** (`crates/strand/src/demo/logic.rs`): a `strand-core`
  runtime with the minute as a `Signal<i64>`, a `Memo` formatting it and
  an `Effect` (the scene emitter) appending a `SetProp(text)` to the
  tick's `SceneDiff`. At most one diff per tick goes over a calloop
  channel. The thread sleeps in `epoll` on a `CLOCK_REALTIME` timerfd
  armed at the absolute next minute (`TFD_TIMER_ABSTIME |
  TFD_TIMER_CANCEL_ON_SET`; `crates/strand/src/demo/clock.rs`), plus the
  runtime's own deadline and wake hook (unused by the demo).
- **Main thread**: `strand-surface` owns the Wayland connection and the
  calloop loop; the diff channel and the text worker's ping are sources on
  it; `strand-render` paints through the `SurfaceHost` wrapper
  (`crates/strand/src/demo/host.rs`).
- **Text worker**: `strand-text`, system fonts through fontique; `"Inter,
  sans-serif" 13px 500` resolves to DejaVu Sans here.

## Method

`scripts/m0-exit.sh`:

1. Builds `target/release/strand`, starts sway headless (pixman) with
   HEADLESS-1 at 2560×1440 scale 1, then `swaymsg create_output` and
   `output HEADLESS-2 resolution 2560x1440 scale 1.25`.
2. Runs `STRAND_LOG=damage strand run --demo`, which prints one line per
   committed frame: surface, buffer size, scale, buffer age and the damage
   submitted with `damage_buffer` (already widened by buffer age) with its
   exact area (`Damage::area`, overlaps counted once).
3. **Wakeups**: from second :03 to :57 of a minute (so no tick is inside),
   sums `voluntary_ctxt_switches + nonvoluntary_ctxt_switches` over every
   `/proc/<pid>/task/*/status` at both ends, and prints them per thread.
   Two such windows are measured.
4. **Damage**: the frames logged after each minute boundary; the gate is
   per committed frame (one per surface), and the script also gates the
   largest frame over everything after boot.
5. **PSS**: `Pss:` from `/proc/<pid>/smaps_rollup` after the two ticks,
   plus a per-mapping breakdown from `smaps`.
6. grim captures of both bars, then `cargo bench -p strand-core --bench
   graph`.

Automated tests cover the same ground in `cargo test` without the full
minute: `crates/strand/tests/demo.rs` (the binary on sway with two outputs,
the clock drawn, zero context switches over 2 s of idle),
`crates/strand/src/demo/mod.rs` (`a_minute_tick_repaints_at_most_2000_px2`:
offline, 2560 px wide at 1.0 and 1.25, tick damage ≤ 2,000 px² and the
incremental frames equal a full repaint), `crates/strand/src/demo/clock.rs`
and `logic.rs` (minute boundaries, the timerfd, one diff with one prop per
tick, equality cut-off, idle with no deadline).

## Details

**Memory.** Of 21.8 MB PSS: 15.3 MB anonymous (heap: glyph atlases and
their render-side mirrors at two scales, vello_cpu state, the font
collection, the runtime, thread stacks, mimalloc overhead), 4.5 MB the
binary's own text and data, 1.1 MB the shm buffers (two per surface,
2560×32×4 and 2560×40×4 bytes, shared with sway), the rest libc, fontconfig
caches and DejaVuSans.ttf. That is under the design's 29–34 MB estimate
for "bar only, 2×1440p". A debug build measures about 31 MB, so the gate
is measured on release only.

**Wakeups.** Across a minute tick the whole process does 5 context
switches: 1 on the logic thread (its timerfd), 1 on the text worker
(shaping the new string) and 3 on the main thread (the diff, the shaped
text, presentation feedback and buffer releases, batched by `epoll`).
Between ticks: none, on any thread. The frame loop requests no
callbacks while idle and the logic thread has no deadline besides the
timerfd. (A real session adds compositor events such as pointer motion
over the bar; those are input, not idle wakeups.)

**Damage.** After a tick the changed text's ink box is all that is
repainted and submitted: 37×12 at 1.0, 47×15 at 1.25 (the design
estimated "about 60×20 px"). Steady-state ticks paint into a buffer of age
2, so the damage is this tick's box ∪ last tick's box, the same rectangle.

## Gate missed on the way, and the fix

The first measurement failed the damage gate on the **first tick after
boot**: 81,920 px² at 1.0 and 102,400 px² at 1.25, a full repaint. Root
cause: at boot each surface commits one buffer, which sway's pixman
renderer keeps until the next commit, so the first tick needs a second,
freshly created buffer, whose age is 0 (unknown contents), and the
renderer correctly repaints all of it. Fix in `strand-surface`
(`crates/strand-surface/src/shm.rs`, `b600780`): a newly created buffer
starts as a copy of the buffer holding the newest frame (a ~330 KB
memcpy, once per buffer per size) and reports age 1. The first tick now
damages 444 / 705 px² like every later one. Tested by
`shm::tests::a_fresh_buffer_copied_forward_has_age_one` and the
strengthened `crates/strand-surface/tests/render.rs` (every move, from the
first, repaints ≤ 4 × 20 × 20 px²); the existing
`renders_pixels_with_exact_damage` still checks that every buffer holds
the frame its age claims.

## strand-core 10k-node benchmark

`cargo bench -p strand-core --bench graph` in the same run (criterion
median; graph and cases described in `docs/benchmarks.md`):

| Case | Time |
| --- | --- |
| Single write + flush (~5,400 memos recompute) | 1.42 ms |
| Full fan-out + flush (~8,200 memos) | 1.98 ms |
| Narrow path + flush (20 memos + a watch: a clock tick) | 1.61 µs |
| Equality cut-off + flush | 656 ns |
| Idle flush | 60.6 ns |
| Idle check (`is_idle` + `next_deadline`) | 21.9 ns |
| Build 10k nodes + 522 watches | 3.50 ms |
| Memory | 346.9 B per node, 5.54 allocations per node |

These match the numbers recorded by the core track within noise.

## Open

- The budgets run from `scripts/m0-exit.sh` by hand; the design's "build
  fails above 34 MB on every push" needs the CI container to have sway and
  grim (not yet).
- Measured on sway with pixman only. Compositors that release shm buffers
  right after upload (GPU renderers) reuse the same buffer at age 1, which
  the copy-forward path does not change.
- Monitors are not yet forwarded to logic as the `screens` service, and
  the demo uses one `bar` node on every output rather than one instance
  per monitor; both arrive with the language (M1).
- The bar's colours are literals and its layout absolute until tokens and
  taffy (M2); the start and end texts are placeholders until M3 services.

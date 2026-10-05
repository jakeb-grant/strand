# M0 exit report

Measured 2026-10-05 on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another build), headless sway 1.x with the pixman renderer,
release build of `strand run --demo` (thin LTO, one codegen unit,
mimalloc). Reproduce with `scripts/m0-exit.sh` (about 4 minutes plus the
benchmark; `--no-bench` skips it, `--no-third` the hotplug scenario). The
script exits non-zero when a gate fails. Ranges below are from review
round 1 (one full run plus five more boots with `--ticks 1 --no-third`);
the final-code reading (review round 3, one full run) falls inside every
range and is listed under "Final-code reading".

Units: MB here means MiB, 1,024 kB, as `/proc/<pid>/smaps_rollup`
reports kB and the script's gate is 34 × 1,024 = 34,816 kB (the same
gate `crates/strand/tests/demo.rs` asserts).

## Result

| Gate | Budget | Measured | |
| --- | --- | --- | --- |
| PSS, bar on 2 × 2560×1440 (scales 1.0 and 1.25) | ≤ 34 MB | **21.3–21.6 MB** (21,834–22,066 kB in six runs) after the ticks; 20.6–20.8 MB right after boot | pass |
| Wakeups between minute ticks | 0 | **0** context switches over :03 → :57, in each of seven measured minutes (six runs) | pass |
| Damage per clock tick | ≤ 2,000 px² | **444–456 px²** at 1.0 (37–38×12), **690–705 px²** at 1.25 (46–47×15); 1,134–1,161 px² for both outputs together, also gated | pass |

In all six boots the two boot frames were the only full repaints: every
later frame, the first tick included (age 1), stayed within the gate, and
every tick's damage was centred on its bar. After hotplugging a third
output of another width at the same scale (HEADLESS-3, 1920×1080 at 1.0)
only the new bar painted, and the next tick repainted 456 / 705 / 444 px²
on the three bars, each centred (clock at x = 1262, 1257 and 942).

![The demo bar at scale 1.0](images/m0-bar.png)

`docs/images/m0-bar.png` is the bar on HEADLESS-1 (1.0, 2560×32);
`docs/images/m0-bar-125.png` the one on HEADLESS-2 (1.25, 2560×40
physical); `docs/images/m0-bar-1920.png` the hotplugged HEADLESS-3 (1.0,
1920×32). All are grim captures from the script run.

## What runs

`strand run --demo` is the hello bar of `design.md` (split; the centre is
`clock.format("%H:%M")`; start and end are static placeholders until the
window and battery services of M3), one layer surface per output, threaded
as `architecture.md` says:

- **Logic thread** (`crates/strand/src/demo/logic.rs`): a `strand-core`
  runtime with the minute as a `Signal<i64>`, a `Memo` formatting it and
  the scene emitter, which watches the memo (`rt.watch`) and turns
  `Tick::changed` into a `SetProp(text)` in the tick's `SceneDiff`, the
  path `architecture.md` fixes for the compiler's emitter (and the one the
  benchmark's "narrow path + watch" case measures). At most one diff per
  tick goes over a calloop channel. The thread sleeps in `epoll` on a `CLOCK_REALTIME` timerfd
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
   painted frame: surface, buffer size, scale, buffer age and the damage
   submitted with `damage_buffer` (already widened by buffer age) with its
   exact area (`Damage::area`, overlaps counted once); a `dropped` line
   follows if that frame's commit fails (none did). Boot is over once no
   new frame has been logged for 500 ms.
3. **Wakeups**: from second :03 to :57 of a minute (so no tick is inside),
   sums `voluntary_ctxt_switches + nonvoluntary_ctxt_switches` over every
   `/proc/<pid>/task/*/status` at both ends, and prints them per thread.
   Two such windows are measured.
4. **Damage**: the frames logged after each minute boundary; the gate is
   applied per frame (one per surface) and to the tick's total over all
   outputs, every frame after boot is held to it, and each tick's damage
   must be centred on its bar (within 5% of the width), which catches a
   clock laid out for another width.
5. **PSS**: `Pss:` from `/proc/<pid>/smaps_rollup` after the two ticks,
   plus a per-mapping breakdown from `smaps`.
6. grim captures of both bars.
7. **Third output**: hotplugs HEADLESS-3 at 1920×1080, scale 1.0 (the
   same scale as HEADLESS-1, another width): only the new bar may paint,
   then the next tick must give one centred frame per bar within the
   gate. Then `cargo bench -p strand-core --bench graph`.

Automated tests cover the same ground in `cargo test` without the full
minute: `crates/strand/tests/demo.rs` (the debug binary on sway with two
2560×1440 outputs at 1.0 and 1.25: PSS ≤ 34 MB in a release run
(`cargo test --release`), ≤ 40 MB debug ceiling in a debug run (about
31 MB),
both bars' clock centred and end text at the edge on grim captures, zero
context switches over 2 s of idle, then a hotplugged 1920×1080 output at
1.0 aligned to its own width while the others stay put; it prints a
loud SKIPPED when sway or grim is missing),
`crates/strand-render` (`one_text_on_two_widths_at_one_scale_aligns_on_each`,
`new_surface_of_another_width_waits_for_its_own_layout`,
`stand_in_of_another_width_is_realigned`),
`crates/strand/src/demo/mod.rs` (`a_minute_tick_repaints_at_most_2000_px2`:
offline, 2560 px wide at 1.0 and 1.25, tick damage ≤ 2,000 px² and the
incremental frames equal a full repaint), `crates/strand/src/demo/clock.rs`
and `logic.rs` (minute boundaries, the timerfd, one diff with one prop per
tick, equality cut-off, idle with no deadline, and the real thread loop
on a 200 ms period: one diff per boundary, never early, ending once the
receiver hangs up).

## Details

**Memory.** Of 21.8 MB PSS: 15.3 MB anonymous (heap: glyph atlases and
their render-side mirrors at two scales, vello_cpu state, the font
collection, the runtime, thread stacks, mimalloc overhead), 4.5 MB the
binary's own text and data, 1.1 MB the shm buffers (two per surface,
2560×32×4 and 2560×40×4 bytes, shared with sway), the rest libc, fontconfig
caches and DejaVuSans.ttf. That is under the design's 29–34 MB estimate
for "bar only, 2×1440p". A debug build measures about 31 MB (about 9 MB of
debug-only overhead), so `crates/strand/tests/demo.rs` holds the 34 MB
gate only when it runs in release (`cargo test --release -p strand --test
demo`) and a debug run to a separate 40 MB debug ceiling; this script
measures the release binary.

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

## Missed in review: one bar layout for every width

Review round 1 found two failures the first measurement setup could not
see, both from one cause in `strand-render`: text layouts were cached per
(node, scale), but `center` and `end` alignment happen inside the shaped
line box, whose width is the surface's. The demo shows one bar node on
every output, so:

- two outputs at the **same scale but different widths** (2560 and 1920
  at 1.0, the most common pair) drew whichever width was shaped last on
  both bars: on one, the clock sat off-centre and the end text was drawn
  off-screen. The gate setup (two 2560 outputs at different scales) and
  the old `demo.rs` (two 1280 outputs at different scales) never had two
  widths at one scale.
- at boot, the 1.25 bar's first frame could borrow the 1.0 bar's layout
  (shaped for 2560 logical px against its 2048) as a stand-in while its
  own was shaped; a correction frame followed (3,738 px²), and because the
  next buffer had age 2 the first tick on that output repainted tick ∪
  correction = 3,738 px², over the gate. About 1 boot in 5–10 under load;
  the previous report's clean first tick was a lucky run.

Fix (gate fix in `strand-render`, see `docs/decisions.md`, m0): layouts
are keyed by node, scale and line box width; a surface draws the one for
its own scale and width; a not-yet-painted surface holds its first frame
until that exact layout arrives (up to the first-frame wait, 500 ms in
the demo); a stand-in from another scale or width, drawn only by a
surface that already painted, is resampled and shifted so its alignment
lands where the right one's would; slots no surface wants are pruned.
The script now waits for boot to go quiet before counting, gates the
per-tick total and centring, and runs the third-output scenario; the six
boots above had no correction frame.

Review round 2 changes (no gate number moved):

- **Refresh-rate test.** The strand-surface test
  `commits_lock_to_the_refresh_rate` flaked about 1 run in 3 on an idle
  machine. It derived its bound from wall-clock time × 60. It now reads
  the compositor's presentation timestamps through a recording
  `FakeClock`: each frame is presented on a later refresh than the one
  before, and 100 changes are coalesced. It passed 9 of 9 full
  `--test sway` runs, 3 of them with 3 cores busy.
- **Poisoned text slot.** In strand-render, a text slot the engine
  crashed on (poisoned) no longer keeps its node's layouts for other
  widths as stand-ins (`a_poisoned_slot_keeps_no_stand_ins`).
- **demo.rs.** The 34 MB PSS gate is asserted in release, and a debug run
  is held to a 40 MB debug ceiling. The hotplug step waits for the new
  bar's own 1920x32 frame.

Review round 3 changes (no gate number moved):

- **m0-exit.sh** gates the per-tick total over all three bars after the
  hotplug as well (it already gated each frame and centring).
- **Dropped stand-in.** In strand-render, `prune_texts` marks dirty (and
  clears the cached flatten of) any surface that may have drawn a
  dropped slot as its stand-in, so a poisoned surface stops showing
  glyphs whose layout is gone (`a_poisoned_slot_keeps_no_stand_ins`).

### Final-code reading

`scripts/m0-exit.sh --no-bench` on the round-3 head, all gates passed:

| Reading | Value | Gate |
| --- | --- | --- |
| PSS after boot / after 2 ticks | 21,171 kB / 21,835 kB (20.7 / 21.3 MB) | 34,816 kB |
| Context switches, :03 → :57, two windows | 0 and 0 (21 → 21, 26 → 26) | 0 |
| Tick damage per frame (1.0 / 1.25) | 456 / 705 px² (38×12, 47×15), age 2 | 2,000 px² |
| Tick damage, both outputs | 1,161 px² (both ticks) | 2,000 px² |
| Hotplug of HEADLESS-3 | 1 frame on the new bar only, 0 on the others | 0 |
| Tick on three bars | 456 / 705 / 444 px², 1,605 px² in all, all centred | 2,000 px² per frame and per tick |
| Frames painted but not committed | 0 | — |

## strand-core 10k-node benchmark

`cargo bench -p strand-core --bench graph`, from the first measurement
run (strand-core is unchanged since; criterion
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

- The budgets are checked on every `cargo test` where sway and grim are
  installed. `crates/strand/tests/demo.rs` checks idle and alignment,
  and PSS: 34 MB in release, 40 MB as the debug ceiling. The CI image
  has neither tool, so in CI that test and the strand-surface sway tests
  print SKIPPED, and the design's "build fails above 34 MB on every push"
  does not hold yet. It has to hold before M1 links in the compiler, VM
  and watcher. Proposed to the lead, who owns
  `.github/workflows/ci.yml`: add `sudo apt-get install -y sway grim
  fonts-dejavu-core` before `cargo test`, then `cargo test --release -p
  strand --test demo`. Tracked as an unticked M0 item in
  `docs/features.md`, so M0 is not closed until CI has it.
- Measured on sway with pixman only. Compositors that release shm buffers
  right after upload (GPU renderers) reuse the same buffer at age 1, which
  the copy-forward path does not change.
- Monitors are not yet forwarded to logic as the `screens` service. The
  demo has no render → logic channel, and it uses one `bar` node on every
  output rather than one instance per monitor. That is a demo shortcut,
  not the M1 model; it is tracked as an M1 item in `docs/features.md`.
  The per-width text layouts in render exist because of the shared node.
  Their M2 costs, a reshape per frame for a springing width and a
  correction frame after a reconfigure, are recorded under "Later" in
  `docs/architecture.md`.
- The bar's colours are literals and its layout absolute until tokens and
  taffy (M2); the start and end texts are placeholders until M3 services.

# Performance

Strand aims to be fast when something moves and to cost nothing when
nothing does. This page says how it gets there, and how to measure
your own shell.

<!-- repo-only -->
It also says which numbers CI checks on every push. Each number here
is named with the test that holds it.
<!-- /repo-only -->

## An idle shell does no work

The program is one graph of values (see
[state and reactivity](state.md)). When nothing is written, nothing is
recomputed and no frame is asked for. A service wakes the shell only
as often as what you read from it changes: a clock that shows minutes
wakes it once a minute, and the battery when the battery changes.
Services nobody reads are not running at all.

<!-- repo-only -->
CI holds this on design.md's bar running on real services (UPower,
NetworkManager and logind as mocks on a private bus, a private
PipeWire, under a headless sway): over 10 seconds between two minute ticks, no thread of
Strand's wakes and no frame is drawn, and the next tick wakes it in one
burst. The test is
`the_design_bar_on_the_real_services_keeps_the_budget` in
[crates/strand/tests/budgets.rs](../../crates/strand/tests/budgets.rs).
<!-- /repo-only -->

## Only what changed is painted

Each frame repaints only the area that changed, its **damage**. When
the clock ticks, Strand repaints the clock's text, not the bar. On the
demo bar (`strand run --demo`), 2560 px wide at scale 1 and 1.25, a
minute tick, even one that
changes every digit and the text's width, repaints at most 2,000 px².

<!-- repo-only -->
The test is `a_minute_tick_repaints_at_most_2000_px2` in
[crates/strand/src/demo/mod.rs](../../crates/strand/src/demo/mod.rs).
<!-- /repo-only -->

Layout is just as narrow. `x`, `y`, `scale`, `rotate` and `opacity`
never lay the tree out again, and a size that springs lays out only the
part of the tree under the nearest ancestor whose size stays put (see
[layout](layout.md#what-lays-the-tree-out-again)).

## CPU and GPU

Strand draws on the CPU by default, into shared memory, with exact
damage: that is the cheapest way to draw a bar whose clock ticks. When
a large area animates, the GPU is faster, so a surface moves to it:

- **A surface moves to the GPU** at the first large frame (at least
  200,000 pixels of damage) of an animation, such as a panel opening, a
  scroll or a theme crossfade, or at the third large frame in a row of
  something that animates with no spring, such as a shader's clock.
- **It goes back to the CPU** once it has been settled, with only small
  frames or none, for 3 seconds. A surface that animates again within
  30 seconds of going back waits 15 seconds the next time, so one you
  use every few seconds is not handed back and forth.
- **A small animation the CPU draws slowly** (4 ms or more a frame)
  keeps its surface on the GPU.
- **The GPU device is started with the shell**, off the drawing path,
  and kept, so an animation never waits for it. Where no GPU can exist,
  Strand never asks for one. After 3 lost devices in one run the GPU
  stays off until Strand restarts, and everything is drawn on the CPU.

<!-- repo-only -->
These rules are in
[crates/strand-render/src/promote.rs](../../crates/strand-render/src/promote.rs),
with tests such as `a_panel_opening_is_promoted_at_its_first_frame`,
`a_settled_surface_goes_back_after_the_settle_period` and
`the_third_lost_device_turns_the_gpu_off`.
<!-- /repo-only -->

`strand report` shows each
surface's backend and why.

Strand's own build is held to memory and speed budgets, set in
[design.md](../../docs/design.md#rendering-performance-and-memory-budget).
They catch a change that makes Strand slower or bigger, and are never
a limit on what your shell may do.

<!-- repo-only -->
## What CI holds

The memory figures are PSS, the memory a process really uses with
shared pages split among their users. They are tripwires that catch a
change making Strand bigger, never a limit on your shell: a config
that does more may use more. Each figure warns above its target and
fails the build above its ceiling, in a release build.

| What | Target | Fails above | Test |
| --- | --- | --- | --- |
| design.md's bar on two monitors, no GPU | 42 MiB | 48 MiB | `the_design_bar_with_no_gpu_keeps_the_cold_budget` |
| The full shell (bar, launcher, notifications, OSD, theme) with a desktop of apps, no GPU | 76 MiB | 128 MiB | `the_full_shell_with_no_gpu_keeps_the_cold_budget` |
| design.md's bar with the GPU device warm (lavapipe, CI's software Vulkan) | 242 MiB | 252 MiB | `the_design_bar_on_the_real_services_keeps_the_budget` |
| The full shell with the GPU device warm (lavapipe) | 296 MiB | 324 MiB | `the_full_shell_on_the_real_services_is_measured` |

These tests are in
[crates/strand/tests/budgets.rs](../../crates/strand/tests/budgets.rs).
Most of each warm figure is lavapipe itself, which maps LLVM; the
cold and warm gates hold the same headroom over what they measure, so
growth in Strand's own memory trips both.

GitHub's `timing` job holds the speed gates, on an optimised build:

| What | Gate | Test |
| --- | --- | --- |
| A save to the frame on screen, token edit | 35 ms at p95 | `reload_latency_to_the_presented_frame` in [crates/strand/src/bench.rs](../../crates/strand/src/bench.rs) |
| A save to the frame on screen, markup edit | 50 ms at p95 | the same test |
| A theme swap's work, all its frames together | under 5 ms (median) | `a_theme_swap_is_under_five_milliseconds_of_work` in [theme_swap_bench.rs](../../crates/strand-render/tests/theme_swap_bench.rs) |
| Animated CPU frames (a masked cover, a morph, a scrolled list, a palette spring, …) | median within a 120 Hz frame, p95 within a 60 Hz one | `animated_cpu_frames_hold_their_gates` in [anim_bench.rs](../../crates/strand-render/tests/anim_bench.rs) |
<!-- /repo-only -->

## Measuring your own shell

`strand report` describes the running shell: the process's PSS, the
GPU's status, and for each surface its backend, why it is on the GPU
and its frame times (p50, p95 and p99 over the frames drawn among its
last 600 paints).

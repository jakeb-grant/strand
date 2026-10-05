# M1 exit report

Measured 2026-10-05 on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another agent's builds), rustc 1.97.0, headless sway 1.9 with
the pixman renderer, release builds (thin LTO, one codegen unit,
mimalloc). Every number below comes from a test in the tree that fails
when its gate is missed; the commands to reproduce each are given with
it.

## Result

| Gate (`docs/features.md`, M1 exit) | Budget | Measured | |
| --- | --- | --- | --- |
| Random edits with no panic or blank frame | 10,000 clean | **10,000 edits clean**, each saved in all five editor styles (50,000 saves) into five live pipelines, in 267.6 s | pass |
| Token edit, save → presented frame | p95 ≤ 35 ms | **p95 17.8 ms** (max 25.7) over 100 edits | pass |
| Markup edit, save → presented frame | p95 ≤ 50 ms | **p95 33.9 ms** (max 34.2) over 100 edits: a node removed p95 17.9 ms, a node added 34.1 ms | pass |
| Monitor change on the next frame | next frame | a plugged monitor's bar presented **6.9–13.9 ms** after `wl_output.done` (five plugs; gate: two refresh intervals, 33.3 ms); the logic thread answers every plug, scale change, unplug and replug in its first diff | pass |
| Portal change on the next frame | next frame | not measured: portal settings are not fed into `strand run` yet (M2) | open |

The design's two M1 exit boxes ("10k random edits with no panic or blank
frame", "under 50 ms from save to pixels") are ticked. The benchmark line
under "Live reload" stays open for its portal clause only.

## Reload fuzzer

`crates/strand/src/fuzz.rs::random_edits_through_five_save_styles`
(design.md, "How reload is tested").

**What runs.** Five copies of the `strand run` pipeline, each with the
real `strand-watch` watcher (inotify on directories, 15 ms coalescing,
50 ms after a removal), the compiler worker (the `Loader`: largest
consistent set, held-back files, last good sources), the logic thread
(`Instance::reload`, the error overlay, the IPC socket) and a mirror of
the scene diffs the main thread would paint. No Wayland: a surface is a
scene root. Each copy has its own config directory on tmpfs
(`/dev/shm/strand-fuzz-<pid>/<style>/config`) and saves in one style:

| Style | How a file is saved | Editors |
| --- | --- | --- |
| In place | truncate and write | VS Code |
| Rename | write `.fuzz-save.tmp`, rename over | Helix, atomic saves |
| Backup then rename | rename to `name~`, write anew, delete the backup | Vim `backupcopy=no` |
| Delete and create | unlink, 5 ms, create | some scripts and sync tools |
| Symlink swap | write a new target in a store directory, swap the link | home-manager |

Every edit is saved into all five pipelines; then each must answer.

**The edits.** A model of a three-file config (`theme.strand`: tokens,
an exported `let`, an exported list, the `Chip` component;
`cells.strand`: exported int and text state; `bar.strand`: the bar, its
own `state n`, a text per cell with a click handler, chips, a keyed
`for` over the list, static texts). Each step draws one edit:

| Kind | Edit | Edits in the 10k run |
| --- | --- | --- |
| token | `bar.bg` or `chip.fg` recoloured | 1,351 |
| binding / prop | the exported `let` text; the bar's height | 756 / 570 |
| node-added / node-removed | a static text or a chip | 752 / 750 |
| move | two children swapped, a chip wrapped in or out of a `row` | 440 |
| list | a list entry added, removed or moved (keyed item state) | 543 |
| component-moved | `component Chip` moved between `theme.strand` and `bar.strand` | 746 |
| state-default | default of `n`, of a cell, or of the chip's `on` | 1,863 |
| rename / retype | a cell renamed or retyped (both files change) | 791 / 716 |
| broken | a file saved with a syntax or name error | 677 |
| fixed | a broken file saved back to its last good text | 45 |

1,124 of the multi-file edits were saved as partial saves: first one
file alone (one whose new text does not type-check with the other files'
old text, checked in the test), which must be held back, then the rest.
A broken save is fixed by the next edit, which rewrites that file. Before
each edit the user clicks 0–2 state texts (`n += 1`, a cell `+= 1` or
`+ "x"`, a chip toggled), so the table has values to keep.

**What is asserted.**

- No panic: every logic thread answers every save and joins with `Ok`.
- No blank frame and no leaked surface: after every diff, in every
  pipeline, exactly one bar is shown and every other root is the error
  overlay.
- A broken or partial save is held back (`strand watch` reports it
  held, nothing committed) and the scene does not change.
- After each committed edit the scene (overlay aside) and the token
  table equal a cold boot of the same files with the state the edit
  table keeps written into it: kept, the new default where the cell held
  the old one, reset when renamed or retyped, fresh for a node or list
  entry added. The cells `strand watch` reports reset are the table's.

**Runs.** 10,000 edits (10,887 drawn; draws that changed nothing are
drawn again) with seed `0x5eedf00dcafe0001`, release build: clean in
267.6 s, no save split by the watcher into two loads. The same code in a
debug build before the list edits were added: 10,000 clean in 359 s.
Seeds 7, 12345 and 99991, 400 edits each: clean. Reproduce:

```sh
STRAND_FUZZ_EDITS=10000 cargo test --release -p strand --bin strand \
  random_edits_through_five_save_styles -- --nocapture
```

`cargo test --workspace` runs 60 edits on every push (about 3 s); the
nightly CI job runs 10,000 with `STRAND_FUZZ_SEED` set to the run id, so
each night explores a new sequence and prints the seed that replays it.
The fuzzer checked itself: with the table's default adoption switched
off it fails within a few steps.

The instance-level fuzzers stay as faster, narrower checks:
`crates/strand-compiler/tests/reload.rs::random_edits_land_on_a_cold_boot`
(random line edits of the design's shells, 10,000 in 203 s debug) and
`random_state_edits_follow_the_table`; the nightly job runs both long.

## Save → pixels

`crates/strand/src/bench.rs::reload_latency_to_the_presented_frame`.

**Method.** `strand run`'s threads as `run::run` starts them (watcher,
compiler worker, logic thread, text worker with its waker, `Renderer`
and `SurfaceManager`) against a headless sway, one 2560×1440 output at
60 Hz. The config is design.md's hello bar (a token for its colour, a
split with three texts). A token edit recolours `bar.bg` (the diff is the
token swap alone); a markup edit adds or removes a text node. The time
runs from just before the write (in place) to the
`wp_presentation_feedback.presented` timestamp of the first frame that
carries the edit: the first frame painted with damage and with no text
still being shaped after the diff was applied. Both clocks are
`CLOCK_MONOTONIC` (asserted from `wp_presentation.clock_id`). A
test-only probe on `Host` and a `FrameClock` wrapping
`PresentationClock` see the paint and the presentation; the shipped
binary has neither.

**Numbers** (100 edits of each kind, final code):

| | p95 | max |
| --- | --- | --- |
| token edit → presented | 17.8 ms | 25.7 ms |
| markup edit → presented | 33.9 ms | 34.2 ms |
| of which a node removed | 17.9 ms | |
| of which a node added | 34.1 ms | |
| token edit → painted buffer (no compositor) | 17.9 ms | 18.5 ms |
| markup edit → painted buffer (no compositor) | 17.4 ms | 18.1 ms |

The watcher's 15 ms coalescing (design.md, "The pipeline") is most of
every number; compiling, committing and painting the 2560×40 bar take
the remaining 2–3 ms. A node added costs one refresh more: the frame
that commits it is painted before the text worker has shaped the new
text, and the frame with the text waits for that frame's callback. It is
inside the 50 ms budget; holding a frame for new text the way a first
frame is held (`Renderer::set_first_frame_wait`) should bring it to the
removed node's 18 ms; that is render's call (see Open).

Headless sway presents a commit at its next frame timer, which is why
the token edit is presented about when it is painted. A real monitor
adds the wait for scanout, 0–16.7 ms at 60 Hz (8.3 ms on average), which
keeps the token edit inside 35 ms but leaves little room above the
coalescing window.

**Monitors.** `swaymsg create_output` five times: from the main thread
hearing of the output (`wl_output.done`) to the presentation of its new
bar's first frame, 6.9–13.9 ms (gate: two refresh intervals, the layer
surface's configure round trip and then the next frame).
`crates/strand/src/run.rs::a_monitor_change_is_in_the_next_diff` checks
the logic side exactly: a plug, a scale change, an unplug and a replug
are each wholly in the first diff the logic thread sends after the main
thread tells it.

Reproduce:

```sh
STRAND_LATENCY_ROUNDS=100 cargo test --release -p strand --bin strand \
  reload_latency -- --nocapture --test-threads=1
```

CI runs it (20 edits per kind) on every push and fails the build when a
p95 misses; the nightly job runs 200 per kind. Both tests are ignored in
debug builds, which measure the unoptimised repaint rather than the
design.

## CI

`.github/workflows/ci.yml`:

- Every push: `cargo test --workspace` (60 fuzzer edits through the five
  styles, on `/dev/shm`), `cargo test --release -p strand --test demo`
  (M0's PSS budget), `cargo test --release -p strand --bin strand
  reload_latency` (both latency benches, sway required).
- Nightly (03:17 UTC) and on demand: the fuzzer for 10,000 edits with a
  new seed, the instance-level fuzzers for 10,000 and 2,000, the latency
  benches for 200 edits per kind.

## Open

- Portal changes on the next frame: `strand-watch` reads and follows the
  portal settings, but nothing feeds `system.dark`, `system.accent` and
  `system.contrast` into `strand run` yet (the portal item under M2).
  The benchmark line in `docs/features.md` stays open for this clause.
- A node added is presented one refresh after a node removed (34 vs
  18 ms): new text is shaped after the first frame with the node is
  committed. Within budget; a short hold for new text in
  `strand-render` would remove it.
- On a real 60 Hz monitor the token edit's p95 should be about 18 ms
  plus the scanout wait; it has not been measured on hardware.
- The fuzzer checks "no blank frame" on the scene the main thread
  paints, not on pixels; the M0 and `strand run` sway tests check the
  painted bars.
- Other M1 items not part of the exit gates are open in
  `docs/features.md`: the formatter, the tree-sitter grammar and the
  basic LSP (`strand-dev`), the render parts of `keyframes`, `shader` and
  `canvas`, and the watcher registrations for `.wgsl` files and
  wallpapers.

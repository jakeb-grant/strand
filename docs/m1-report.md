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
| Random edits with no panic or blank frame | 10,000 clean | **10,000 edits clean**, each saved in all five editor styles (50,000 saves) into five live pipelines on two screens, in 883.6 s; one pipeline's frames painted and compared with a cold boot's | pass |
| Token edit, save → presented frame | p95 ≤ 35 ms | **p95 33.5 ms as a monitor would show it** (headless sway 17.8 ms, max 25.8, plus a vblank wait over one refresh), 100 edits | pass |
| Markup edit, save → presented frame | p95 ≤ 50 ms | **p95 33.5 ms as a monitor would show it** (headless 18.1 ms, max 18.6: a node added 18.2, removed 17.7), 100 edits | pass |
| Monitor change on the next frame | next frame | the first frame painted after the shell hears of it shows it: a scale change 1.1–2.3 ms after `wl_output.done`, a plugged monitor's bar 0.9–1.1 ms after its surface's configure (round trip 0.5–0.8 ms), and that frame is the one presented (sway's next frame timer, 7–17 ms later); the logic thread answers every plug, scale change, unplug and replug in its first diff | pass |
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
the scene diffs the main thread would paint, with two screens plugged in
(a `bar` on each). No Wayland: a surface is a scene root. One of the
five (in place) also feeds its diffs to an offline `Renderer` (vello_cpu,
inline text shaping), a surface per surface-kind node as `strand-surface`
would make them, each painted into its own buffer with buffer age 1 so
damage decides what is repainted. Each copy has its own config directory
on tmpfs (`/dev/shm/strand-fuzz-<pid>/<style>/config`, removed when the
run ends, passed or not) and saves in one style:

| Style | How a file is saved | Editors |
| --- | --- | --- |
| In place | truncate and write | VS Code |
| Rename | write `.fuzz-save.tmp`, rename over | Helix, atomic saves |
| Backup then rename | rename to `name~`, write anew, delete the backup | Vim `backupcopy=no` |
| Delete and create | unlink, wait 0–25 ms (random), create | some scripts and sync tools |
| Symlink swap | write a new target in a store directory, swap the link, delete the old target | home-manager |

A file renamed (`mv`) is renamed in every style. Every edit is saved
into all five pipelines; then each must answer.

**The edits.** A model of a three-file config (`theme.strand`: tokens,
an exported `let`, an exported list, the `Chip` component;
`cells.strand`: exported int and text state; `bar.strand`: the surface
(a `bar` on every screen or one `panel`), its `state n`, a timer, a text
per cell with a click handler, chips, a keyed `for` over the list,
static texts). Each step draws one edit, covering the rows of design.md's
"What each edit does" but two: a custom service declaration (a service
needs a D-Bus name, a file, a socket or a permitted command to run) and
anything inside `lock` (deferred only while a session lock is shown);
their rows have their own tests
(`reload.rs::a_changed_service_declaration_restarts_only_it`,
`run.rs::lock_edits_wait_for_the_unlock_and_then_land`).

| Kind | Edit | Table row | Edits in the 10k run |
| --- | --- | --- | --- |
| token | `bar.bg` or `chip.fg` recoloured | token value | 841 |
| binding / prop | the exported `let` text; the surface's height | prop or binding | 455 / 349 |
| node-added / node-removed | a static text or a chip | node added or removed | 457 / 458 |
| move | two children swapped, a chip wrapped in or out of a `row` | node moved | 241 |
| list | a list entry added, removed or moved (keyed item state) | node added or removed | 331 |
| state-default | default of `n`, of a cell, or of the chip's `on` | `state` default | 1,228 |
| rename / retype | a cell renamed or retyped (both files change) | `state` name or type | 456 / 429 |
| handler | `n += 1` → `n += k` in the click handler | handler code | 343 |
| timer | `every N000s` duration changed | timer duration | 411 |
| surface-layer / -name / -kind | `layer: top` ↔ `bottom`; `bar Top` ↔ `bar Main`; `bar` ↔ `panel` | surface layer, namespace or kind | 467 / 430 / 420 |
| rename-token | `bar.bg` ↔ `bar.fill` in `theme.strand` and its use in `bar.strand` | (renames) | 462 |
| rename-param | `Chip(label)` ↔ `Chip(name)` and the named arguments in `bar.strand` | (renames) | 471 |
| rename-module | `cells.strand` ↔ `store.strand` (the file renamed) and every `cells.x` | (renames, moves) | 455 |
| component-moved | `component Chip` moved between `theme.strand` and `bar.strand` | (moves) | 431 |
| broken | a file saved with a syntax or name error: four templates, or a random token deleted or duplicated | (syntax errors) | 802 |
| fixed | a broken file saved back to its last good text | | 63 |

1,275 of the multi-file edits were saved as partial saves: first
one change alone (one that does not type-check with the other files' old
text nor with the last good text, checked in the test), which must be
held back, then the rest. A broken save is fixed by the next edit, which
rewrites that file. Before each edit the user clicks 0–2 state texts
(`n += k`, a cell `+= 1` or `+ "x"`, a chip on one screen toggled), so
the table has values to keep, and now and then closes the overlay.
The edit table's effects that need a clock (a timer firing, an `await`
in flight) are covered at the instance level by
`crates/strand-compiler/tests/reload.rs` (`timer_duration_rescales`, the
handler-restart tests); here a timer's and a handler's edits must keep
state and the next click must run the new handler code.

**What is asserted**, in every pipeline:

- No panic: every logic thread answers every save and joins with `Ok`.
- Atomic commits: after every diff the scene (overlay aside) and the
  token table are those before the step or those after it; a click's
  diff likewise.
- No blank frame: every surface of the shell is up after every diff,
  with its texts; in the painted pipeline no surface's frame is only its
  background after any diff.
- No leaked surface: the surfaces keep their scene ids through every
  diff, unless the edit changes the surface's layer, namespace or kind,
  which replaces every one of them in a single diff; the painted
  pipeline has one painted surface per scene surface; at most one error
  overlay is shown.
- The overlay opens only after a load held something back or a reload
  left notices (a reset, a value kept over a new default) that were not
  dismissed, and once a commit lands clean it lists no errors.
- A broken or partial save is held back (`strand watch` reports it
  `held`, never `unreadable`; nothing committed, or for a partial save
  only what is consistent without it) and the scene does not change,
  watched for 50 ms after the event. One file's save is one load: a
  single save split into two (delete-and-create included) fails the run.
- After each step the scene and the token table equal a cold boot of the
  same files with the state the edit table keeps written into it: kept,
  the new default where the cell held the old one, reset when renamed or
  retyped or its module renamed, fresh for a node or list entry added,
  reset for the chips when the surface turns from a bar on two screens
  into one panel or back (decisions.md, wave2-runtime); the painted
  pipeline's pixels equal a fresh renderer's painting of that cold boot.
  The cells `strand watch` reports reset are the table's.

**Runs.** 10,000 edits (10,753 drawn; draws that changed nothing
are drawn again) with seed `0x5eedf00dcafe0001`, release build: clean in
883.6 s, no multi-file save taken in two loads. (A first attempt, run while
clippy built the workspace on the same 4 CPUs, stopped at step 7,181:
the test thread's delete and create of one file came further apart than
the watcher's 50 ms grace, so the watcher rightly took two loads. The
fuzzer now caps the pause at 25 ms and names a descheduled test thread
as such.) Seeds 7 and 12345,
400 edits each (debug): clean. Reproduce:

```sh
STRAND_FUZZ_EDITS=10000 cargo test --release -p strand --bin strand \
  random_edits_through_five_save_styles -- --nocapture
```

`STRAND_FUZZ_SEED` takes the seed as printed (decimal or `0x` hex); a
value that does not parse fails the run. `cargo test --workspace` runs
60 edits on every push (about 8 s in a debug build); the nightly CI job
runs 10,000 with `STRAND_FUZZ_SEED` set to the run id, so each night
explores a new sequence and prints the seed that replays it. The fuzzer
checked itself: with the table's default adoption switched off it fails
within a few steps, and with the renderer's damage made to forget where
a changed node was, the pixel comparison fails on the first click.

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
still being shaped after the diff was applied, and a presentation timed
after that paint (an older frame's feedback is not taken for it). Both
clocks are `CLOCK_MONOTONIC` (asserted from `wp_presentation.clock_id`).
A test-only probe on `Host` and a `FrameClock` wrapping
`PresentationClock` see the paint, the configure and the presentation;
the shipped binary has neither.

**Headless sway presents a commit at once**: the counted frames were
presented 0.4 ms (p95; max 0.7) after they were painted. A monitor
shows a commit at its next vblank, 0 to one refresh later. So the gates
apply to the samples as a monitor would show them: each sample taken at
20 evenly spaced vblank phases over one refresh, and the p95 of all of
them (`on_a_monitor`). Adding a whole refresh to every sample (the worst
phase) gives 34.4 ms (token) and 34.8 ms (markup).

**Numbers** (100 edits of each kind, final code):

| | headless p95 | max | on a monitor p95 | at worst |
| --- | --- | --- | --- | --- |
| token edit → presented | 17.8 ms | 25.8 ms | 33.5 ms | 34.4 ms |
| markup edit → presented | 18.1 ms | 18.6 ms | 33.5 ms | 34.8 ms |
| of which a node added | 18.2 ms | | | |
| of which a node removed | 17.7 ms | | | |
| token edit → painted buffer (no compositor) | 18.0 ms | 18.4 ms | | |
| markup edit → painted buffer (no compositor) | 17.5 ms | 18.4 ms | | |

The watcher's 15 ms coalescing (design.md, "The pipeline") is most of
every number; compiling, committing and painting the 2560×40 bar take
the remaining 2–3 ms. A node added used to cost one refresh more (34.1
vs 17.9 ms): the frame that committed it was painted before the text
worker had shaped its text, and the frame with the text waited for that
frame's callback. `strand-render` now holds a frame that would show a
text with nothing to draw yet (a node just added) for up to
`NEW_TEXT_WAIT` = 16 ms, as a first frame is held for its text, so the
node and its glyphs arrive together
(`crates/strand-render/tests/damage.rs::a_new_text_node_holds_the_frame_for_its_glyphs`).

The token edit's 35 ms is the tight one: 15 ms of coalescing, 2–3 ms of
work and up to a refresh of vblank wait leave about 1.5 ms at p95 on a
60 Hz monitor. It has not been measured on hardware.

**Monitors.** A scale change (`swaymsg output HEADLESS-1 scale 1.5`,
then back to 1, four times) is timed from the main thread hearing of it
(`wl_output.done`) to the first frame painted at the new scale (1.1–2.3
ms) and on to that frame's presentation (16.2–17.3 ms: sway holds the
first frame after an output change for its next frame timer). A
monitor plugged in (`swaymsg create_output`, five times) is timed from
`wl_output.done` to its new layer surface's first configure (the round
trip a layer surface must wait for before it may commit: 0.5–0.8 ms),
to its bar's first frame (0.9–1.1 ms later) and to that frame's
presentation (7.1–13.0 ms later: the new output's first frame timer).
The gate is design.md's "next frame": the first frame the shell paints
once it may shows the change, within one refresh (a frame lost on the
shell's side misses by a refresh), and it is the frame presented next,
under two refreshes. `crates/strand/src/run.rs::a_monitor_change_is_in_the_next_diff`
checks the logic side exactly: a plug, a scale change, an unplug and a
replug are each wholly in the first diff the logic thread sends after
the main thread tells it.

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
  reload_latency -- --test-threads=1` (both latency benches, one at a
  time, sway required).
- Nightly (03:17 UTC) and on demand: the fuzzer for 10,000 edits with a
  new seed, the instance-level fuzzers for 10,000 and 2,000, the latency
  benches for 200 edits per kind.

## Open

- Portal changes on the next frame: `strand-watch` reads and follows the
  portal settings, but nothing feeds `system.dark`, `system.accent` and
  `system.contrast` into `strand run` yet (the portal item under M2).
  The benchmark line in `docs/features.md` stays open for this clause.
- The token edit on a 60 Hz monitor is estimated at 33.5 ms p95 against
  its 35 ms budget (above); it has not been measured on hardware.
- A module renamed in one load reports its cells reset (`renamed`); in
  two loads (the new file saved first, the old one's removal held back)
  it is a module added and then one removed, and the old cells go with
  their declarations without a reset notice. The values are the same
  either way (decisions.md, wave2-exit).
- The fuzzer's pixels come from the offline renderer, not a compositor;
  the M0 and `strand run` sway tests check the painted bars on sway.
- Other M1 items not part of the exit gates are open in
  `docs/features.md`: the formatter, the tree-sitter grammar and the
  basic LSP (`strand-dev`), the render parts of `keyframes`, `shader` and
  `canvas`, and the watcher registrations for `.wgsl` files and
  wallpapers.

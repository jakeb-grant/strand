# M1 exit report

Measured 2026-10-05 and 2026-10-06 (round 2) on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another agent's builds), rustc 1.97.0, headless sway 1.9 with
the pixman renderer, release builds (thin LTO, one codegen unit,
mimalloc). Every number below comes from a test in the tree that fails
when its gate is missed; the commands to reproduce each are given with
it.

## Result

| Gate (`docs/features.md`, M1 exit) | Budget | Measured | |
| --- | --- | --- | --- |
| Random edits with no panic or blank frame | 10,000 clean | **10,000 edits clean, each saved in all five editor styles (50,000 saves) into five live pipelines on two screens and in place into `strand run` on headless sway, in 918.7 s; one pipeline's frames painted offline and compared with a cold boot's, the sway pipeline's layer surfaces and committed buffers checked** | pass |
| Token edit, save → presented frame | p95 ≤ 35 ms | **p95 33.4 ms in the model of a 60 Hz monitor** (headless sway 17.8 ms, max 18.5, plus a vblank wait at a uniform phase over one refresh; worst phase 34.4), 100 edits | pass (model) |
| Markup edit, save → presented frame | p95 ≤ 50 ms | **p95 33.4 ms in the model** (headless 18.0 ms, max 18.3: a node added 18.0, removed 18.0; worst phase 34.7), 100 edits | pass (model) |
| Monitor change on the next frame | next frame | the first frame painted after the shell hears of it shows it: a scale change 1.2–1.8 ms after `wl_output.done`, a plugged monitor's bar 1.7–2.1 ms after `wl_output.done` (its new surface configured 0.6–0.7 ms after it), and that frame is the one presented (sway's next frame timer, 6–18 ms later); the logic thread answers every plug, scale change, unplug and replug in its first diff | pass |
| Portal change on the next frame | next frame | not measured: portal settings are not fed into `strand run` yet (M2) | open |

The design's two M1 exit boxes ("10k random edits with no panic or blank
frame", "under 50 ms from save to pixels") are ticked. The benchmark line
under "Live reload" stays open for its portal clause only.

## Reload fuzzer

`crates/strand/src/fuzz.rs::random_edits_through_five_save_styles`
(design.md, "How reload is tested").

**What runs.** Six copies of the `strand run` pipeline. Five run without
Wayland, one per save style: the real `strand-watch` watcher (inotify on
directories, 15 ms coalescing, 50 ms after a removal), the compiler
worker (the `Loader`: largest consistent set, held-back files, last good
sources), the logic thread (`Instance::reload`, the error overlay, the
IPC socket) and a mirror of the scene diffs, with two screens plugged in
(a `bar` on each). One of the five (in place) also feeds its diffs to an
offline `Renderer` (vello_cpu, inline text shaping), a surface per
surface-kind node, painted into two buffers in turn with their buffer
age, so damage and its history decide what is repainted. The sixth
saves in place into the whole of `strand run` on a headless sway with
two outputs, `HEADLESS-1` and `HEADLESS-2` (the fuzzer's screens are
named after them, so its bars land on them): the `SurfaceManager`,
`Host`, the renderer and the text worker, as `run::run` wires them.
Each copy has its own config directory on tmpfs
(`/dev/shm/strand-fuzz-<pid>/<style>/config`, removed when the run ends,
passed or not) and saves in one style:

| Style | How a file is saved | Editors |
| --- | --- | --- |
| In place | truncate and write | VS Code |
| Rename | write `.fuzz-save.tmp`, rename over | Helix, atomic saves |
| Backup then rename | rename to `name~`, write anew, delete the backup | Vim `backupcopy=no` |
| Delete and create | unlink, wait 0–25 ms (random, spun), create | some scripts and sync tools |
| Symlink swap | write a new target in a store directory, swap the link, delete the old target | home-manager |

A file renamed (`mv`) is renamed in every style. Every edit is saved
into all six pipelines; then each must answer.

**The edits.** A model of a four-file config (`theme.strand`: tokens,
an exported `let`, an exported list, the `Chip` component;
`cells.strand`: exported int and text state; `bar.strand`: the main
surface (a `bar` on every screen or one `panel`), its `state n`, a
timer, a text per cell with a click handler, chips, a keyed `for` over
the list, static texts; `note.strand`: an `osd` no edit touches). Each
step draws one edit, covering the rows of design.md's "What each edit
does" but two: a custom service declaration (a service needs a D-Bus
name, a file, a socket or a permitted command to run) and anything
inside `lock` (deferred only while a session lock is shown); their rows
have their own tests
(`reload.rs::a_changed_service_declaration_restarts_only_it`,
`run.rs::lock_edits_wait_for_the_unlock_and_then_land`).

| Kind | Edit | Table row | Edits in the 10k run |
| --- | --- | --- | --- |
| token | `bar.bg` or `chip.fg` recoloured | token value | 866 |
| binding / prop | the exported `let` text; the surface's height | prop or binding | 413 / 386 |
| node-added / node-removed | a static text or a chip | node added or removed | 454 / 453 |
| move | two children swapped, a chip wrapped in or out of a `row` | node moved | 219 |
| list | a list entry added, removed or moved (keyed item state) | node added or removed | 314 |
| state-default | default of `n`, of a cell, or of the chip's `on` | `state` default | 1,113 |
| rename / retype | a cell renamed or retyped (both files change) | `state` name or type | 444 / 431 |
| handler | `n += 1` → `n += k` in the click handler | handler code | 339 |
| timer | `every N000s` duration changed | timer duration | 404 |
| surface-layer / -name / -kind | `layer: top` ↔ `bottom`; `bar Top` ↔ `bar Main`; `bar` ↔ `panel` (the `osd` kept) | surface layer, namespace or kind | 435 / 421 / 409 |
| rename-token | `bar.bg` ↔ `bar.fill` in `theme.strand` and its use in `bar.strand` | (renames) | 444 |
| rename-param | `Chip(label)` ↔ `Chip(name)` and the named arguments in `bar.strand` | (renames) | 426 |
| rename-module | `cells.strand` ↔ `store.strand` (the file renamed) and every `cells.x` | (renames, moves) | 424 |
| component-moved | `component Chip` moved between `theme.strand` and `bar.strand` | (moves) | 418 |
| broken | a file saved with a syntax or name error: four templates, or a random token deleted or duplicated | (syntax errors) | 781 |
| fixed | a broken file saved back to its last good text | | 63 |
| mutation | a random word deleted or duplicated that still compiles, then saved back | prop or binding | 343 |

1,167 of the multi-file edits were saved as partial saves:
first one change alone (one that does not type-check with the other
files' old text nor with the last good text, checked in the test),
which must be held back, then the rest. A broken save is fixed by the
next edit, which rewrites that file. Before each edit the user clicks
0–2 state texts (`n += k`, a cell `+= 1` or `+ "x"`, a chip on one
screen toggled), also while a broken save is held back (the shell runs
its last good config meanwhile, design.md's scenario 3, and the fix must
land on the clicked state), so the table has values to keep, and now
and then closes the overlay. A random word deleted or duplicated that
still compiles is run as an edit when it touches only an expression (a
string, an argument, a path) outside the keyed list's literal (an entry
dropped and put back starts fresh, which the fuzzer does not model) and
then saved back; about 3% of random
mutations compile, so one draw in 25 searches for one. The edit table's
effects that need a clock (a timer firing, an `await` in flight) are
covered at the instance level by `crates/strand-compiler/tests/reload.rs`
(`timer_duration_rescales`, the handler-restart tests); here a timer's
and a handler's edits must keep state and the next click must run the
new handler code.

**What is asserted**, in every pipeline:

- No panic: every logic thread answers every save and joins with `Ok`;
  every compiler worker and watcher thread joins without a panic
  (`Worker::join`, `Watcher::join`).
- Atomic commits: after every diff the scene (overlay aside) and the
  token table are those before the step or those after it; a click's
  diff likewise. (A compiling mutation's own frame is not modelled; the
  save back must land on the state from before.)
- No blank frame: every surface of the shell is up after every diff,
  with its texts; in the offline-painted pipeline no surface's frame is
  only its background after any diff; on sway no committed buffer is
  only its background (a probe on `Host::paint` sees every committed
  frame), and after each step every layer surface is configured and has
  committed a frame.
- No leaked surface: on sway, whenever the main thread has applied what
  the logic thread sent, its live layer surfaces are exactly the scene's
  surface roots, the overlay's included. The surfaces keep their scene
  ids through every diff, unless the edit changes the main surface's
  layer, namespace or kind, which replaces it in a single diff and keeps
  the `osd`; the offline pipeline has one painted surface per scene
  surface; at most one error overlay is shown.
- The overlay opens only when a reload left notices (a reset, a value
  kept over a new default) that were not dismissed, or a load was held
  back for 250 ms (checked as 200 ms on the test's clock, which starts
  before the save) with no save ending the hold before then: a partial
  save completed 50 ms later, a broken save fixed at once or a save split
  into two loads never shows it. Once a commit lands clean it lists no
  errors.
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
  into one panel or back (decisions.md, wave2-runtime); the offline
  pipeline's pixels equal a fresh renderer's painting of that cold boot.
  The cells `strand watch` reports reset are the table's; for a module
  renamed in two loads (the new file first), the cells holding a value
  the user set, reported `removed with its module` (decisions.md,
  wave2-exit, round 2).

**Self-checks.** The fuzzer fails within a few steps with the table's
default adoption switched off; on the first click with the renderer's
damage made to forget where a changed node was; at the first surface
edit with `strand-surface` not destroying a removed node's surfaces (the
sway pipeline); and at the first partial save with the overlay's quiet
period set to zero.

**Runs.** 10,000 edits (10,859 drawn; draws that changed nothing are drawn again)
with seed `0x5eedf00dcafe0001`, release build, round 2 code, sway
required: clean in 918.7 s, with 841 clicks made while a broken save was
held back, no multi-file save taken in two loads and no late create.
Seeds 777 (800 edits) and the default seed for 200 (debug): clean. The
first round-2 10k attempt stopped at step 2,299 on a mutation that
dropped a keyed list entry and put it back (its chips' state, rightly,
started fresh, which the fuzzer had not modelled); such mutations are
now drawn again (decisions.md, wave2-exit). Reproduce:

```sh
STRAND_REQUIRE_SWAY=1 STRAND_FUZZ_EDITS=10000 cargo test --release \
  -p strand --bin strand random_edits_through_five_save_styles -- --nocapture
```

`STRAND_FUZZ_SEED` takes the seed as printed (decimal or `0x` hex); a
value that does not parse fails the run. Every push runs 60 edits in a
CI step of its own (about 10 s in a debug build, `--test-threads=1`, a
delete-to-create gap of at most 20 ms); the nightly CI job runs 10,000
with `STRAND_FUZZ_SEED` set to the run id, so each night explores a new
sequence and prints the seed that replays it.

**Delete and create on a loaded machine.** A delete and its create
further apart than the watcher's 50 ms grace are two saves, correctly,
and removing `bar.strand` alone even commits (the shell has no bar until
the create), so such a step cannot be checked as one save. A first 10k
attempt in round 1, while clippy built the workspace, and one in round 2,
while another agent built on the same 4 CPUs, stopped on this (51 ms).
The pause is now spun rather than slept (a sleeping thread's wake-up
waits for a CPU), a gap past the grace only marks the step, and the run
fails only when the watcher then really took two loads, with a message
naming the descheduled test thread. The run prints how many such late
creates still landed as one load. In CI the fuzzer has a step (or a
job) of its own.

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
presented 0.37 ms (p95; max 0.41) after they were painted. A monitor
shows a commit at its next vblank, 0 to one refresh later. So the gates
apply to the samples in a model of a 60 Hz monitor: each sample taken
at 20 evenly spaced vblank phases over one refresh, and the p95 of all
of them (`on_a_monitor`). This is a model, not a measurement: it
assumes the save's phase against the vblank is uniform and that the
surface is idle (no frame callback pending; the bench idles 250 ms
between edits). Adding a whole refresh to every sample (the worst
phase) gives 34.4 ms (token) and 34.7 ms (markup); it is printed and
recorded here, not gated.

**Numbers** (100 edits of each kind, round 2 code):

| | headless p95 | max | model p95 | worst phase |
| --- | --- | --- | --- | --- |
| token edit → presented | 17.8 ms | 18.5 ms | 33.4 ms | 34.4 ms |
| markup edit → presented | 18.0 ms | 18.3 ms | 33.4 ms | 34.7 ms |
| of which a node added | 18.0 ms | | | |
| of which a node removed | 18.0 ms | | | |
| token edit → painted buffer (no compositor) | 18.0 ms | 18.4 ms | | |
| markup edit → painted buffer (no compositor) | 17.3 ms | 17.6 ms | | |

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
Only an idle surface holds: one that painted within `BUSY_WINDOW` = 34
ms (an animation, rows scrolling into view) paints at once and its new
glyphs follow a frame later, so nothing else on it waits
(`a_busy_surface_does_not_hold_for_new_text`).

**Headroom and flakes.** The token edit's 35 ms is the tight one: 15 ms
of coalescing, 2–3 ms of work and up to a refresh of vblank wait leave
about 1.5 ms at p95. The gate breaks once the headless p95 passes about
19.2 ms (35 less 0.95 of a refresh); the failure message prints that
break-even beside the headless p95. A slow or busy CI runner shows as
every sample a little high; a regression as a step (a refresh more, as
the node-added bug was). CI runs 50 edits per kind per push so p95 is
not the worst of 20 samples, and 200 nightly.

**Not measured.** A token edit on a busy surface (an animation running,
a frame callback pending): M1's renderer animates nothing (springs land
in M2), and headless sway answers a frame callback at once, so the wait
a monitor adds there (the pending callback's vblank, then the next, up
to one refresh more than the model) cannot be seen on this setup. On
hardware it would be about 50 ms at p95 in the same model; it is the
first thing to measure once M2's springs and a real output exist.

**Monitors.** A scale change (`swaymsg output HEADLESS-1 scale 1.5`,
then back to 1, four times) is timed from the main thread hearing of it
(`wl_output.done`) to the first frame painted at the new scale (1.2–1.8
ms) and on to that frame's presentation (16.6–18.1 ms: sway holds the
first frame after an output change for its next frame timer). A
monitor plugged in (`swaymsg create_output`, five times) is timed from
`wl_output.done` to its bar's first painted frame (1.7–2.1 ms): the
logic thread's answer (the new bar's diff), the main thread applying
it, the layer surface's creation and its configure round trip (all of
which take 0.6–0.7 ms to the configure; an earlier run had one sample
at 7.8 ms, most of it the logic and main thread's work), then the paint;
and on to that frame's presentation (6.2–12.3 ms later: the new
output's first frame timer). The gate is design.md's "next frame",
end to end from hearing of the change: the first frame the shell paints
shows it, within one refresh (a frame lost on the shell's side misses by
a refresh), and it is the frame presented next, under two refreshes.
`crates/strand/src/run.rs::a_monitor_change_is_in_the_next_diff` checks
the logic side exactly: a plug, a scale change, an unplug and a replug
are each wholly in the first diff the logic thread sends after the main
thread tells it. A counted frame the compositor discards is painted
again (none were discarded).

Reproduce:

```sh
STRAND_LATENCY_ROUNDS=100 cargo test --release -p strand --bin strand \
  reload_latency -- --nocapture --test-threads=1
```

CI runs it (50 edits per kind) on every push and fails the build when a
p95 misses; the nightly job runs 200 per kind. Both tests are ignored in
debug builds, which measure the unoptimised repaint rather than the
design.

## CI

`.github/workflows/ci.yml`:

- Every push: `cargo test --workspace -- --skip
  random_edits_through_five_save_styles`, `cargo test --release -p strand
  --test demo` (M0's PSS budget), `cargo test --release -p strand --bin
  strand reload_latency -- --test-threads=1` (both latency benches, one
  at a time, 50 edits per kind, sway required), and the fuzzer's 60
  edits in a step of its own (`--test-threads=1`, on `/dev/shm`, the sway
  pipeline required, `STRAND_FUZZ_MAX_GAP_MS=20`).
- Nightly (03:17 UTC) and on demand: the fuzzer for 10,000 edits with a
  new seed, the instance-level fuzzers for 10,000 and 2,000, the latency
  benches for 200 edits per kind.

## Open

- Portal changes on the next frame: `strand-watch` reads and follows the
  portal settings, but nothing feeds `system.dark`, `system.accent` and
  `system.contrast` into `strand run` yet (the portal item under M2).
  The benchmark line in `docs/features.md` stays open for this clause.
- The latency gates are a model of a 60 Hz monitor (uniform vblank
  phase, idle surface) on headless sway; nothing is measured on
  hardware. The token edit has about 1.5 ms of headroom in the model,
  and a busy surface is not measured (above).
- The fuzzer does not exercise the edit table's custom-service and
  `lock` rows (their own tests are named above), and a compiling
  mutation's own frame is not modelled (only that it keeps every cell
  and that the save back lands on the cold boot).
- Other M1 items not part of the exit gates are open in
  `docs/features.md`: the formatter, the tree-sitter grammar and the
  basic LSP (`strand-dev`), the render parts of `keyframes`, `shader` and
  `canvas`, and the watcher registrations for `.wgsl` files and
  wallpapers.

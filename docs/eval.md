# The animation evaluation sandbox

`eval/` runs a shell config on a virtual clock and checks every frame it
draws, node by node. A scenario script drives it: it sets state, moves the
pointer, holds text back, changes the scale or refresh rate, and waits for
rest. The checks are invariants that hold for any correct animation, such
as "a node whose layout does not move is not drawn elsewhere while
something else animates". Each run leaves a report that a person can scrub
and Claude can read.

It exists because owner-testing.md bugs 24 and 25 were found by eye in the
owner's shell. Both bugs were a few frames of bars dipping out of an
island and coming back. The regression scenarios reproduce them, and the
mutants prove the scenarios catch them. Curated scenarios, the regression
scenarios and an effect matrix catch the next bug of that kind before it
ships.

The sandbox is evaluation tooling, not product. It is a Cargo workspace of
its own (`eval/Cargo.toml`), outside the root workspace, so `--workspace`
builds, `affected` and the shipped binary never see it. It reads
strand-render through its `eval-hooks` feature. docs/decisions.md
(`eval-sandbox`) records why each part is the way it is.

## Quick start

Everything runs through the container, like every other build here:

```text
scripts/container/run.sh eval check eval/scenarios/curated      # run and check
scripts/container/run.sh eval run my.scn --frames all           # run, no checks
scripts/container/run.sh eval explain target/eval/latest/<id>   # a node's frames as text
scripts/container/run.sh eval selftest                          # the CI gate (scripts/eval/ci.sh)
scripts/container/run.sh eval matrix --hz 60,144                # the effect matrix (see Matrix)
scripts/container/run.sh eval mutants                           # the bugs the regressions catch
scripts/container/run.sh eval ab-revs HEAD~1                    # A/B: the last commit against this checkout
```

`run.sh eval <command>` is `scripts/eval/eval.sh`. For anything except
`selftest`, `mutants`, `ab-revs` and `matrix-all`, it is `strand-eval
<command>` built from `eval/`.

The commands:

- `check [--gate] <scn|dir>... [--out DIR] [--frames all|failing|none]`
  runs and checks each scenario, and fails when any scenario fails.
  `--gate` is CI's form: it refuses `eval/scenarios/local/` and an empty
  set, and fails on an XPASS.
- `run <scn|dir>... [--out DIR] [--frames …] [--reduced-motion]
  [--backend cpu|gpu]` runs and writes the report without checking.
- `explain <run>/<id> [--node SEL] [--frames a..b] [--check NAME]` prints
  a node's frames as a text table. By default it shows the nodes of the
  failing findings.
- `verdict <run>/<id> good|bad [--note TEXT]` records a verdict (see
  Verdicts and references).
- `promote <run>/<id> --as NAME [--xfail CHECK]... [--note TEXT]` writes a
  run's scenario out as a regression (see Matrix).
- `ab <run A> <run B> [-o DIR]` makes one page of two reports (see A/B).
- `matrix [--effects a,b|all] [--hz 60,144] [--singles] [--jobs N]
  [--out DIR]` runs the effect matrix.

`bench` (a scenario's cost table) and `new` (a scenario skeleton) are
reserved names. They are not built yet and return a typed "unsupported"
error.

A report lands in `--out`, or in `target/eval/runs/<seconds>-<rev>`, with
`target/eval/latest` pointing at it.

## Scenario format

A scenario is a `.scn` file with three parts:

- a TOML header, up to the first `=== ` line;
- the config's files, each as an `=== <path>.strand` section;
- the steps, under `=== steps`.

None of it is Strand syntax. The config sections are ordinary Strand files.

```text
id     = "status-row-reorder"
about  = "Two keyed boxes swap; the boxes around them stay put."
screen = { width = 300, scale = 1.0 }
hz     = 60

=== shell.strand
state wide = false
bar Top {
  height: 30
  row { height: 24; gap: 6; align: center
    box { id: before; width: 20; height: 12; bg: #00ff00 }
    for n in (wide ? ["b", "a"] : ["a", "b"]) key n { box { width: 10; height: 10; bg: #ffffff } }
    box { id: after; width: 20; height: 12; bg: #0000ff }
  }
}

=== steps
rest
mark swap
set wide true
rest
expect still #before
expect still #after
```

### Header

| Key | Default | Meaning |
|---|---|---|
| `id` | required | The scenario's name in reports. |
| `about` | none | What it holds, in prose. |
| `screen` | `{ width = 300, height = 1080, scale = 1.0 }` | The screen (scale up to 4). |
| `hz` | 60 | The refresh rate; a `hz` step changes it. |
| `backend` | `"cpu"` | `"cpu"`, `"gpu"` or `"both"` (see CPU and GPU). |
| `text` | `"inline"` | `"inline"` only; `"worker"` is refused as unsupported. |
| `fonts` | `"bundled"` | Liberation Sans from strand-text; `"local"` only in `scenarios/local/`. |
| `checks` | every CPU check | The checks to run (see Checks). |
| `expect_fail` | none | `{ check = "why" }`: a known failure, reported as XFAIL. |
| `reduce_motion` | false | Motion reduced from the start. |
| `twin_pixels` | false | `twin` compares pixels too. |
| `tooltip_delay` | the renderer's | In ms. |
| `config`, `config_dir` | none | A config named instead of written in (see Local configs and fonts). |
| `[services]` | none | `"path" = value` pairs, set before boot. |
| `[allow]` | none | `check = [{ node = "<selector>", why = "…" }]`. |
| `[tolerance]` | `edge_px = 0.5`, `overshoot = 0.35`, `teleport_px = 32` | See Checks. |
| `[budget]` | none | Per-frame ceilings: `items`, `records`, `damage_px`, `layout_nodes`, `layout_passes`. |

A committed scenario is any scenario outside `eval/scenarios/local/`. It
may name only files inside `eval/` and the bundled fonts. The parser
refuses anything else, so an owner's config or fonts never reach a gate.

### Step vocabulary

There is one step per line, and `#` followed by a space starts a comment.
Instant steps act at the tick the next time step starts. Time steps
advance the clock.

**State and services**

- `set <path> <value>`: a value as `strand set` reads it.
- `toggle <path>`.
- `emit <service.event> [args…]`.
- `notify "<summary>" ["<body>"] [app=…] [urgency=…]` parses, but a run
  refuses it as unsupported: the notification daemon is not in the
  sandbox. Set `notifications.*` and `emit` instead.

**Input**

- `pointer X,Y [on <surface>]`.
- `press` and `release`, each with `left`, `right` or `middle`.
- `click X,Y [left|right|middle] [on <surface>]`.
- `scroll DY`.
- `glide X,Y -> X,Y over MS [on <surface>]` moves the pointer.
- `drag X,Y -> X,Y over MS [on <surface>]` is the same glide with the
  left button held: pressed at the start, released at the end.

**Perturbations**

- `text hold [where "s"]`.
- `text release [where "s"] [via waker|paint]`.
- `text late N [where "s"] [via …]`.
- `drop N [surface=<selector>]`: the next N ticks paint nothing, on
  every surface or on the one named.
- `jitter 1,1,2,1` sets per-tick period factors.

**Environment**

- `reduce-motion on|off` sets `system.reduced_motion`, which logic reads
  and hands render in its diff, as in the shell (so does `set
  system.reduced_motion …`).
- `scale S`.
- `hz N`.
- `backend cpu|gpu`.

**Annotation**

- `mark LABEL` names the segment that starts here.
- `shot LABEL` keeps that frame's image.
- `log TEXT`.
- `intend <selector> moves|enters|exits|changes [axes]` says that
  movement in this segment is intended.
- `expect still|moves <selector> [axis] [from +K]`.
- `expect visible|hidden <selector>`.

**Time**

- `frames N`.
- `wait MS`.
- `rest [max N]` waits until nothing wants a frame. The default max is
  600 ticks, and `settle` fails a rest that does not arrive.

### Selectors

- `#id` selects the node with that `id:`.
- `@file.strand:LINE[:COL][n]` selects the element written there. `[n]`
  picks a copy among siblings.
- `text("…")` selects a node whose text is exactly that.
- A kind path from a surface root, such as `bar>row>box[2]`, counts `[n]`
  from 0 among siblings of that kind.

A selector resolves against the nodes mounted when its step runs. When it
matches none of those, it matches the nodes seen earlier in the run (so
`expect hidden #x` names a node that has left). One that matches neither
is an error at that line.

### Clock

Tick `k` is presented at `T0 + Σ period · jitter`, in whole nanoseconds,
with `T0` = 1 s. At time zero every motion would snap. 144 Hz does not
drift.

### Input

Pointer steps go through the host's input router as a surface manager's
events would, and routing reaches logic as it does in the shell. A popup's
`delay` and grace run on the virtual clock. Actions the shell would run are
recorded, never executed.

### Perturbations

Texts no rule names are shaped inline, as if nothing held them:

- `text hold` keeps matching texts back until a `text release`.
- `text late N` holds each one for N ticks after the first frame drew its
  estimate.

A release `via paint` delivers just before the next paint. A release `via
waker` delivers between frames and runs `Renderer::update`, as the
runtime's `text_ready` wake does.

## Driver

One scenario runs in one process: the logic VM, the input router and the
renderer, wired as the runtime wires them (crates/strand/src/demo and
run/). The renderer's own timers (a tooltip's delay, a capped clock's tick)
and the router read the virtual clock. Each tick does this, in order:

1. The tick's input steps go through the router, as `Host::input` does:
   input that leaves an idle surface wanting a frame wakes the loop.
2. Logic runs its timers and steps to `t`. Only a diff with something in
   it is applied (as the logic thread sends), then the router settles and
   surfaces follow.
3. The loop wakes between frames if a waker text is delivered or the
   renderer's next wake is due: the router ticks and `Renderer::update`
   runs (`text_ready`).
4. A frame held for logic's answer to its layout facts (an idle surface's
   container query) gets it first.
5. Paint texts are delivered, then every surface that wants a frame paints
   once at `t`. A paint that leaves surface changes wakes the loop
   (`Host::painted`).
6. The frame's traces are collected, and what the frame laid out goes to
   logic; its answer lands before the next frame.

Text is shaped inline, where the shell has a text worker that wakes the
loop for each layout it delivers: the driver wakes the loop for each
layout shaped inline, as that wake would. The renderer's wall-clock holds
are neutralised with its own setters (the query hold excepted: logic always
answers in time), so a run is a function of its scenario. Surfaces are attached and configured at
the size their spec asks, as the surface manager does. A run that leaves a
surface wanting frames at the end of a rest is a `settle` failure, not a
hang.

## Trace

Each run writes `trace.json` (schema 1): per tick, every painted surface's
nodes, plus the surface's cost and its `Painter` regions.

For each node the trace records:

- its drawn, visual and laid boxes, in logical pixels quantised to 1/64;
- its effective opacity, scale and rotation;
- its motions, both its own and its ancestors' (glide, spring, size
  spring, enter, exit, keyframes, morph, reveal, stagger, lean, jelly,
  drag, time);
- its authored axes;
- the parts of its offset (inherited, and of that the part in flight;
  written, spring, glide, morph);
- why it is hidden;
- its text state (estimate, shaped or held).

Each frame also records the screen's scale it was drawn at, and whether
logic's diff or input reached the renderer since the frame before.

A node's key is its origin plus its sibling path, stable across runs and
builds. The digest hashes every deterministic field and none of the wall
times.

## Checks

A segment runs from a step that changes something to the next `rest`. `s`
is the tick before its first change, and `e` is its rest. On each axis a
node is either a mover or a bystander:

- A **mover** is laid out differently at `s` and `e`, or drawn at a
  different place at both (a written offset changed), or laid out
  elsewhere between them for a reason: something on its surface that lays
  out anew by itself (an exiting ghost holding its slot, a size spring,
  keyframes, a time-driven prop, a drag, a text drawn by its estimate), or
  logic's diff or input arriving at the frame it came back on.
- A **bystander** is any other node: laid out the same at `s` and `e`, and
  elsewhere between them for no reason. Such a layout is a wrong one
  drawn: it neither makes the node a mover nor widens the way `envelope`
  holds a mover to.

Authored motion is exempt per axis. That covers springs of values logic
set, poses, keyframes, morphs, lean, jelly and drags. Derived motion
(glides, layout, text arriving) is held to the checks.

| Check | Holds |
|---|---|
| `settle` | Every `rest` arrives within its max: no surface wants a frame, no wake is due soon and no text is held. |
| `bystander` | A bystander is not drawn elsewhere while the segment runs (bugs 24 and 25). |
| `envelope` | Derived motion stays between where a mover started, where it rests and where each frame counted laid it out, plus an overshoot of `max(3 px, overshoot × travel)`. |
| `teleport` | A moving node's edge moves at most `max(teleport_px × 60 / hz, 0.75 × what is left) + tol` per tick; what is left is the travel, or its own and its ancestors' glide (or drag) still in flight. A jump with no motion (layout snapping) is noted, not failed. On w and h, only motion that sizes a box counts. |
| `vanish` | Nothing blinks out and back within three ticks, shows for one frame only, pops in past half its enter pose, or leaves a damaged frame blank. |
| `twin` | The run with motion reduced ends each settled segment where the run does. |
| `determinism` | Two runs have equal digests. Curated scenarios always run it. |
| `budget` | Each frame stays under `[budget]`. Wall times never gate. |
| `reference` | The run does not reproduce a recording marked `bad` (see Verdicts and references). |
| `expect` | The `expect` steps. It always runs. |
| `regions` | Opt-in. Where a surface takes input follows what it draws. |
| `cpu-gpu` | Comes with `backend = "both"`. The CPU and GPU legs agree (see CPU and GPU). |

`tol` is `edge_px`, and at least one physical pixel at the coarsest scale
any frame was drawn at.

Authored axes pass to children as motion carries them: an ancestor's
offset authors their x and y, its opacity their opacity, its scale or
rotation all their geometry. Its size does not: a size that springs or
plays is laid out each frame, and its children are held to their layouts.

**`regions`.** It holds docs/spec.md 6.7's rule: a `trigger: hover` popup
that draws nothing itself takes input only in the boxes of its top-level
elements that draw, as they are drawn this frame. On a frame whose surface
reports a partial input region, the check holds two things:

- every shown top-level element that draws content is inside the region,
  or each of its children is (the `hit: none` case);
- every rect of the region is a box drawn this frame.

`eval/scenarios/curated/hover-island-regions.scn` is the example. It is
opt-in because only a scenario built around such a popup states what its
region should be.

**Exempting a node.** Three mechanisms, from narrowest to widest:

- `intend #x moves x` in a segment exempts that node and its subtree on
  those axes, in that segment only.
- `[allow] check = [{ node, why }]` exempts a node from one check for the
  whole scenario. The reason is required and shown in the report.
- `expect_fail = { check = "why" }` turns the check's failure into XFAIL.
  If the check passes, it is an XPASS, which fails the gate until the line
  is removed.

None of these is for making a real finding pass. A finding the owner
accepts gets a scenario that says why.

## Verdicts and references

`strand-eval verdict <run>/<id> good|bad` stores the recording's digest
and node paths as `eval/refs/<id>.ref.json`:

- A later run that reproduces a `bad` recording fails `reference`. It
  counts as reproduced when the digest matches, or when every node's path
  is within `tol`.
- A run that no longer matches a `good` recording is CHANGED, a warning
  that never gates.

## Reports

A report directory holds, per scenario:

- `trace.json`, `checks.json` and `scenario.scn`;
- the frames (all of them, failing ones only, or none, set by `--frames`);
- contact sheets.

Per run, it also holds `summary.md`, `summary.json`, `summary.txt` and
`index.html`. In CI, `scripts/eval/ci.sh`'s gate and `scripts/eval/matrix.sh`
append their run's `summary.md` to `$GITHUB_STEP_SUMMARY`; `strand-eval` itself
writes nothing outside `--out`, so its tests and mutant runs never do.

### The viewer

`index.html` is one self-contained file, with nothing loaded from
elsewhere. It shows each scenario's frames:

- scrubbable by tick;
- with nodes outlined by drawn, visual or laid box;
- with the findings listed and linked to their frames;
- with a node's path plotted per axis.

When the images would not fit its size budget, it keeps the frames around
findings, `shot` steps and segment ends. It draws the rest as wireframes
from the trace, which is always complete.

### Reading a failure

A finding line reads as follows:

```text
teleport [off] box#tag2 (shell.strand:23:88) k84 x 17.00 not in [0.00, 14.33]: jumped 17.0 px on x in one frame
```

From left to right, that is:

- the check;
- the segment, named by its `mark`;
- the node and where it is written;
- the tick;
- the axis;
- the value and its bound;
- what happened.

`explain <run>/<id> --node '#tag2' --frames 80..90` prints that node's rows
around the tick. Start from the trace, not the pixels: the motion bits and
the offset parts say which mechanism moved the node.

## Matrix

`eval/matrix/effects.toml` lists effects: poses, size springs, FLIP
reorders, morphs, reveal, stagger, lean, jelly, keyframes, late text,
screen scales, and the GPU leg (`gpu`). Each one is a set of fragments
filled into `eval/matrix/fixture.strand.in`, then run with the standard
steps (on, wide, off). The matrix runs every effect alone and every
unordered pair at each rate.

Two effects that set the same prop of one slot cannot pair, and their cell
is `n/a: conflict`. A cell fails the matrix in three cases:

- it fails a check `eval/matrix/known.toml` does not list for it, or a
  listed check on a node the entry does not name;
- a check listed for it passes (stale: remove the entry);
- it cannot run.

Each known entry names its checks and the nodes their failures are on
(`nodes`, selectors; `*` is any node, the only way to accept a finding on
no node, such as a surface's pixels), says why and since when, and is
listed only after it was triaged as the product's finding or as the
sandbox's known limit. An entry can be limited to one rate (`a+b@144`).
`promote` takes committed scenarios and matrix cells only, never a local
scenario's run.

```text
scripts/container/run.sh eval matrix --hz 60,144 --out target/eval/matrix
scripts/container/run.sh eval promote target/eval/matrix/<cell>@<hz> --as <name>
```

`promote` writes a cell's scenario, exactly as it ran, to
`eval/scenarios/regressions/<name>.scn`. Add `--xfail CHECK --note "why"`
when the finding stays open. The matrix runs nightly and on demand in
`.github/workflows/eval-matrix.yml`.

## Mutants

`eval/mutants/mutants.txt` pairs patches with regression scenarios. Each
patch reverts a fix in crates/strand-render, for example bug 24's flip-open
and bug 25's hidden-text. In a detached worktree of HEAD, each scenario
must pass on HEAD and fail under its patch with at least the checks listed.
A patch that no longer applies is STALE: regenerate it. Each mutant leaves
an A/B page of HEAD against it. Commit first: the mutants run HEAD.

## A/B

There are two ways to compare runs:

- `scripts/container/run.sh eval ab-revs <revA> [<revB>]` builds each
  revision in a detached worktree, one at a time. It runs this checkout's
  curated and regression scenarios on both (copied into each revision's
  `eval/`, with `eval/fixtures`, since a revision's `strand-eval` holds
  committed scenarios to its own `eval/`) and writes
  `target/eval/ab/<A>-vs-<B>/`. `revB` defaults to the checkout as it is,
  uncommitted changes included. Both revisions must have `eval/`: one
  from before the sandbox has nothing to build.
- `strand-eval ab <run A> <run B>` pairs two existing reports.

The page matches nodes by key and lists the first frame each pair differs
at. This is how to see what an animation change does to every scenario.

## CPU and GPU

`backend = "both"` runs the scenario twice on the same virtual clock:

- once drawn on the CPU;
- once drawn by strand-gpu on the container's lavapipe device.

On the GPU leg every surface is `GpuPresent`, and its frames are read back.
`cpu-gpu` then compares the two legs tick by tick:

- the surfaces painted and the surfaces still wanting frames;
- every node by key: boxes within 0.01 px, opacity within 1e-4, and the
  same motions, hidden reasons and text states;
- the `Painter` regions;
- the pixels, within the GPU tests' tolerance: 6 per channel, with at most
  2% of pixels past it.

`[allow] cpu-gpu` leaves a node out and masks its box out of the pixel
comparison, for a `shader` or a dithered gradient.

You can also run one leg:

- `strand-eval run --backend gpu x.scn` runs the GPU leg alone.
- A `backend gpu` or `backend cpu` step switches the shown surfaces from
  their next frame.

Without a Vulkan device the check is skipped, and says so. With
`STRAND_REQUIRE_GPU=1`, as in the container and CI, a missing device is a
failure.

The GPU leg leaves out three parts of the shell's GPU path:

- promotion's wall-clock rules: a surface stays on the GPU for the whole
  run;
- `Presented` replies;
- shader passes, which are dropped and noted.

**The leg cannot catch bug 23.** Both legs flatten the same scene in the
same renderer, so their regions agree by construction. Bug 23 was a
GPU-drawn frame's regions not reaching the compositor, and the surface
manager that sends them is outside the sandbox. That bug's tests live in
strand-surface and strand.

## Local configs and fonts

`eval/scenarios/local/` is gitignored, and is the only place a scenario
may name things outside `eval/`:

- `config = "${VAR}/shell.strand"` or `config_dir = "${VAR}"` names a
  config. `${VAR}` is expanded from the environment.
- `fonts = "local"` shapes text with the fonts in `$STRAND_EVAL_FONTS`.

`run.sh` passes only `STRAND_*` variables into the container, and mounts
only the checkout. So a local config must sit inside the checkout and be
named through a `STRAND_*` variable. `check --gate` refuses local
scenarios, and nothing from them is ever committed: a finding in an
owner's config becomes a committed scenario written from scratch.

## Limits

- **Text is shaped inline.** Lateness is a hold on top of inline shaping
  (`text late`, `text hold`). A real shaping worker's timing is not
  modelled.
- **Wall time is not measured.** Frame times are recorded, but no check
  gates on them. The timing tests in crates/strand-render hold the frame
  budgets.
- **The GPU leg does not see bug 23**, as described above.
- **`jelly` never shows.** Each pointer motion of a drag wakes the loop
  for the surface its last frame left idle, as `Host::input` does;
  `Renderer::update` flattens a preview and the drag frame reuses it,
  where `jelly` never measures a speed. The matrix's jelly cell exercises
  the drag (LIFT), not the squash. It is reported as the product's
  (decisions.md, eval-sandbox), not worked around.
- **The renderer's other wall-clock reads stay wall-clock**: an exit's
  stall, GPU promotion and device retries, the busy window. Their waits
  are set so they never fire in a run.

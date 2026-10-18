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
ships. The checks catch the breakage they predict; every scenario and
matrix cell is also held to an approved recording of its run, which
catches any change at all (see Reference recordings).

The sandbox is evaluation tooling, not product. It is a Cargo workspace of
its own (`eval/Cargo.toml`), outside the root workspace, so `--workspace`
builds, `affected` and the shipped binary never see it. It reads
strand-render through its `eval-hooks` feature. docs/decisions.md
(`eval-sandbox`) records why each part is the way it is.

## Quick start

Everything runs through the container, like every other build here:

```text
scripts/container/run.sh eval check eval/scenarios/curated      # run and check
scripts/container/run.sh eval approve target/eval/latest/<id> --note "why"   # approve a change
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
  runs and checks each scenario, and fails when any scenario fails or is
  CHANGED (it left its approved reference recording). `--gate` is CI's
  form: it refuses `eval/scenarios/local/` and an empty set, and fails on
  an XPASS and on a scenario with no approved reference.
- `run <scn|dir>... [--out DIR] [--frames …] [--reduced-motion]
  [--backend cpu|gpu]` runs and writes the report without checking.
- `explain <run>/<id> [--node SEL] [--frames a..b] [--check NAME]` prints
  a node's frames as a text table. By default it shows the nodes of the
  failing findings.
- `approve <run>/<id>|<matrix>/<cell>@<hz>... --note TEXT` and `approve
  --all-changed <run> --note TEXT` write approved references (see
  Reference recordings).
- `verdict <run>/<id> good|bad --note TEXT`: `good` approves the run,
  `bad` records it as a known-bad recording.
- `refs-check [--base REV]` checks the reference files (CI's side of
  Reference recordings).
- `promote <run>/<id> --as NAME [--xfail CHECK]... [--note TEXT]` writes a
  run's scenario out as a regression (see Matrix).
- `ab <run A> <run B> [-o DIR]` makes one page of two reports (see A/B).
- `matrix [--effects a,b|all] [--hz 60,144] [--singles] [--jobs N]
  [--out DIR]` runs the effect matrix.
- `bench <run>... [--out FILE]` prints each scenario's cost table (see
  Cost).
- `new <id> [--config F] [--from-steps FILE] [--to DIR]` writes a
  scenario skeleton (see Scaffolding).

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
| `compositor_poses` | false | The compositor applies surface poses (`Renderer::set_compositor_poses`, as on Hyprland): a surface's `enter`/`exit` pose is committed as its pose, its content painted at rest. |
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
is an error at that line, except in an `intend`: a node its segment
mounts later (`intend #toast enters` before the `set` that shows it) is
named when it first appears, and one that has not appeared by the end of
the segment is an error at the `intend`'s line.

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
- `text late N` holds each one for N ticks after the first frame drew it
  unshaped (laid out with no room of its own, a list's row's by its
  estimate; decisions.md late-text-dip).

A release `via paint` delivers just before the next paint. A release `via
waker` delivers between frames and runs `Renderer::update`, as the
runtime's `text_ready` wake does.

## Driver

One scenario runs in one process: the logic VM, the input router and the
renderer, wired as the runtime wires them (crates/strand/src/demo and
run/). What the shell's main thread does when logic's diff arrives, the
loop wakes, an input event comes, a surface is attached, configured or
detached, or a frame is painted is one function per reaction in
strand-render, `strand_render::host_loop`, and the driver runs the same
functions (docs/decisions.md eval-host-loop): the order inside each
(observe, apply, settle, the input wake, update, sync, the after-paint
facts and wakes) is the shell's by construction, and `eval/tests/drift.rs`
fails if the driver takes one of their steps by hand. After each reaction
the loop turns: a wake the reaction owed runs, then logic answers what
routing told it, each as a reaction of its own, never inside the one that
caused it (as the shell's loop runs its ping and logic's diff after the
callback that caused them). The renderer's own timers (a tooltip's delay,
a capped clock's tick)
and the router read the virtual clock. Each tick does this, in order:

1. The tick's input steps go through the router, each event a reaction
   (`host_loop::input`): input that leaves an idle surface wanting a
   frame wakes the loop.
2. Logic runs its timers and steps to `t`. Only a diff with something in
   it is applied (`host_loop::apply`, as the logic thread sends), then the
   router settles and surfaces follow.
3. The loop wakes between frames if a waker text is delivered or the
   renderer's next wake is due: the router ticks and `Renderer::update`
   runs (`host_loop::woke`, the shell's `text_ready`).
4. A frame held for logic's answer to its layout facts (an idle surface's
   container query) gets it first.
5. Paint texts are delivered, then every surface that wants a frame paints
   once at `t`. A paint that leaves surface changes wakes the loop
   (`host_loop::painted`).
6. The frame's traces are collected, and what the frame laid out goes to
   logic; its answer lands before the next frame.

Text is shaped inline, where the shell has a text worker that wakes the
loop for each layout it delivers: the driver wakes the loop for each
layout shaped inline, as that wake would. The renderer's wall-clock holds
are neutralised with its own setters (the query hold excepted: logic always
answers in time), so a run is a function of its scenario. Surfaces are
attached (with the screen as their output's bounds), placed and configured
at the size their spec asks, as the surface manager does, each through the
host loop (`attached`, `configured`, `detached`). A run that leaves a
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
  drag, time), and which of its own colours springs (fill, border, text);
- its authored axes;
- the parts of its offset (inherited, and of that the part in flight;
  written, spring, glide, morph);
- why it is hidden;
- its text state (estimate, shaped or held).

A text also records the line box it asked to be shaped for when its
unbounded layout is wider than its box (`fit`), and its glyphs' ink as
drawn: the layout it drew, where it drew it, clipped where the frame clips
it (`ink`, before transforms). Both come from strand-render's
`eval-hooks`, as does each node's `paint`: the fills, borders, shadows and
text paints it drew itself, from the display items the flattener gave it
that frame, and for each text the source its glyphs came from (the text a
layout was shaped from, kept by the renderer while tracing is on, and the
glyphs of it drawn when not all were). A frame with a theme swap's
crossfade playing records its weight per surface (`fades`).

Each frame also records the screen's scale it was drawn at, whether
logic's diff or input reached the renderer since the frame before, where
each shown surface lies on the output (`origins`: a content-sized popup
the compositor re-centres as it grows moves), and, with
`compositor_poses`, the pose each painted surface commits with its frame
(`poses`), read right after its paint as the surface manager reads it.
A painted surface's size, origin and image are read there too: a paint
that leaves its surface a new size (a held surface let go) has it
configured before the tick is collected, and the frame shows the buffer
that paint drew, where it lay.

The checks read the trace as the eye sees it (`RunTrace::on_screen`):
every row moved with its surface since the surface was first placed, and
a committed pose applied (its opacity multiplied into every row's, its
scale and offset into every visual box, the rows authored on the axes it
moves). A trace whose surfaces never moved and were never posed is read
as it is.

With the `redraw` check, the trace counts the painted frames compared
with their full repaints (`redraw`). Each frame records, for each surface
whose buffer differed at all, the pixels past the tolerance, how many of
them were stale, the worst difference, the region, what the frame redrew,
the leg and the repaint's image (`redraw`). Neither is in the digest.

Once per node the trace records what the rule checks read from its
config when it was first seen: its `enter` and `exit` poses (an exit
mirrors its enter; a pose's width and height too), a container's
`stagger` step, whether it clips what it holds (`clip: true`, a
`scroll`, a `list`) and whether it is rigid in a full row (a length
width or `min_width`, `shrink: 0`, an image or icon, a text with no
ellipsis). Per surface it records the axes its content sizes (`content`:
a popup or panel with no `width`, or no `height`).

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
| `teleport` | A moving node's edge moves at most `max(teleport_px × 60 / hz, 0.75 × what is left) + tol` per tick; what is left is the travel (the span of its way, so a spring sent out and back in one segment travels its excursion), or its own and its ancestors' glide (or drag) still in flight. A jump with no motion (layout snapping) is noted, not failed. On w and h, only motion that sizes a box counts, and a glide retargeted from its layout is held on x and y only, on an axis its glide moves it on (a node gliding across while a size spring lays it out anew down follows its layout down, as the spring's neighbours do). |
| `vanish` | Nothing blinks out and back within three ticks, shows for one frame only, pops in past half its enter pose (each time it comes onto a surface: a popup opened again plays its pose again; a shared morph plays in place of the pose, so a first frame that morphs is `coherence`'s morph start to judge), or leaves a damaged frame blank while something inside its surface's root shows. With `compositor_poses`, a closing surface never commits a pose back toward rest before it goes (`pose back`, bug 7). An exit's last frame is at its exit pose (`pop-out`, see below). |
| `coherence` | One frame hangs together. `inherit`: a node is drawn at its laid offset from its parent, its parent's paint offset carrying it. `pivot`: scale, rotate and a morph's scale turn a node about its box's centre, through its ancestors' transforms. `morph start`: a shared morph's first frame is one share (at most 0.5) of the way from the box its name was last drawn at to its own, on all four edges. `stacking`: a node in flight never goes below an overlapping sibling while their layout keeps its order. |
| `ink` | A text's glyphs stay inside its box, give or take 2 px (`spill`: a stand-in drawn unclipped), and an ellipsised text never draws less of itself as its box widens (`over-cut`). |
| `colour` | A node's own fill, border or text colour that springs is drawn, frame by frame, on the straight way between where it started and where it rests in premultiplied OKLab, within 3 steps of 1/255 (`off its way`). See below. |
| `contain` | A surface sized by its content holds what it lays out, mid-motion too (`cut`), and an ellipsised text gives way before it is laid past its surface's edge (`give way`). See below. |
| `tiling` | Two edges at one place in layout are drawn on one physical pixel, at every scale (`apart`): no seam between boxes that touch, no pixel drawn twice. See below. |
| `stagger` | Children mounting together under `stagger:` each leave their enter pose within a frame of their turn, one step apart in child order (`start`). See below. |
| `twin` | The run with motion reduced ends each settled segment where the run does, time-driven nodes included (their clocks stand at t = 0 at a settled rest in both). |
| `determinism` | Two runs have equal digests. Curated scenarios always run it. |
| `budget` | Each frame stays under `[budget]`. Wall times never gate. |
| `reference` | The run is its approved reference recording within a quantum, and does not come back to one marked bad (see Reference recordings). |
| `expect` | The `expect` steps. It always runs. |
| `regions` | Opt-in. Where a surface takes input follows what it draws. |
| `cpu-gpu` | Comes with `backend = "both"`. The CPU and GPU legs agree (see CPU and GPU). |
| `redraw` | Every painted frame's buffer, left by a partial redraw at its buffer's age, is the same frame repainted in full into a fresh buffer: exactly on the CPU, and within one step per channel on the GPU leg (`backend = "both"`). |

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

**`redraw`.** The renderer repaints only a frame's damage, widened by
the buffer's age, over what the buffer kept from the frames before.
Damage that misses something that changed leaves stale pixels, and no
geometry check sees them: the nodes are where they belong, and the
buffer is not. So after every paint the harness draws the same frame
again in full into a fresh buffer and compares the two:

- On the CPU, `Renderer::repaint_fresh` draws the display list that
  frame drew, at its time, with a raster that has nothing cached, and
  blends it over the crossfade snapshot when the frame was blended.
- On the GPU leg, `Renderer::fresh_gpu_frame` lowers the frame again with
  nothing cached and every pixmap uploaded with it. A device of its own
  draws it, and each frame retires the last one's uploads. A GPU frame
  always redraws the whole buffer, so what this holds is the state the
  GPU and the renderer keep between frames: uploaded images, cached
  shadows and gradients, offscreen groups.

Both repaints read the renderer and change nothing a later frame reads.
A run's digest and its frame images are the same with the check on and
off (`eval/tests/redraw.rs`).

Both sides are drawn by the same code on the same machine from the same
display list, so the tolerance is as small as rounding allows:

- On the CPU it is 0, and any differing channel counts. The raster draws
  each pixel in the same fixed cell whatever the damage, so nothing
  rounds differently.
- On the GPU leg it is one step per channel. The fresh device places each
  uploaded pixmap (a glyph atlas page, an image) somewhere else in its
  image atlas than the presented device does, and a pixmap drawn under a
  transform (a rotated, scaled or sheared glyph) is sampled at
  coordinates offset by that place. The rounding can move a channel by
  one. A stale or missing upload differs by far more.

A difference within the tolerance is an info line. A failure names the surface, the leg and the tick (the count of
later ticks follows). It gives the number of pixels that differ, the
worst channel difference and the bounding box of the region, in buffer
pixels. It also says how many of those pixels lie outside what the frame
redrew: those are stale, left by a partial redraw. The full repaint's
image is kept beside the frame's (`frames/<hash>.png`, named in the
finding). A painted frame that was not compared also fails the check, so
the check cannot go blind. The twin and the determinism run are not
compared.

The sandbox's surfaces each keep one buffer, which the compositor has
released by the next frame, so every frame is painted at age 1, a state
the shell's buffers also reach. The check therefore holds each frame's
own damage. It does not hold the union with earlier frames' damage that
an age of 2 or 3 adds.

**The rule checks** (`colour`, `contain`, `tiling`, `stagger` and
`vanish`'s `pop-out`) each hold one guarantee of docs/design.md about how
a motion plays, read off the trace with no reference. Each was kept only
with no false positive over every curated and regression scenario and the
whole matrix at 60 and 144 Hz (decisions.md eval-rules). An `intend`
says a node moves, not how, so it does not exempt from them; only an
`[allow]` does.

- **`colour`.** design.md springs colour on `$motion.effects`, a theme
  swap's palette in OKLab, and a colour's channels are premultiplied OKLab
  plus alpha (decisions.md 2026-10-04 render, wave3-theme t2). A spring
  moves every channel along one curve, so the colours drawn lie on the
  line between the two ends: a fade from transparent keeps its hue (it
  never passes a darker colour), and blue to yellow passes OKLab's light
  middle, not sRGB's grey. It holds a node's own `bg`, border and text
  colour where its own spring of that kind moves it (`Motions`
  `FILL_SPRING`, `LINE_SPRING`, `INK_SPRING`), over a segment from the
  colour at `s` to the colour at `e`. A frame's colour is compared with
  the nearest point of the line (from -0.5 to 1.5 of the way, for
  overshoot), drawn as the raster draws it, in premultiplied sRGB: within
  3 steps of 1/255 on every channel. It skips a colour the token graph
  derives each frame (a theme swap's derived tokens, the contrast guard),
  one retargeted mid-flight (a frame logic or input drove), one keyframes
  or time drive, gradients, and span colours.
- **`contain`.** `cut`: a `popup` or `panel` with no `width`, or no
  `height`, is sized by its content on that axis; it grows to its content
  at once and is held while its content springs smaller (decisions.md
  popup-shrink), so nothing it holds is drawn past its edge on that axis.
  What is past both where it started the segment and where it rests (a
  spring's overshoot) is excused, and so is a node moved by authored
  motion on that axis (offset, pose, scale, turn) or held inside a
  clipping ancestor (`clip: true`, a `scroll`, a `list`). The allowance is
  `tol` plus a quantum. `give way`: a full row's children give way by
  `shrink`, an ellipsised text's min-content is 0 and a surface root never
  widens to its content (design.md "Layout", decisions.md wave3-pixels),
  so an ellipsised text drawn whole is never laid past its surface's
  edge, unless something that cannot give way puts it there: it or an
  ancestor past the edge with a width of its own, `shrink: 0` or a
  `min_width`, an image or icon, or a text with no ellipsis inside that
  ancestor and past the edge too, or a clipping ancestor.
- **`tiling`.** A box's edges are snapped from where they fall
  (decisions.md wave3-pixels), so two that share an edge in layout share
  its physical pixel. On every painted frame, among the nodes shown on
  one surface that nothing moves (no motion of their own or inherited, no
  offset, scale or turn), every two edges on an axis laid at one place
  (within 1/128 px) are drawn (their `visual` boxes, snapped) within
  1/32 px. A box in motion is placed by its layer's whole pixels and is
  not held.
- **`stagger`.** "Delays each child's `enter` this much after the one
  before" (the schema; design.md `stagger: 30ms`). The children of a
  container with `stagger:` first drawn at one frame after boot are a
  batch, numbered in paint order. Child `n` is due `n` steps (at most
  1 s) after the batch's change, a period before its first frame, and its
  first frame off its enter pose must come within a period after that.
  It skips a child without a pose, one whose text is on its way (it waits
  for its text, decisions.md late-text-dip) and reduced motion.
- **`pop-out`** (in `vanish`). "A removed subtree plays `exit`, then
  unmounts" (design.md "Poses"). The last frame an exiting node is drawn
  in shows it at its exit pose (its `exit`, else its `enter`): its own
  opacity within 1/255, its offset within `tol`, its scale within 0.005
  and a pose's width or height within `tol`, or nothing at all (its
  effective opacity at most 1/255). A slide to an edge, whose offset is the
  node's size, is held on its opacity only. Reduced motion is not held.

**Exempting a node.** Three mechanisms, from narrowest to widest:

- `intend #x moves x` in a segment exempts that node and its subtree on
  those axes, in that segment only (not from the rule checks).
- `[allow] check = [{ node, why }]` exempts a node from one check for the
  whole scenario. The reason is required and shown in the report.
- `expect_fail = { check = "why" }` turns the check's failure into XFAIL.
  If the check passes, it is an XPASS, which fails the gate until the line
  is removed.

None of these is for making a real finding pass. A finding the owner
accepts gets a scenario that says why.

## Reference recordings

The checks catch the breakage they predict. A reference recording catches
any change: every committed scenario and every matrix cell, at 60 and
144 Hz, has an approved recording of its run, and a run that differs from
it fails as CHANGED until the change is undone or approved.

### What a recording holds

Per painted tick:

- the surfaces painted;
- per surface shown: its size, where it lies on the output, the pose the
  compositor applies and its input region;
- per node on screen: its drawn and visual boxes, effective opacity,
  scale, rotation, hidden reason and text state, and its place in its
  surface's paint order (`ord`);
- per text: the ink of the glyphs it drew (`ink`) and, when it is cut to
  its box, the width it was shaped for (`fit`);
- per node, what it painted itself that tick, read from the flattened
  scene, never from pixels: its fills (`bg`), borders (`bd`), drop shadows
  with the area each may cover, before the node's transforms as `ink` is
  (`sh`), and its texts' paints (`fg`) with
  the text each one's glyphs were shaped from (`gl`). A roll's two texts
  are both there, and a text of which only some glyphs were drawn names
  them (`gl=["43","41[1]"]`: "43" whole and the second glyph of "41",
  rolling out), so drawing the wrong digit, or a stale one, changes it;
- per surface, the weight of a theme swap's crossfade while one plays
  (`f`).

A surface not painted at a tick keeps what it showed, as the checks read a
trace. The laid box, motions and offset parts are not in it: they explain
a change, and the trace has them. Radii, blur and pixels are not in it
either (see Limits for what covers those).

A colour is straight-alpha sRGB, `rrggbbaa`, each channel clamped as the
raster draws it; a gradient is its kind, angle and stops
(`lin45:ff0000ff@0:0000ffff@1`); a shadow is its colour and extent
(`000000aa@9,30,108,70`); a text paint with span colours adds them after
`+`. Each value is quantised: boxes, origins, pose offsets, regions and
shadow extents to 1/64 logical px, opacity, colour channels and the
crossfade weight to 1/255, scale to 1/1000, rotation to 1/100 degree. A run matches its reference when every value is within one
quantum of it. A last-bit difference that rounds the other way never
counts; anything larger does, as does a tick painted in one and not the
other, or a node shown in one and not the other.

### The files

`eval/refs/<id>.ref` holds a scenario's recordings, and
`eval/refs/matrix-<hz>.ref` those of every cell at that rate. A file holds
blocks, one per recording, sorted by scenario. Each block starts with its
meta lines:

```text
ref "hover-popup"
verdict good
note "initial references (eval round 2), a2d4d726"
date 2026-10-18
rev "a2d4d726+changes"
approver "claude"
scenario 9d54c3d1545f228d
seal c07234992c037f23
```

`rev` is the build the run came from, with `+changes` when it had
uncommitted ones. `scenario` hashes the scenario text it ran, so a
CHANGED line can say the scenario changed since. `seal` hashes the whole
block.

The recording follows, written so a change diffs line by line:

```text
S1 0 - "bar Top"                    surface 1: attached at k0, never detached
N0 "<key>" "box#after"              node 0: its key and label
k0 p1                               tick 0, surface 1 painted
s1 z=420,60                         surface 1's size (o= origin, P= pose, i= input)
n0 x=82 y=10 w=20 h=12 txt=S        node 0: its drawn box, its text shaped
k1                                  tick 1, the same surfaces painted
n0 x=83                             only what changed since its last line
n3 -                                node 3 left the screen
```

A node's first line leaves out what is at its default: the first surface,
`v=d` (a visual box equal to the drawn one), `op=255`, `sc=1,1`, `rot=0`,
`hid=-` and `txt=-`, and `-` for each paint field it did not draw
(`bg`, `bd`, `sh`, `fg`, `gl`). Several of one kind are joined with `/`
in paint order; `gl` is a JSON list.

Everything together stays under 5 MB. With each node's paint and glyph
sources (eval-coverage) it is 4.15 MB: the 41 scenarios 156 kB (the 24
recorded before 110 kB, 105 kB without paint), `matrix-60.ref` 1.61 MB
(4.0 kB a cell) and `matrix-144.ref` 2.39 MB (6.0 kB a cell), 401 cells
each. Paint costs little because a field is written only on the ticks
it changes. Compressed, as git stores it, that is about 380 kB. eval/tests/refs.rs
holds the total to 5 MB, a scenario's file to 64 kB and a matrix file to
8 kB a cell. Each is a tripwire for a recording that stopped being
compact, not a cap on scenarios.

### CHANGED

`check` holds each scenario to its approved reference, and the matrix
holds each cell to its rate's file. A run that differs prints its first
difference, by tick, then surface, then node:

```text
CHANGED: hover-popup: k57 box#after drawn.x 101.5 (reference 100); differs at 12 of 140 ticks, on 3 nodes or surfaces, 40 values in all (reference good (2026-10-18, a2d4d726+changes, claude): initial references (eval round 2), a2d4d726)
    k58 text("iiii") visual.w 41 (reference 40)
  approve: scripts/container/run.sh eval approve target/eval/ci/hover-popup --note "why"
```

The indented lines are each other node's first difference. `summary.md`
has them too, with the approve command. The viewer's A/B view puts the
reference beside the run: the reference is the B side, drawn as
wireframes from the recording, and the A/B tab lists the first tick each
node differs at.

A scenario with no approved reference is CHANGED under `check --gate`
and in the matrix, and skipped under a plain `check`, so an ad hoc
scenario runs without one. A `--reduced-motion` run is not compared.

### Approving

Approve a run once its change has been seen to be right:

```text
scripts/container/run.sh eval approve target/eval/ci/hover-popup --note "the popup now opens from its anchor"
scripts/container/run.sh eval approve target/eval/matrix/jelly+lean@144 --note "why"
scripts/container/run.sh eval approve --all-changed target/eval/ci --note "why"
```

The note is required, and so is the approver: `--approver NAME`, else
`$STRAND_EVAL_APPROVER` (`run.sh` passes it into the container). With
neither, or with `unknown`, nothing is written. The block records the
note with today's date, the run's revision and the approver. Approve from
a committed build where you can: the revision then names a commit that
reproduces the recording, and `+changes` marks one that does not. A run
that matches its reference already
writes nothing. A changed recording approved under the note its reference
already has is refused: the note says why this recording is right.

`--all-changed` approves every run in the report that left its reference
or has none. On a matrix run that covered a whole rate, it also prunes the
blocks of cells the matrix no longer has. The matrix reports those blocks
as STALE REFERENCE and fails until they are pruned.

`promote` writes a new regression with no reference: approve its run.

The viewer's Approve tab is the same step. It shows the approved
reference, the known-bad ones and the run's difference from it:

- **Good** copies the approve command with the note typed there;
- **Bad** copies `verdict <run>/<id> bad --note …`.

### Known-bad recordings

`verdict <run>/<id> bad --note TEXT` adds the run to its scenario's file
as a block with `verdict bad`. A later run that comes back to it fails
`reference`, "reproduces a known-bad recording". It counts as back when
every value is within `tol` on geometry and a quantum elsewhere. `verdict
<run>/<id> good` is `approve`.

### The CI side

`refs-check` runs before the gate, in scripts/eval/ci.sh step 5. It
checks four things:

- every file parses as `approve` writes it;
- every block's seal holds, so nothing was edited by hand;
- every block has a note and an approver other than `unknown`;
- every block lives in its own file.

With `--base` (the merge base with main; GitHub's eval job fetches the
history for it), it also refuses a recording that changed since the base
under its old note. Every approval is therefore a line a reviewer reads.

The seal is an unkeyed hash of its block. It catches a hand edit made by
mistake, and nothing more: anyone can recompute it, as `approve` does. The
approver is a name, not a signature. What makes an approval trustworthy
is review: the reference diff and its note are in the commit, and the
owner reads them before a merge.

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
- with a node's path plotted per axis;
- with an Approve tab: the run's approved reference and the command that
  approves it (see Reference recordings).

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
reorders and nested FLIP scopes, a content-sized popup whose height
springs, morphs, reveal, stagger, lean, jelly, keyframes, a drop shadow,
rolling text, a colour change, late text, screen scales, and the GPU leg
(`gpu`). Each one is a set of fragments
filled into `eval/matrix/fixture.strand.in`, then run with the standard
steps (on, wide, off). The matrix runs every effect alone and every
unordered pair at each rate.

Two effects that set the same prop of one slot cannot pair, and their cell
is `n/a: conflict`. Effects carry what their motion moves: `flip-reorder`'s
keyed boxes each hold a child, so a glide carried to a subtree is checked
(`coherence` inherit), and `reduced-motion` turns its subject with
`rotate: t * 90deg`, so a time signal under reduced motion is checked
(`twin`, `teleport`). A cell fails the matrix in four cases:

- it fails a check `eval/matrix/known.toml` does not list for it, or a
  listed check on a node the entry does not name;
- a check listed for it passes (stale: remove the entry);
- it is CHANGED: it left its approved reference in
  `eval/refs/matrix-<hz>.ref`, or has none (see Reference recordings);
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
when the finding stays open. The whole matrix, at 60 and 144 Hz, runs on
every push as scripts/eval/ci.sh step 6 (about 90 s on the laptop with
8 jobs), so a branch whose change moves a cell cannot pass CI until that
cell's new recording is approved. It also runs nightly and on demand in
`.github/workflows/eval-matrix.yml`, with the mutants.

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

## Cost

Every painted surface records its deterministic cost proxies (damage_px,
raster_px, items, records, layout_passes, layout_nodes, text_pending,
text_held) and its wall times (flatten_us, text_wait_us, raster_us).
`bench` reads one or more runs and prints, per scenario, the p50, p95 and
max (nearest rank) of each, as a markdown table ready for
docs/benchmarks.md:

```text
scripts/container/run.sh eval bench target/eval/latest --out target/eval/bench.md
```

Each painted surface of each frame is one sample; runs of the same
scenario pool theirs, so a wall time's spread across runs shows in its
p95. The wall times are marked advisory: they vary with the machine and
never gate. Only `[budget]` lines, on the deterministic metrics, gate.

## Scaffolding

`new <id>` writes `eval/scenarios/local/<id>.scn` (`--to DIR` elsewhere),
a header, a bar to write the shell into (or `config = "<F>"` with
`--config F`, which may name `${VAR}`, see Local configs and fonts) and a
steps section that starts and ends with `rest`. It never replaces a file.
`--from-steps FILE` maps a shots-keys steps file one line to one step:

| shots-keys | step |
|---|---|
| `ipc set <path> <value>` | `set <path> <value>` |
| `pause <ms>` (`250`, `250ms`, `1.5s`) | `wait <ms>` |
| `burst <n>` | `frames <n>` |
| `pointer` with a point that clicks (`click`, `tap`, `press`, a button) | `click X,Y [button]` |
| `pointer` with a point and a time, from a known place | `glide <last> -> X,Y over MS` |
| `pointer` with a point | `pointer X,Y` |
| `notify …` | `notify …` |

A point is `X,Y` or two numbers. A line it does not understand, or one
whose mapping the step parser refuses, is kept as a `# shots-keys:`
comment; a mapping that drops words keeps the original line as a comment
above its step. Nothing of the original is lost, and the skeleton parses.

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
- **Wall time is not gated.** Frame times are recorded and `bench`
  reports them, marked advisory, but no check gates on them. The timing
  tests in crates/strand-render hold the frame budgets.
- **`ink` sees the layout drawn, not the pixels.** It reads the glyph
  ink of the layout the frame drew, before transforms; a glyph clipped
  by an ancestor's clip only is not told apart from one drawn.
- **The GPU leg does not see bug 23**, as described above.
- **References hold paint as the scene says, not pixels.** A recording
  holds geometry, opacity, transforms, paint order, text state and ink,
  and each node's fills, borders, shadows, text colours and glyph
  sources as the flattened scene gives them, not a pixel hash. vello_cpu
  picks its SIMD level from the CPU it runs on, and nothing here shows
  one hash agreeing between the laptop's container and GitHub's runners.
  So a colour that snaps instead of springing, springs the wrong way or
  ends wrong, a shadow left behind its box, a crossfade's weight and a
  roll drawing the wrong text are CHANGED; what the raster then does with
  them is not. What covers that:
  - gradients, corners, borders, shadows and blur rasterised wrong:
    crates/strand-render/tests/paint.rs (`gradients_linear_radial_conic`,
    `corners_per_corner_full_and_squircle`,
    `borders_solid_gradient_and_on_a_pill`,
    `elevation_shadow_lists_are_drawn_and_cached`,
    `blur_tints_until_the_compositor_blurs`) and layers.rs, pixel tests
    on rendered buffers; a colour's spring curve itself:
    crates/strand-render/tests/motion.rs (`colours_spring_in_oklab`);
  - the pixels of a theme swap's crossfade (the recording has its
    weight): crates/strand-render/tests/theme_swap.rs
    (`an_unreadable_spring_crossfades_from_a_snapshot`,
    `a_colourless_table_mid_crossfade_keeps_fading`);
  - stale or missing pixels after a partial redraw, and the GPU drawing
    differently from the CPU: `redraw` and `cpu-gpu` here.
  Radii, blur radii, an image's pixels and a reveal mask's progress are
  not recorded. `gl` names the text a
  layout was shaped from, not where an ellipsis cut it: the cut width
  (`fit`) and the ink record that. A text's paint is recorded once per
  run of glyphs drawn, so two texts with the same source, colour and
  glyphs drawn on one node are one entry.
- **No check holds a retarget's velocity.** design.md: "Retargeting
  mid-flight keeps velocity". A spring retargeted with its velocity
  dropped is drawn as a smooth curve that differs from the right one by a
  few pixels over a few frames, and the spring's stiffness and damping
  are not in the trace, so no bound on the drawn frames alone told the two
  apart at 60 Hz without failing correct reversals (decisions.md
  eval-rules). The references catch it where a scenario reverses a
  spring (`heldc-spring-reversal`, `heldc-list-reorder`,
  `title-pill-cut-late`, and the matrix).
- **The rule checks read the scene, not the pixels.** `colour` reads
  the colours the flattened scene gives, `tiling` the snapped boxes the
  flattener places; how the raster fills them is the pixel tests'
  (above).
- **`redraw` holds one buffer age.** Every frame is painted at age 1,
  so damage widened by older frames (ages 2 and 3) is not compared.
  On the GPU leg it compares against a fresh device, allows one step per
  channel, and only synthetic traces have shown it failing there. The
  step was measured on the laptop's lavapipe: the first nightly on
  GitHub's runners must show no `redraw` failure on the GPU leg.
- **The renderer's other wall-clock reads stay wall-clock**: an exit's
  stall, GPU promotion and device retries, the busy window. Their waits
  are set so they never fire in a run.

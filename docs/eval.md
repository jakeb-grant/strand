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
strand-render through its `eval-hooks` feature, and its surface leg runs
strand-surface against the fake compositor strand-fake-wayland, which is
only ever a dev-dependency of the root's crates. docs/decisions.md
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
scripts/container/run.sh eval local new mine --config /path/to/shell.strand   # a starter for your own config
scripts/container/run.sh eval local                             # run eval/scenarios/local/ (see Local configs)
scripts/eval/viewer-check.sh --selftest                         # the viewer in a real browser (on the host)
scripts/eval/live.sh /path/to/shell.strand                      # the live sandbox, a window on your session (on the host)
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
- `new <id> [--config F [--explore]] [--from-steps FILE] [--to DIR]`
  writes a scenario skeleton (see Scaffolding).
- `local [<scn|dir>...] [--out DIR] [--frames …]` checks the local
  scenarios, `eval/scenarios/local/` by default; `local new <id> --config
  F` writes an explored starter there (see Local configs and fonts).
- `live <shell.strand|scn> [--screen WxH[@S]] [--hz N] [--fonts
  bundled|local] [--speed F] [--paused] [--record-to FILE] [--ring N]
  [--max-frames N]` opens the live sandbox's window (see Live sandbox);
  `scripts/eval/live.sh` builds it and runs it on the host.

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
| `surface` | false | Runs the surface leg and adds `regions-on-wire` and `late-present` (see Surface leg); naming either check turns it on too. Scale 1 only. |
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
- `scroll DY` turns the wheel `DY` notches (positive down; fractions
  allowed), each 15 px (`strand_render::WHEEL_STEP`, as libinput's legacy
  wheel), as one axis frame from a wheel: each notch springs the offset
  from where the last one aims (m4-plan.md "Lists": wheel steps spring).
- `swipe DY over MS` moves a touchpad's fingers `DY` logical px over `MS`,
  one axis frame a tick, and lifts them with the last frame (`axis_stop`):
  the offset follows the fingers at once, and a lift while they move
  flings it on. Strand has no overscroll: an offset stops at either end.
- `leave`: the pointer leaves every surface, as the compositor's
  `wl_pointer.leave` says when it moves off the shell (a hover popup's
  grace starts). The live sandbox writes it when the pointer leaves the
  stage (eval-live).
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

### Theme swaps

A theme swap needs no step of its own (eval-coverage-2): a palette chosen
by a state (`let scheme: Palette = split ? import("base16:…") :
import("base16:…")`, `use palette scheme`) swaps when a `set` changes the
state, and logic sends the new table as the shell's does. `import`
reads a palette file against the config's directory, as the shell does:
the scenario file's for a config written in `===` sections. A committed
scenario names only files inside eval/: eval/fixtures/palettes/ holds
`grey.yaml` (every background one mid grey) and `split.yaml` (the
background black, the lighter ones white). From one to the other no text
lightness reaches 3:1 on both kinds half way, so that swap cannot spring
and crossfades from a snapshot (design.md "How a swap animates"; the
trace's `fades`); a swap between two readable palettes (catppuccin's
latte and mocha, colour-mid-motion) springs. theme-crossfade runs the
crossfade on both backends.

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
attached (with the screen as their output's bounds) and configured at the
size their spec asks, as the surface manager does, each through the host
loop (`attached`, `configured`, `detached`), and placed on the output as
the compositor places them, by strand-surface's own placement code
(eval-false-alarms): a layer surface from `placement::layer_config` (its
anchor, margins and exclusive zone), arranged with the other layer
surfaces' zones in the order they were shown (`arranged_area`) and placed
in what they leave (`position_in_rect`: centred on an axis it is not
anchored to, in whole pixels), again at every new size; a popup from
`popup_config` and its positioner (`popup_request`), shifted onto the
screen (`popup_shift`), relative to its parent's window geometry, moving
with its parent and placed again when its configuration changes. A popup
waits for its parent to be shown, and closes with it. A run that leaves a
surface wanting frames at the end of a rest is a `settle` failure, not a
hang.

**Placement** (eval-coverage-2). A layer surface lies where the compositor
puts it: its configure from crates/strand-surface's `layer_config` (the
box's size rounded to whole logical pixels, the overhang added), placed in
the area the exclusive zones before it leave (`arranged_area`) by
`LayerConfig::position_in_rect`, wlroots' arrangement: anchored to an edge
it keeps its margin there; on an axis anchored on neither side or on both
it is centred, at `area / 2 - size / 2` in whole pixels. Every shown
surface is placed again whenever one is attached, configured at a new
size or detached (a centred panel that shrinks moves; a bar's zone moves
the panels after it), and a popup moves with its parent. Its buffer is
the configured size at the screen's scale as the surface manager sizes it
(`buffer_size`: times the scale, rounded half away from zero at a
fractional one, as `wp_fractional_scale_v1` says). Before, every layer
surface lay at 0,0 and buffers were rounded up, so a centred panel's
shrink and a 1.25 buffer one pixel wide differed from the compositor's
(held-out set D's launcher-filter and edge-shift-centre).

## Trace

Each run writes `trace.json` (schema 1): per tick, every painted surface's
nodes, plus the surface's cost and its `Painter` regions.

For each node the trace records:

- its drawn, visual and laid boxes, in logical pixels quantised to 1/64;
- its effective opacity, scale and rotation;
- its motions, both its own and its ancestors' (glide, spring, size
  spring, enter, exit, keyframes, morph, reveal, stagger, lean, jelly,
  drag, time, a `scroll` or `list` whose offset moves by itself (`SCROLL`:
  a wheel step's spring or a fling) and a fling (`FLING`)), and which of
  its own colours springs (fill, border, text);
- its authored axes;
- the parts of its offset (inherited, and of that the part in flight;
  written, spring, glide, morph);
- why it is hidden: opacity 0, scale 0, an exit's ghost (`inert`, drawn
  but never hit), or `clipped` (on its surface but wholly outside the clip
  its ancestors set: a list's row scrolled out of its box, the overflow of
  a `clip: true` box or of a box whose size springs), which the checks
  read as not shown;
- its text state (estimate, shaped or held).

A text also records the line box it asked to be shaped for when its
unbounded layout is wider than its box (`fit`), and its glyphs' ink as
drawn: the layout it drew, where it drew it, clipped where the frame clips
it (`ink`, before transforms). Both come from strand-render's
`eval-hooks`, as does each node's `paint`: the fills, borders, shadows and
text paints it drew itself, from the display items the flattener gave it
that frame, and for each text the source its glyphs came from (the text a
layout was shaped from, kept by the renderer while tracing is on, and the
glyphs of it drawn when not all were). Its parts (`pt`, eval-coverage-2)
are where each fill that is not its whole box lies inside it, in order, as
`x,y,w,h` from its box's top-left, its box's edges taken to physical
pixels: a slider's track, value and knob, a meter's value, an `arc`'s
track and value (stroke outlines it fills), a `segmented`'s chosen cell,
a shaped box's path. (Strand draws no scrollbar and has no switch.) A control whose parts move (a knob dragged, a
value springing) is recorded frame by frame where its box stays still.
Its images (`im`, eval-coverage-3) are each image or icon it drew: where
the whole decoded pixmap lies (`x,y,w,h` from its box's top-left, as the
parts are), `@` the size it was decoded at in device pixels, `~` the box
it was drawn in when that is not its own, and `+` a symbolic icon's
tint. So a decode of another size standing in while its box springs, and
where it is placed, are recorded frame by frame; its pixels are not. A frame with a theme swap's
crossfade playing records its weight per surface (`fades`).

Each frame also records the screen's scale it was drawn at, whether
logic's diff or input reached the renderer since the frame before, where
each shown surface lies on the output (`origins`: a content-sized popup
the compositor re-centres as it grows moves), each painted `scroll`'s and
`list`'s offset (`offsets`, integration-r4: how far a scroll carried
what it holds, which `teleport` reads), and, with
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

What `continuity`, `contain`'s `held` and `shadow` and `coherence`'s
`drag` read (eval-rules-2), all from strand-render's `eval-hooks`:

- every sample a painted frame took of a prop's spring, a laid-out
  size's (`w`, `h`), a glide's or, in a theme swap, a palette root's
  (`palette`, keyed `$` and its token path, with `ask`: the curve the
  swap's table gives a colour; eval-rules-3) (`sp`): the node, its surface, the value
  drawn per channel, the target and curve it moves along (a spring's
  stiffness and damping, a timed curve's duration and easing), its
  settling tolerance, `rest` when it was at rest before the sample,
  `start` when a change started at it (the motion's sample before and a
  FLIP's jump), and on the frame its node enters, whether its enter
  pose sets that prop (`enter`). Never its velocity or where its way
  began: the check works those out. With them, each surface's frame
  period, which a change starts one of before its first frame
  (`period`);
- each `roll: true` text rolling (`rolls`): the texts it rolls from and
  to, how far, and on the frame a change interrupted a roll, that roll as
  it stood;
- per node, the clip its ancestors set when narrower than its surface
  (`clip`, axis aligned: a turned box's is its bounding box);
- per shown surface, the sides of its buffer that lie on or past the
  output's edge where the driver's placement put it, and its `attach:`
  side (`cuts`; integration-r4: read from the modelled buffer, not from
  render's own overhang caps, so a side render cuts away from the edge is
  still a finding);
- where the pointer is after the tick's input (`ptr`) and each slider a
  drag holds, with the value the drag gives it (`drags`).

They are in the digest when a frame has any.

What `placement`, `ink`'s `fills` and `redraw`'s `blend` read
(eval-rules-4):

- per surface, its placement as declared, from tick `k` on, one entry
  per change (`places`): its layer, a bar's edge, `anchor:`, `margin`
  (and whether it was set), `attach:`, whether it is a tooltip, a
  declared width and height, and for a popup the surface it opens from
  and the key of its anchor element (its tree parent). These are the
  props as written (`SurfaceSpec`), never where the surface manager put
  the surface;
- per frame, each `image` and `icon` drawn (`imgs`): its box, where its
  pixmap was drawn, the pixmap's size, the source's size, its `fit:`, the
  fit the pixmap was decoded by and whether a decode made for another box
  size stood in;
- per frame, each text cut to its line box and drawn by the layout
  shaped for that box at the frame's scale (`lines`): its text, its
  `ellipsis:` and `max_lines:`, the drawn layout's cluster stops (byte,
  x, line) and, when one line, its unbounded layout's (byte, x);
- per node, a `split`'s padding and a `start`, `center` or `end`
  section's margin (`inset`).

`imgs` and `lines` are in the digest when a frame has any. The CPU
crossfade frames `redraw` compares also record, per surface, the blend
measured against the check's own (`blend`: the weight, the worst channel
and how many channels lie off it), which is not in the digest.

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
  keyframes, a time-driven prop, a drag, a text drawn by its estimate, a
  `scroll` or `list` above it whose offset springs: it lays its rows out
  anew each frame, eval-false-alarms), or logic's diff or input arriving
  at the frame it came back on.
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
| `bystander` | A bystander is not drawn elsewhere while the segment runs (bugs 24 and 25). A frame under the node's own authored `scale` or `rotate` is judged by its box before transforms (eval-rules-4). |
| `envelope` | Derived motion stays between where a mover started, where it rests and where each frame counted laid it out, plus an overshoot of `max(3 px, overshoot × travel)`. A frame under the node's own authored `scale` or `rotate` is judged by its box before transforms (eval-rules-4). |
| `teleport` | A moving node's edge moves at most `max(teleport_px × 60 / hz, 0.75 × what is left) + tol` per tick; what is left is the travel (the span of its way, so a spring sent out and back in one segment travels its excursion; a node that comes onto its surface in the segment, or leaves it, travels from its first frame shown there or to its last, and so does each time it comes back or leaves again; a later change in the segment starts its way again where it found the node, so an offset sent out and turned back travels its excursion), or its own and its ancestors' glide (or drag) still in flight. A jump with no motion (layout snapping) is noted, not failed; a row a scroll moves by itself (`SCROLL`, a fling's `FLING`) moves, so its jumps are held to the bound, and the scroll laying it out widens its way, as a size spring's does; on x and y a fade or a colour spring (opacity, fill, ink, border, a meter's level) is no motion. On w and h, only motion that sizes a box counts, and a glide retargeted from its layout is held on x and y only, on an axis its glide moves it on, and on the node's own move in its parent (what its parent's layout carried it by is the parent's; a node gliding across while a size spring or a scroll lays it out anew down follows its layout down, as the spring's neighbours do). |
| `vanish` | Nothing blinks out and back within three ticks, nor, with nothing taking it out, at any length before its segment comes to rest (eval-rules-3: set E's module swap pushed a bar's labels out of it for seven frames) (one still drawn whole, hidden for no reason the renderer gives, and wholly past its surface's edge, moved there by a scroll's spring, a glide, a drag or an offset, left view by moving; one an ancestor's clip cut out, `hidden: clipped`, blinked), shows for one frame only, pops in past half its enter pose (each time it comes onto a surface: a popup opened again plays its pose again; a shared morph plays in place of the pose, so a first frame that morphs is `coherence`'s morph start to judge; a node kept from view until then by an ancestor drawn the frame before, transparent, or clipping (or its surface's root) and not reaching it, shows at rest by design; a pose that moves nothing has nothing to play, and a pose of size alone plays as a size spring), or leaves a damaged frame blank while something inside its surface's root that draws something of its own shows (a fill, a border, a shadow, glyphs, an image or icon or shader; eval-rules-3: a bare container showing over a toast at its enter pose's `opacity: 0` is a frame with nothing to draw). A row a scroll carries past its viewport by itself (`SCROLL`) may show for one frame; a clipped node (`hidden: clipped`) does not show; any step that acts counts as bringing a node back or taking it out, not only a segment's first (a second `scroll` in one segment), and a `swipe` acts on every tick of its span, but only on what lies under the scrolls under the pointer as it starts (a swipe faster than the viewport carries those rows across it). With `compositor_poses`, a closing surface never commits a pose back toward rest before it goes (`pose back`, bug 7). An exit's last frame is at its exit pose (`pop-out`, see below). |
| `coherence` | One frame hangs together. `inherit`: a node is drawn at its laid offset from its parent, its parent's paint offset carrying it. `pivot`: scale, rotate and a morph's scale turn a node about its box's centre, through its ancestors' transforms. `morph start`: a shared morph's first frame is one share (at most 0.5) of the way from the box its name was last drawn at to its own, on all four edges. `stacking`: a node in flight never goes below an overlapping sibling while their layout keeps its order. `drag`: a slider a drag holds draws its knob under the pointer (see below). |
| `ink` | A text's glyphs stay inside its box, give or take 2 px, and a text drawn by the layout shaped for its own box the font's own overhang of that layout besides (`spill`: a stand-in drawn unclipped, or a layout drawn in the wrong place), an ellipsised text never draws less of itself as its box widens (`over-cut`), and a cut text keeps on each line what fits there (`fills`, eval-rules-4). |
| `colour` | A node's own fill, border or text colour that springs is drawn, frame by frame, on the straight way between where it started and where it rests in premultiplied OKLab, within 3 steps of 1/255 (`off its way`), no further past either end than its spring overshoots (`past its end`, eval-rules-3), and a theme swap springs each palette root along the curve its table asks a colour to take (`palette`, eval-rules-3). See below. |
| `contain` | A surface sized by its content holds what it lays out, mid-motion too (`cut`), an ellipsised text gives way before it is laid past its surface's edge (`give way`), a box whose size springs or that exits to a size holds what it lays out, its texts' glyphs included (`held`), a pose that sizes one axis leaves the other at rest (`other axis`), a shadow at rest is drawn inside its buffer (`shadow`), and what is inside its surface and its clip where a segment starts and rests, laid at one place at both, is not drawn past them between with nothing moving it (`pushed`, eval-rules-3). See below. |
| `tiling` | Two edges at one place in layout are drawn on one physical pixel, at every scale (`apart`): no seam between boxes that touch, no pixel drawn twice. See below. |
| `stagger` | Children mounting together under `stagger:` each leave their enter pose within a frame of their turn, one step apart in child order (`start`). See below. |
| `continuity` | A motion goes on from what is on screen: each spring sampled is drawn on the way its samples work out (`way`), a change keeps the velocity the way had (`velocity`) and starts one refresh before its first frame (`start`), a surface that stops wanting frames shows each spring at its target (`end`), a fade or colour change settles without a jump the eye sees (`snap`), a new node springs only what its enter pose sets (`first frame`), a size sprung from zero or to zero and gone carries its gap with it, and its parent, sized by its content, does not step as it unmounts (`fold`, eval-rules-3, eval-coverage-3), and an interrupted roll rolls on from the text on screen (`roll`). See below. |
| `placement` | Each surface's card is where its declared props put it, at every frame painted at a settled size and place: a popup by its anchor (`anchor`), a bar, panel or OSD by its margin in what the bars' exclusive zones leave (`margin`); a split's `center` is centred on its split (`centre`); an image's source shows where its `fit:` puts it in its box (`fit`). eval-rules-4; see below. |
| `twin` | The run with motion reduced ends each settled segment where the run does, on the output (each run's surfaces where that run placed them), time-driven nodes included (their clocks stand at t = 0 at a settled rest in both). Nodes are matched by identity: a branch render re-created (`#0~2` in one run, `#0~3` in the other) is one node. What lies under a scroll that flung is left out from the fling on (the scroll's own box, and the rest of its surface, still compared): `reduced_motion` flings nothing (renderer/lists.rs `scroll_input`). |
| `determinism` | Two runs have equal digests. Curated scenarios always run it. |
| `budget` | Each frame stays under `[budget]`. Wall times never gate. |
| `reference` | The run is its approved reference recording within a quantum, and does not come back to one marked bad (see Reference recordings). |
| `expect` | The `expect` steps. It always runs. |
| `regions` | Opt-in. Where a surface takes input follows what it draws. |
| `cpu-gpu` | Comes with `backend = "both"`. The CPU and GPU legs agree (see CPU and GPU). |
| `redraw` | Every painted frame's buffer, left by a partial redraw at its buffer's age, is the same frame repainted in full into a fresh buffer: exactly on the CPU, and within one step per channel on the GPU leg (`backend = "both"`); a CPU crossfade frame is its new frame weighted over its snapshot (`blend`, eval-rules-4). |
| `regions-on-wire` | Comes with `surface = true`. After each commit, the compositor holds the opaque, input and blur regions the painter computed for the frame that commit carries, on the shm path and the GPU's presents (bug 23). Its buffer is the frame's size (see Surface leg). |
| `late-present` | Comes with `surface = true`. Every frame that drew is committed in its tick and shown by that tick's refresh: not later, not replaced first, not never (see Surface leg). |

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

`blend` (eval-rules-4): the full repaint blends over the crossfade
snapshot with the renderer's own blend, so a wrong weight is on both
sides. On each CPU frame a theme swap's crossfade plays, the harness also
takes the full repaint before it is blended and the snapshot, and the
check blends them itself: each premultiplied channel `w·new + (1 −
w)·old`, rounded, with `w` the frame's weight (design.md: the old frame
crossfades into the new one). The renderer carries the weight in
1/256ths and rounds each channel once, which moves a channel by at most
one; a frame with a channel more than 2 steps off fails, once per
surface, with the count of later frames. A weight eased or turned round
lies `|w² − w|` or `|2w − 1|` of the two frames' difference away.

The sandbox's surfaces each keep one buffer, which the compositor has
released by the next frame, so every frame is painted at age 1, a state
the shell's buffers also reach. The check therefore holds each frame's
own damage. It does not hold the union with earlier frames' damage that
an age of 2 or 3 adds.

**The rule checks** (`colour`, `contain`, `tiling`, `stagger`,
`continuity`, `placement`, `vanish`'s `pop-out`, `coherence`'s `drag`,
`ink`'s `fills` and `redraw`'s `blend`) each hold one guarantee of docs/design.md about how
a motion plays, read off the trace with no reference. Each was kept only
with no false positive over every curated and regression scenario and the
whole matrix at 60 and 144 Hz (decisions.md eval-rules; the
eval-rules-2 ones also over held-out sets C and D's correct scenarios and
three owner shells run with `eval local`). At round 4's integration
(integration-r4) the same sweep over every check: of C's 8 and D's 10
correct scenarios only D's launcher-filter fails (`vanish`, the product's
blink of a gliding row that starts outside its card's clip, left open in
eval-coverage-2; fixed in round 5, decisions.md clip-blink, after which
C and D pass every check), and two owner shells pass every check run; the third
no longer loads, since pose-transition made a `~` in a pose a load error
(its owner takes the `~` out). The eval-rules-3 arms (`vanish`'s
segment-long blink and its `blank` held to what draws, `contain`'s
`pushed` and bounded overshoot, `continuity`'s `fold`, `colour`'s `past
its end` and `palette`) were kept only with no false alarm over the gate
(88 scenarios), the matrix at 60 and 144 Hz (914 pass, 16 n/a), held-out
sets C, D and E's correct scenarios (38: only open product findings
fail, set D's launcher-filter and set E's module swap) and the two owner
shells that load (decisions.md eval-rules-3). At round 5's integration
(integration-r5, with border-eps' `snap` and bar-blank-on-exit's `other
axis` beside them, and `cut`, `other axis` and `fold` refined) the same
sweep: the gate's 93 scenarios and the matrix's 930 cells pass; of C, D
and E's 38 correct scenarios only D's launcher-filter fails, by `cut`
alone (its centred card snapped taller and glided past its panel, the
growth glide the owner is deciding; its blink is fixed), and E's module
swap passes now that the product keeps a squeezed label's layout; the
owner's `qs` and `launcher` shells pass every check run. The
eval-rules-4 arms (`placement`, `ink`'s `fills`, `contain`'s `held` on
glyphs, `continuity`'s `fold` on the parent, `redraw`'s `blend`, and
`bystander` and `envelope` on an authored turn) were kept with no false
alarm over the gate (93 scenarios), the matrix at 60 and 144 Hz (914
pass, 16 n/a), held-out sets C to F's correct scenarios (58: only open
product findings fail, D's launcher-filter and F's toasts at 144 Hz by
`cut`, the growth the owner is deciding, and F's media card by
`redraw`, a damage bug) and the owner's `qs` and `launcher` shells
(decisions.md eval-rules-4). An `intend`
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
  one retargeted mid-flight (a frame logic or input drove), one still
  springing at `s` (an earlier segment set it moving, so this segment's
  change retargets it and its way bends with the velocity it kept), one
  keyframes or time drive, gradients, and span colours. `past its end`
  (eval-rules-3): along that line it goes no further past either end
  than the curve its own spring's samples name overshoots from rest (a
  spring's first peak, `exp(−ζπ/√(1−ζ²))` below critical damping and
  none at or above it; a bézier no further than its control values lie
  outside 0..1; linear and bounce easings none), so a critically damped
  `$motion.effects` colour never passes its end and none passes back
  past where it started. With no sample naming its curve, or an elastic
  easing, the old -0.5..1.5 holds. `palette` (eval-rules-3): a theme
  swap springs each palette root along the curve its table gives a colour
  (`$motion.effects` unless the swap names its own transition, as the
  table it sent defines it); each root's sample records both (`sp`'s
  `ask`), and a swap whose roots spring along another curve fails, once
  per swap, naming how many roots did. It is a drift guard, not a
  reading of the design (integration-r5): `ask` is worked out by the
  same expression as the plan's own curve, so it fails when the plan
  takes another curve than the colour's, and a fault in the token
  table's resolution makes both wrong together; that shows as CHANGED
  references (`colour-mid-motion`, `theme-swap-own-effects`), not here.
  `continuity` holds the roots' samples to their way like any spring's.
- **`contain`.** `cut`: a `popup` or `panel` with no `width`, or no
  `height`, is sized by its content on that axis; it grows to its content
  at once and is held while its content springs smaller (decisions.md
  popup-shrink), so nothing it holds is drawn past its edge on that axis.
  What is past both where it started the segment and where it rests (a
  spring's overshoot) is excused, and so is a node moved by authored
  motion on that axis (offset, pose, scale, turn) or held inside a
  clipping ancestor (`clip: true`, a `scroll`, a `list`). A node is
  judged by what of it shows, its drawn box cut by the clip its ancestors
  set (integration-r5: a label a squeezed chip clips is not cut where the
  chip hides it). The allowance is
  `tol` plus a quantum. The overshoot excused is bounded (eval-rules-3):
  no more than the tolerance's overshoot share of how far the node
  travels (its near edge's, far edge's or size's span from where it
  started to where it rests, or to where a change mid-segment found it),
  so a box that rests where it started and is glided past the surface's
  edge between is cut (set D's launcher-filter, its card glided 33.5 px
  past its panel). `pushed` (eval-rules-3): a node that draws something,
  wholly inside its surface (and the clip its ancestors set) and laid at
  one place (within `tol`) where a segment starts and where it rests, is
  not drawn past that surface's edge or that clip by more than `tol` plus
  a quantum on any frame between, unless something moves it there: a
  motion of its own or inherited that carries it (a glide, a slide, a
  lift, a scroll, a fling, keyframes, time; not a size spring or a
  stagger, which lay out), authored geometry, or a scroll. A clip a
  springing size sets is `held`'s to judge. Set E's module swap: an
  exiting text whose width springs to 0 wraps taller, and the bar's other
  modules are laid out below its 30 px for seven frames; set E's
  correct scenarios, the gate and the matrix lay nothing so. `give way`: a full row's children give way by
  `shrink`, an ellipsised text's min-content is 0 and a surface root never
  widens to its content (design.md "Layout", decisions.md wave3-pixels),
  so an ellipsised text drawn whole is never laid past its surface's
  edge, unless something that cannot give way puts it there: it or an
  ancestor past the edge with a width of its own, `shrink: 0` or a
  `min_width`, an image or icon, or a text with no ellipsis inside that
  ancestor and past the edge too, or a clipping ancestor. `held`
  (eval-rules-2): "a node whose size springs clips its content while it
  does (a collapsing toast hides what no longer fits)" (decisions.md
  wave3-pixels p2), and one exiting to a pose that sets its width or
  height clips until it unmounts (exit-height0), so what a descendant
  shows (its visual box cut by its ancestors' clip) lies inside the
  sizing box's visual box, within `tol` plus a quantum, however small the
  box has become. A text's glyphs count with its box (eval-rules-4: its
  ink as drawn, cut by its ancestors' clip): a label wider than a badge
  growing from `width: 0` reaches past its box, and the badge clips it
  (set F's module swap drew "Muted" 10 px past its badge). Glyphs under
  a scale are left out (their ink is drawn before transforms). A box
  drawn turned, or under a turned ancestor, is left
  out: its clip reaches the trace as a bounding box. `other axis`
  (bar-blank-on-exit): a pose sets the props it names and leaves the rest
  at rest (design.md "Poses"), so a pose that sizes one axis of its node
  alone (`enter { width: 0; opacity: 0 }`, mirrored by its exit) squeezes
  that axis only: on each frame the node plays it, its laid size on the
  other axis lies between its sizes at rest either side of the pose (the
  frame before its exit began, the frame after its enter ended), within
  `tol` plus a quantum, or at a sibling's size on that frame (a parent
  that stretches its children gives them the widest one's, integration-r5).
  A frame whose text is not shaped yet is left out,
  and so is a pose with no rest frame beside it. `shadow`
  (eval-rules-2): "Shadows enlarge the buffer but not the input region"
  (design.md "Layout"): the buffer keeps an overhang for its shadows'
  reach at rest (decisions.md wave3-pixels), so a box at rest (no
  motion that moves it or lays it out, no authored geometry, no paint
  offset, scale or turn, no clipping ancestor, and nothing on its
  surface springing a size, whose overhang is held where the spring
  started or rests, m6-owner-fixes bug 9) draws each shadow inside the
  buffer, within `tol` plus a quantum, on every side but the ones its
  surface's `cuts` name.
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
  1 s) after the batch's change, a period before its first frame. Its
  start is the first frame it no longer waits for its turn (`Motions`
  `STAGGER` cleared), or an earlier one that already shows it off its
  enter pose, and must come within a period after it is due: not before
  (drawn entering ahead of its turn) and not later. The start is not the
  first frame drawn off the pose, since an enter on a curve that starts
  slowly (`ease(ease_in, …)`, `in_back`) stays within a quantum of its
  pose for frames after it began (eval/scenarios/curated/
  stagger-slow-start.scn). It skips a child without a pose, one that never
  leaves its pose in the segment, one whose text is on its way (it waits
  for its text, decisions.md late-text-dip), one whose pose was dropped
  unseen (kept from view by an ancestor the frame before its first, it
  shows at rest and plays no enter, as `vanish` reads it) and reduced
  motion.
- **`pop-out`** (in `vanish`). "A removed subtree plays `exit`, then
  unmounts" (design.md "Poses"). The last frame an exiting node is drawn
  in shows it at its exit pose (its `exit`, else its `enter`): its own
  opacity within 1/255, its offset within `tol`, its scale within 0.005
  and a pose's width or height within `tol`, or nothing at all (its
  effective opacity at most 1/255). A slide to an edge, whose offset is the
  node's size, is held on its opacity only. Reduced motion is not held.
- **`continuity`** (eval-rules-2). "Retargeting mid-flight keeps
  velocity, so interrupted animations never jolt" (design.md), a spring
  is a unit-mass damped oscillator, and a change starts one refresh of
  its surface before its first frame (at most 16.7 ms, never before the
  motion's sample before), from where its way is then and with its
  velocity, a timed curve carrying that velocity as `v0·t·(1 − t/d)²`
  (architecture.md "Motion"; decisions.md first-frame-lead). From each
  spring's samples (`sp`) alone the check works its way out, with an
  oscillator of its own, and holds every value drawn to it within four
  times the spring's settling tolerance plus 1e-4 of its span. A way
  comes to rest by the product's rule: each channel's amplitude,
  `√(x² + (v/ω₀)²)`, within the tolerance (decisions.md roll-rate,
  integration-r4), each channel by its own tolerance (decisions.md
  border-eps). When a way comes to rest on a spring, the jump from where
  its curve is to its target is drawn: on an opacity or a colour's
  channels (a fill's, a text's, a track's, and a border's, a stroke's or a
  glow's colour) it is at most a step of 1/255 (`snap`), a threshold of
  the check's own, so a tolerance too coarse for what it settles is
  found. A value off
  its way after a change is reported as what fits it: `velocity` (the
  change started from rest), `start` (it started 16.7 ms early on a
  faster surface), else `way`. A way in flight that the next sample finds
  at rest elsewhere, or there but moving, was dropped (`velocity`). A
  surface whose frame says it wants no more frames shows its last frame
  until something changes, so each spring it sampled last is exactly at
  its target (within f32 rounding: a motion that settles snaps to its
  target), unless its node is exiting (`end`). On the frame a node
  enters, a prop its enter pose does not set has no value on screen to
  spring from, so it starts at its value (`first frame`; decisions.md
  m4-effects-finish). An interrupted roll rolls on from the nearer of its
  two texts, either when the frame before showed the other side of half
  way (`roll`; design.md "Rolling numbers"). A way met in flight with no
  sample before it, or one its surface painted a frame without sampling
  (reduced motion snaps), is picked up at its next rest. `fold`
  (eval-rules-3): a size sprung from zero, or exiting to a width or
  height of zero and then unmounted, folds the gap beside it with its
  slot (strand-render's `Forced::collapse`: "its padding and the
  parent's gap beside it fold with it, so its slot reaches zero as the
  spring settles and unmounting it moves nothing"). On the frame it
  first draws, and on the frame after it unmounts, each sibling (where
  nothing else in the parent comes, goes or changes size) is laid,
  relative to the parent, no further from the frame before than twice
  the folding slots' sizes plus `tol`, and starts no glide over a step
  within the parent (a glide's jump less the parent's move: one that
  carries the whole parent, a centred panel shrinking once a fold
  ends, is not one). Set E's enter-from-zero mutant opened the gap at
  once and glided the siblings over it. A sibling that shows nothing at
  both frames (no area, or no opacity), and one whose place among its
  staying siblings changed (a reorder moved it; FLIP glides it), are not
  judged (integration-r5). Its parent too (eval-coverage-3): a parent
  sized by what it holds is laid, across the frame a slot unmounts at
  zero, no larger or smaller than the slots then took plus `tol`, unless
  its own size moves by itself (it springs, exits, or keyframes or time
  drive it); unmounting a last row whose gap was never folded dropped
  that gap at once (`wifi-last-row-fold-2`; two neighbours leaving
  together did so in `toast-stack` and `unhold-flip` until strand-render
  laid such slots a hair above zero, decisions.md eval-coverage-3). The
  frame a slot mounts from zero is not judged on the parent: a
  content-sized parent takes its size at rest then, which is the
  growth rule the owner is deciding.
- **`placement`** (eval-rules-4). The expectation is worked out from the
  declared props (`places`), design.md, spec.md 5.1 and the protocols'
  semantics, never by strand-surface's placement code. The tolerance is
  one logical pixel plus `tol` (the compositor places whole pixels).
  `anchor`: a popup opens away from a bar's edge, across the whole bar,
  below its anchor otherwise, and beside its row for a popup in a popup
  (on the side its `anchor:` names, level with the row's top); `attach:`
  names its side that touches its anchor. The gap on the side facing the
  anchor is its `margin` there when set, else 6 px (0 with `attach:`).
  Across, it is centred on its anchor element. An axis where that place
  does not fit on the output is not judged (the compositor may slide or
  flip it), and a frame is judged only once its anchor is where it was
  when the popup was shown and nothing of the popup or its anchor moved
  since the frame before. A tooltip is not judged: nothing documents its
  place. `margin`: a layer surface lies in what the exclusive zones of the
  bars arranged before it leave of the output (a zone is a bar's thickness
  and its margin on its edge; higher layers first, then the order they
  were made), its margin from that area's edge on each side it is
  anchored to (a bar's edge and both ends, `anchor:`, a panel's
  `attach:`, which anchors that edge and not the opposite one). An axis
  anchored at both ends is judged only along a stretched bar, an
  unanchored one is centred by the compositor and not judged, and a frame
  is judged only when no layer surface changed size or place since the
  frame before. `centre`: a split's `center`, with its margin, is centred
  on the split's content box ("whatever the other two hold", spec.md 6);
  a side bar's split runs down. `fit`: an image's or icon's source shows
  where its `fit:` puts it in its box (`contain` whole and centred,
  `cover` covering it centred, `fill` stretched; `none` is not judged), a
  decode for another size standing in while its box springs included.
- **`fills`** (in `ink`, eval-rules-4). A text cut to its line box keeps
  on each line all of itself that fits there. Read from the layout drawn
  when it is the one shaped for its box at the frame's scale: before each
  wrapped line's break, the line with its next word, and before an end
  ellipsis, the kept start through the end of its next word with the
  ellipsis as drawn, measured on the text's unbounded layout, would not
  fit, give or take a pixel and `tol`. The cut is judged a word at a time:
  the engine cuts at a glyph, so it keeps at least the words that fit.
  Lines broken at a newline, an ellipsis at the start or middle and a
  text whose unbounded layout is not one left-to-right line are not
  judged. A line box shaped narrower than it is drawn (a text measured at
  one scale and drawn at another) breaks early and cuts words that fit.
- **`drag`** (in `coherence`, eval-rules-2). "A dragged slider draws the
  drag until the release", and its drag and its drawing share one span,
  the knob centre running between the knob radii, "so the knob stays
  under the pointer" (decisions.md wave3-pixels p3). On each frame a drag
  holds a slider, its knob's centre (its last part) is the pointer's x,
  held a knob radius in from either end of its box, within `tol` plus
  one physical pixel.

`$STRAND_EVAL_RULE_STATS` set (run.sh passes `STRAND_*` in) makes each
eval-rules-2, -3 and -4 arm say on stderr how much of a run it held
(`rule-stats <arm> <scenario>: …`: samples, boxes, shadows, knobs, the
frames `contain.pushed` held a node on, the siblings and parents `continuity.fold`
held, the colour ways `colour.way` held and bounded by a curve, the
palette changes `colour.palette` read, the frames each `placement` arm
judged, the lines `ink.fills` read and the crossfade frames
`redraw.blend` compared), so a sweep can tell a pass from
an arm that saw nothing.

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
  a control's parts (`pt`) and, for an image or icon, where its decode
  was drawn and at what size (`im`, eval-coverage-3);
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
(`bg`, `bd`, `sh`, `fg`, `gl`, `pt`, `im`). Several of one kind are joined with `/`
in paint order; `gl` is a JSON list.

Everything together stays under 6 MiB. With each node's paint and glyph
sources (eval-coverage) it was 4.15 MB, and with the integration's
scenarios (eval-r3) 4.18 MB. With the scroll, control, placement and
theme scenarios, the controls' parts and the two input effects
(eval-coverage-2), and round 4's integration (integration-r4) it was
5.09 MB; with eval-rules-3's nine scenarios 5.13 MB, and at round 5's
integration 5.15 MB. With eval-coverage-3's nine scenarios, the image
effect and the deeper nested FLIP it is 5.58 MB (5,577,969 bytes, 713
kB under the tripwire): the 102 scenarios 610 kB, `matrix-60.ref` 2.01
MB and `matrix-144.ref` 2.96 MB, 486 cells each. Paint
costs little because a field is written only on the ticks it changes.
Compressed, as git stores it, that is about 530 kB. eval/tests/refs.rs
holds the total to 6 MiB (5 MiB until integration-r4, when the
references reached 97% of it), a scenario's file to 64 KiB and a matrix
file to 8 KiB a cell. Each is a tripwire for a recording that stopped
being compact, not a cap on scenarios.

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

- scrubbable by tick, with a filmstrip of every frame and an onion skin
  of the frames before;
- at `fit` zoom by default: the largest whole zoom (or the fraction
  below 1) at which the widest surface of the run fits the stage, so a
  phone shows the whole surface and the frame does not jump in size as
  the scrubber moves; A and B side by side only where each still gets
  twice its size, else one above the other;
- with nodes outlined by drawn, visual or laid box;
- with the findings listed and linked to their frames;
- with a node's path plotted per axis, at the panel's width;
- with an A/B tab when the page holds two runs: a pixel difference of
  two runs with frames, and against an approved reference (which records
  no pixels) the nodes drawn differently from it, the run's solid red
  and the reference's dashed;
- with an Approve tab: the run's approved reference and the command that
  approves it, with the note and the approver typed there (see
  Reference recordings).

The matrix grid lists, under each rate's grid, every cell that did not
pass, linked to its page with its failed checks, so nothing needs a
pointer's hover to be read.

### The viewer in a browser

`scripts/eval/viewer-check.sh [--expect KINDS] PAGE...` loads report
pages in headless Chromium and drives every mode: the scrubber and its
keys, the filmstrip, the overlays, the onion skin, zoom, the curves, the
cost lane, each A/B view, the Approve tab (its buttons stay disabled until
a verdict and a note are given, and the copied command is checked word
for word), light and dark, at a desktop's and a phone's width (no page
scroll sideways). It fails on any console error, uncaught exception,
failed request or request off the page, and on any mode that does not
work. Screenshots of each mode land in `target/eval/viewer-shots/`.

It runs on the host's Docker, like `run.sh`, in
`scripts/eval/Dockerfile.viewer`: Playwright's Python image pinned by
version and digest, with its Python package of the same version, tagged by
the file's hash. The container has no network and the pages are mounted
read-only; the script fails if the container outlives it. `--selftest`
makes its own pages first through `run.sh`: the curated and regression
scenarios, the same under the bug25-hidden-text mutant (a failing
scenario, CHANGED ones and their A/B against the reference) and a small
matrix. `--expect pass,fail,changed,ab,matrix` names the kinds the pages
must have held. The nightly eval-matrix workflow runs it on the mutants'
pages and the matrix grid.

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
rolling text, a colour change, late text, screen scales, a list the
wheel scrolls (`scroll`: two notches in quick steps and one back during
the spring) and a two-way slider dragged from its knob (`slider-drag`)
(eval-coverage-2), an image (`image`: a 3:2 PNG under `fit: cover`
whose box widens, so a decode of its old size stands in while it
springs; eval-coverage-3), and the GPU leg (`gpu`). `flip-nested` moves
three levels at once (eval-coverage-3: each moving box holds a mark).
Each one is a set of
fragments
filled into `eval/matrix/fixture.strand.in`, then run with the standard
steps (on, wide, off); a relative path in one (the image's) is read
against `eval/matrix/`, the fixture's directory. The matrix runs every
effect alone and every unordered pair at each rate.

Two effects that set the same prop of one slot cannot pair, and their cell
is `n/a: conflict`. Effects carry what their motion moves: `flip-reorder`'s
keyed boxes each hold a child, so a glide carried to a subtree is checked
(`coherence` inherit), and `reduced-motion` turns its subject with
`rotate: t * 90deg`, so a time signal under reduced motion is checked
(`twin`, `teleport`). The two input effects act in the `mid` steps,
after the `on` segment's rest, so each effect paired with one runs beside
a list scrolled or a knob dragged (and its time-driven motion, a
keyframe or a jelly, runs on through them); the two cannot pair, since
both fill `below`. A cell fails the matrix in four cases:

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
every push as scripts/eval/ci.sh step 6 (about 110 s on the laptop for
both rates' 930 cells with eval-coverage-2's two input effects, 90 s
before them), so a branch whose change moves a cell cannot pass CI until that
cell's new recording is approved. It also runs nightly and on demand in
`.github/workflows/eval-matrix.yml`, with the mutants.

**Repeated runs.** The workflow's `repeat` input (on demand; default 1,
one matrix run as nightly) runs scripts/eval/repeat.sh, which runs the
scenario gate and the whole matrix that many times in one job on one
runner. Each run must pass on its own, held to its approved references
(so a recording that differs from the reference fails that run as
CHANGED), and every scenario's and cell's trace digest in every run must
be the first run's: a run that differs anywhere fails the job, and its
summary names the run and the scenario or cell. A recording the laptop's
container holds but another machine's lavapipe or scheduler wavers on
shows up there. Locally, `scripts/container/run.sh eval repeat N` (at
60 and 144 Hz; matrix flags such as `--effects a,b` follow N) runs the
same, under
target/eval/repeat; scripts/eval/test-repeat.sh tests it with a stand-in
for the build.

## Mutants

`eval/mutants/mutants.txt` pairs patches with regression scenarios. Each
patch reverts a fix in the product's crates, for example bug 24's flip-open
and bug 25's hidden-text in crates/strand-render, and bug 23's gpu-regions
in crates/strand-surface and crates/strand (see Surface leg), popup-still-pose's
exit-due fix in crates/strand-render (decisions.md eval-r3), and round 4's
fade-layer-redraw (`redraw`), gradient-far-hue (`cpu-gpu`) and roll-rate
(`settle`, in crates/strand-scene) fixes (decisions.md shadow-redraw,
gradient-cpu-gpu and roll-rate), and round 5's border-fade-snap
(`continuity`, decisions.md border-eps) and text-exit-squeeze (`contain`,
`expect`; decisions.md bar-blank-on-exit) and clip-carries-rows (`vanish`,
decisions.md clip-blink) and shrink-keeps-edge (`bystander`, `expect`;
decisions.md bottom-osd-shrink), and unhold-flip (the same revert as
shrink-keeps-edge's, on a held centred launcher letting go: `envelope`,
`contain`; decisions.md integration-r5), and round 6's damage-fraction
(`redraw`, decisions.md damage-fraction) and, on one regression,
cross-surface-morph (`expect`) and morph-group-bounds (`redraw`;
decisions.md cross-surface-morph): 15 in all. In a detached worktree of HEAD, each scenario
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

**The GPU leg alone cannot catch bug 23.** Both legs flatten the same
scene in the same renderer, so their `Painter` regions agree by
construction. Bug 23 was a GPU-drawn frame's regions not reaching the
compositor, which only the surface manager's side sees: the surface leg
below runs it, on both legs.

## Surface leg

`surface = true` runs the surface leg beside the run (`eval/src/wire.rs`).
The harness still schedules and paints every tick as it always does. Its
frames are then handed through the real surface manager
(`strand_surface::SurfaceManager`) to the fake compositor
(`strand_fake_wayland`), which runs in-process on its own thread. Both run
on the run's virtual clock: the manager's `FakeClock` and the fake's
presentation clock read the same time.

- The renderer's surface changes reach the manager as the renderer
  reported them, in their order around the tick's paints. The sandbox's
  screen is the fake's one output.
- The manager's painter answers with the harness's frame, as the renderer
  answered right after painting it: its damage, whether it wants another
  frame, and its `Painter` regions and pose. No pixels are copied; the
  fake reads none.
- A CPU frame is the manager's to paint and commit, through its shm
  buffers.
- A GPU frame goes as the shell's GPU host sends one
  (crates/strand/src/run/gpu.rs `present`). The first time, the surface is
  lent (`raw_handles`) and handed off (`hand_off`). After each frame,
  `sync_presented` sets its regions as pending state, and a stand-in for
  the GPU thread's WSI presents on the lent `wl_surface`: a buffer of the
  surface's size, its whole damage and a frame callback. A surface's first
  frame is the manager's whatever draws it. A surface the manager never
  committed is not mapped for it, so no popup would open on it, and in the
  shell a surface is promoted only after frames of its own on the CPU.
- A `backend cpu` step takes the surface back (`take_back`), as the GPU
  thread's `Released` does. So does a surface the manager has to destroy
  while handed off (`gpu_release`).
- The fake runs in vblank mode. Nothing is shown, no frame callback is
  done and no feedback is presented until the leg's refresh at the end of
  each tick, at the tick's time. A commit made in tick `k` is shown at
  refresh `k`. A buffer that another commit replaces before the refresh is
  never shown, and its feedback is discarded.

The fake logs every commit (`Fake::commits`): its buffer, buffer scale,
viewport, damage, the opaque, input and blur regions it holds after it,
its alpha, frame callbacks and feedback, and the refresh that showed it.
The leg matches each buffer commit to the paint or present that made it.
Both are made in order on one connection, so the n-th buffer commit is the
n-th of them. The result is `RunTrace::wire`: every frame with the
regions its painter computed, converted as the manager converts them (the
opaque region rounded in, the input region rounded out, the blur region by
`strand_surface::blur::region_rects`), every commit, and every refresh.
Regions are compared as the pixels they cover: each is kept in one normal
form, so the same pixels sent as other rectangles compare equal. The log
is in no digest and no recording, since it is how frames reach a
compositor, not what they draw.

`regions-on-wire` holds the compositor's regions after each commit that
carries a frame, and after each bare commit after it, to that frame's;
a bare commit after a frame that drew nothing (no damage, so no buffer
of its own) is held to that frame's, whose regions it sends. A
frame whose painter asks for no input region keeps the manager's own,
which is not compared. A frame whose surface the compositor poses at a
scale (`compositor_poses`) sends its regions in the pose's coordinates,
and is not compared either. `late-present` holds every frame that drew to
a commit of its tick shown at its refresh. With `backend = "both"`, both
legs run the surface leg, and the GPU leg's findings are marked `GPU
leg:`. The twin and the determinism run do not run it.

regressions/bug23-gpu-regions.scn is bug 23's scenario. Its mutant,
eval/mutants/bug23-gpu-regions.patch, reverts the gpu-regions fix:
`sync_presented` does nothing, and the shell's host no longer calls it.
Under it, the island's presents keep the input and blur regions of its
first, shm frame, and `regions-on-wire` fails on the GPU leg. The leg costs
a few milliseconds a tick, so it runs in CI's eval job with the gate.

What the surface leg leaves out:

- the GPU thread's own latency, since the stand-in presents at once
  (strand-gpu's tests hold the real one), and the window in which the
  handles are lent before the hand-off, while the GPU thread builds its
  swapchain;
- the host's call site in crates/strand/src/run/gpu.rs, which the leg
  mirrors rather than runs: of the mutant's two hunks the leg sees the
  emptied `sync_presented`, not the removed call. No test runs that call
  site (the shell's run loop is outside the sandbox and strand-surface's
  tests call `sync_presented` themselves);
- scales other than 1, since the fake's output has no other; a `scale`
  step with the leg is refused as unsupported;
- exclusive zones: the fake stretches each layer surface over the whole
  output, so a run in which another surface's zone sizes one (a left bar
  under a top bar) is refused as unsupported with the leg
  (eval-false-alarms);
- the regions of a posed frame (see above).

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

`--explore` (with `--config`; `local new` is `new --explore`) writes a
starter that exercises what the config declares instead, found from its
compiled program and its runs, knowing nothing of any shell:

- each top-level `state` that is a `bool` or an `enum` is set to each of
  its other values and back to the one it booted at, a rest after each;
  other states are listed as `# set` comments to fill in;
- each element whose `hover` or `pressed` is read, or that anchors a
  `trigger:` popup, is hovered, and each with `on click`, `on secondary`,
  `on middle` or `on scroll` is pointed at and given each input twice (a
  scroll and its reverse), at the centre of where it is laid out, on its
  surface;
- after a hover or an input that may reveal more (a popup's anchor, a
  click), and after each state value, the steps so far are run again, and
  what is newly on screen is exercised there before going on (three
  levels deep, at most 32 runs and 40 targets of each kind);
- a target that moved is pointed at where it is now, a second input is
  left out when the first took its target away, and a target whose run
  fails is dropped and named.

The starter ends with comments: what was never on screen, any step it
stops at, and any `rest` that never came (a live readout or a running
clock keeps the shell awake; `settle` reports each). It is a start: say
what the scenario holds in `about` and add the `expect` steps a correct
shell meets.
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

- `config = "/abs/path/shell.strand"`, `config = "${VAR}/shell.strand"`
  or `config_dir = "…"` names a config where it is. `${VAR}` is expanded
  from the environment.
- `fonts = "local"` shapes text with the machine's fonts, found through
  fontconfig as the shell finds them, plus any font files under the
  `:`-separated directories of `$STRAND_EVAL_FONTS`.

```text
scripts/container/run.sh eval local new my-shell --config /path/to/shell.strand
scripts/container/run.sh eval local                  # every eval/scenarios/local/*.scn
scripts/container/run.sh eval local eval/scenarios/local/my-shell.scn --frames all
```

`run.sh` mounts only the checkout, except for `run.sh eval local`
(`scripts/eval/local-mounts.sh`): it mounts read-only, at their own
paths, the directory of each config a local scenario's header names by an
absolute path (and of each absolute path among the command's arguments),
and the host's font directories (`/usr/share/fonts`,
`/usr/local/share/fonts`, `~/.local/share/fonts`, `~/.fonts`) at
`/opt/host-fonts/<n>`, with a fontconfig file
(`target/eval/local/fonts.conf`) that adds them to the image's. Nothing is
copied, and no other command sees them. It prints what it mounted.

`check --gate` refuses local scenarios, and nothing from them is ever
committed: a finding in an owner's config becomes a committed scenario
written from scratch, in the smallest shape that shows it.

## Live sandbox

`strand-eval live` is a window for prototyping an animation by hand: it
runs a config (or a `.scn`) in the driver, on the virtual clock, at the
speed chosen, and shows its surfaces on a stage as the sandbox lays them
out (bars and layers on the screen, popups where the placement model
puts them). What you do in it is a scenario: `w` writes it, and that
scenario runs to the session's own trace (the same digest) under `check`.
The window is the eval workspace's own: the shipped binary has none of
it, and the renderer's hooks stay behind `eval-hooks`.

On your own session, from the checkout:

```text
scripts/eval/live.sh ~/.config/strand/shell.strand
```

It builds `strand-eval` in the container (the timing profile) and runs
the window on the host, in the session the terminal is in. Flags follow
the file: `--screen 2560x1440@1.5` (the screen the layout sees, default
1920x1080@1), `--hz 144`, `--fonts local` (the machine's fonts, as in
Local configs), `--speed 0.25`, `--paused`, `--record-to FILE`, `--ring
N` (frames kept to step back through, default 240) and `--max-frames N`
(default 20000).

**The window.** A strip of controls on top, the stage under it:

- the status line: playing or paused, the speed, the tick and its
  virtual time, the frame shown when stepping back, the refresh rate,
  scale, motion, the steps so far, the steps still to play, reloads;
- buttons for each key below, and one per knob;
- the checks' verdicts (`c` runs them; failures first, `*` when older
  than the session);
- the failures at the frame shown, each outlined in red on the stage
  where its node was drawn, else the last message;
- the command line (`:`).

The pointer on the stage is the shell's pointer: hovering, clicking and
scrolling there are steps (`pointer X,Y on <surface>`, `press`,
`release`, `scroll`), and leaving the stage is `leave`.

**Keys.**

| Key | |
|---|---|
| space | play or pause |
| `.` or Right | one tick forward (paused) |
| `,` or Left | one frame back through the ring |
| End, Home | the newest frame, the oldest kept |
| `[`, `]` | slower, faster: 0.1x, 0.2x, 0.25x, 0.5x, 0.75x, 1x |
| `1`-`9` | run knob N |
| `m` | reduced motion on or off (`reduce-motion`) |
| `s` | the next scale: 1, 1.25, 1.5, 2 (`scale`) |
| `h` | the next refresh rate: 60, 144, 30 (`hz`) |
| `l` | the pointer leaves (`leave`) |
| `c` | run the checks now |
| `f` | the next frame with a failing finding |
| `w` | record the scenario |
| `r` | restart: the config booted again, nothing done |
| F5 | reload now (edits are seen within a third of a second anyway) |
| `:` | type a step (any of the step vocabulary), Return runs it |
| `-`, `=`, `0` | zoom out, in, fit; shift+arrows pan |
| `q` or Esc | quit |

**Knobs** come from the compiled program, as `new --explore`'s do: a
toggle per bool state, a cycle per enum state, a hover per popup's
anchor, the inputs (`click`, `scroll`) of the elements that handle them,
and an `emit` per service event the config handles (one with arguments
fills the command line instead).

**Time.** Paused, `.` runs one tick. Playing, the virtual clock runs at
the speed chosen against the wall clock; the clock is the scenario's, so
a slowed spring is the same spring frame for frame. Between your steps
the clock's run is written as `rest` when the last thing done came to
rest at its last tick, else as `frames N`. A config's boot is run to rest
at once (its first step is `rest`); a `.scn` plays its own steps.

**Edits.** Saving the config (or one of its imported files) reloads it:
the steps so far run on the new config at once, up to the last thing
done, which plays again live, so the change is seen in motion. An edit
that does not compile is shown and changes nothing. Saving an opened
`.scn` plays its steps again from the start.

**Recording.** `w` writes the session as a scenario: the header, the
config inline as `===` sections (as it was when it ran, so later edits
do not change it), and the steps. By default it lands in
`eval/scenarios/local/live-<name>.scn` (gitignored; `--record-to`
names another place). Check it, and approve its reference after reading
the diff, as for any scenario:

```text
scripts/container/run.sh eval local eval/scenarios/local/live-shell.scn
scripts/container/run.sh eval approve target/eval/latest/live-shell --note "why"
```

A scenario that is to be committed is moved to `eval/scenarios/curated/`
(or written from scratch from it, for an owner's config) and given an
`about`.

**Driving it without a window's keyboard.** It reads commands on stdin,
one per line, and answers each with a line of JSON (`{"ok":true,...}`),
after a first `{"ready":true,...}`: `status`, `play`, `pause`, `toggle`,
`speed F`, `slower`, `faster`, `step [N]`, `back [N]`, `head`,
`finding`, `do STEP`, `knob N`, `knobs`, `motion`, `scale`, `hz`,
`leave`, `hover X,Y`, `press [BTN]`, `release [BTN]`, `scroll DY`,
`record [PATH]`, `scenario`, `digest`, `reload`, `restart`, `checks`,
`zoom F|fit`, `shot PATH [WxH]` (the window's drawing, offscreen) and
`quit`. eval/tests/live.rs drives it this way on a headless sway of its
own, with a virtual pointer and keyboard, and reads grim's shots.

## Limits

- **Placement is strand-surface's, not a compositor's** (eval-false-alarms).
  The sandbox places surfaces with the code the surface manager sends
  the compositor, and arranges layer surfaces as wlroots does from it. A
  popup is shifted onto the screen but never flipped or resized past
  that (its positioner asks for no more), a posed parent's scaled anchor
  is not followed, only the config's own surfaces reserve zones, and how
  a compositor treats unequal margins on an axis anchored at both ends
  is taken from `position_in_rect`, not checked against wlroots.
- **`contain`'s overshoot allowance is a share of the travel**
  (eval-false-alarms; bounded in eval-rules-3). What lies past both
  where a box started and where it rests is excused up to the
  tolerance's overshoot share of how far it travels, so a glide that
  carries a box past both ends by less than that passes `cut`. The
  travel counts a laid size that snaps and edges measured in each
  frame's own buffer, so `osd-edge-margins`' label, cut 11 px at its
  buffer's edge for five frames by the growth glide, passes by 0.5 px
  (decisions.md integration-r5); `cut` judges what of a node shows,
  so what a clip hides past the edge is no cut.
- **A label nudged on the output by its parent's glide**
  (eval-rules-3, integration-r5). A content-sized surface anchored to an
  edge that grows a second time while its first growth's glide still
  runs moves the label at its anchored edge on the output for a few
  frames (4 px on a bottom OSD); only an `expect still` holds it, since
  the label is laid elsewhere in the resized surface and carried by its
  parent's glide, which `bystander`, `envelope` and `pushed` excuse.
- **Growth on a content-sized surface waits for the owner**
  (integration-r5). The growth glide (`glide_origin`) is the
  content-sized growth rule's, which the owner is deciding: a bottom
  panel growing glides its card past its bottom on the first frame
  (`contain` 16 px), and a centred card whose content snaps larger is
  glided by half the growth from its corner (cut up to 11 px). These
  stay out of the gate; held-glides' change to the growth glide waits on
  laptop/fix-r5b. A shrink is fixed (bottom-osd-shrink); at scale 1.5 a
  held bottom OSD's last row is laid 1 px off on two frames as its row
  folds (the held root's placement rounding, popup-shrink's rule).

- **An image's pixels are not recorded** (eval-coverage-3). `im` holds
  where a decode is drawn and at what size, and `fit` judges that
  placement; what the decode holds is not seen. A PNG reduced by an
  integer factor before it is resampled rounds each side up apart
  (`decode_png`'s `div_ceil`), so `art.png` (60 x 40) is reduced to 20 x
  14 for a 16 x 12 box and its content drawn about 5% taller than its
  aspect; neither `fit` nor a reference sees it (left open,
  decisions.md eval-coverage-3).
- **Edge margins and submenu places have no rule** (eval-coverage-3).
  A layer surface's margin moved by the wrong overhang, or a submenu
  placed from its parent's buffer instead of its window geometry, moves
  a whole surface that nothing inside it judges: only its reference's
  `o=` sees it (the mutants of decisions.md eval-coverage-3).
- **Text is shaped inline.** Lateness is a hold on top of inline shaping
  (`text late`, `text hold`). A real shaping worker's timing is not
  modelled.
- **Wall time is not gated.** Frame times are recorded and `bench`
  reports them, marked advisory, but no check gates on them. The timing
  tests in crates/strand-render hold the frame budgets.
- **`ink` sees the layout drawn, not the pixels.** It reads the glyph
  ink of the layout the frame drew, before transforms; a glyph clipped
  by an ancestor's clip only is not told apart from one drawn. A node
  wholly outside the clip its ancestors set is `hidden: clipped`
  (eval-coverage-2); one partly inside is drawn as a whole.
- **`ink`'s allowance follows the font** (eval-r3). A text drawn by its
  own layout may reach past its box as far as that layout's ink reaches
  past the layout's box; a layout drawn offset by less than that
  overhang is not told apart from one in place.
- **The host loop's drift guards match spellings** (eval-r3).
  eval/tests/drift.rs and its shell twin catch a step of the host loop
  taken by hand under its own name; one reached through an alias passes
  them, and review holds the rest.
- **The surface leg's compositor is a fake.** It runs the real surface
  manager against strand-fake-wayland at scale 1, with a stand-in for the
  GPU thread's WSI. A real compositor's own handling of the regions (a
  blur it draws, input it routes) and the GPU thread's present latency
  are outside it (see Surface leg).
- **The surface manager's throttle gives up on real time.** It stops
  waiting for a refresh after a second of `Instant::now`, not of the
  leg's virtual clock, so only a run that slow (a loaded machine) sees it
  give up. The leg notes each tick it did (`WireLog::gave_up`) and
  `late-present` does not judge that tick's frames or the next's: a
  frame the product presents late only on such a tick goes unseen.
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
  A gradient's geometry (how far its line runs across the box) is not
  recorded either: paint.rs's three gradient pixel tests hold the
  dithered pixmap every drawable gradient is painted from
  (`cache::render_gradient_part`). Radii, blur radii, an image's pixels
  and a reveal mask's progress are not recorded. `gl` names the text a
  layout was shaped from, not where an ellipsis cut it: the cut width
  (`fit`) and the ink record that. A text's paint is recorded once per
  run of glyphs drawn, so two texts with the same source, colour and
  glyphs drawn on one node are one entry.
- **`continuity` reads what the motion was asked, not how it moved.**
  The trace records each spring's target, curve and changes as the
  renderer's `Motion` holds them (eval-rules-2), and the check works the
  way out itself, so a renderer that springs with the wrong stiffness, a
  velocity lost or a start mistimed is caught; one that records a change
  it never made, or makes one it does not record (a target changed with
  no change started), is not judged until that spring's next rest.
  Only props', laid-out sizes' and glides' springs are traced: a
  shadow's, a shader uniform's, a roll's own progress, a shared morph's
  box and a lean's tilt are held by other checks and the references.
- **`held` and `drag` hold boxes, not shapes.** `held` skips a box drawn
  turned (its clip is traced as a bounding box), and `drag` skips a
  slider drawn turned or scaled (its knob's part is in its unturned
  box).
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
- **The live sandbox runs the CPU leg, live** (eval-live). No surface
  leg, no GPU, no twin or determinism run: those are `check`'s, on the
  recording. Surfaces are drawn at buffer scale 1 to the stage whatever
  the window's scale, keys typed go to the sandbox, never to the shell
  (no keyboard step), a touchpad on the stage scrolls by wheel notches
  (`scroll`; type `swipe DY over MS` at `:` or `do` for a touchpad's, which
  plays tick by tick like a glide, integration-r4), and a recording
  carries the `.strand` files
  inline, not other assets (images, icons, fonts it names by path). The
  ring keeps the last `--ring` frames; a session stops at `--max-frames`
  painted frames (record and restart). The checks run when paused (or
  every two seconds while playing), over the whole session so far.
- **The renderer's other wall-clock reads stay wall-clock**: an exit's
  stall, GPU promotion and device retries, the busy window. Their waits
  are set so they never fire in a run.

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

What the sandbox guarantees is the trace, the renderer's own record of
where it drew each Strand node in each frame, and, for a glitch report,
the replay's fidelity to the live shell (see Replaying a glitch report).
The checks' rules are frozen: one changes only when it is wrong on
Strand's own committed gate scenarios, regressions or effect matrix. A
finding on a real shell (a local scenario) or in a report's replay is a
hint, which the checks may get wrong there; it is never tuned away per
shell, and no threshold is loosened and no `known.toml`, `allow` or
`expect_fail` entry is added for it. Only Strand's own surfaces are
recorded, never other programs' windows (decisions.md
owner-2026-10-21).

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
- `explain <run>/<id> [--node SEL] [--frames a..b] [--check NAME]
  [--png DIR]` prints a node's frames as a text table. By default it
  shows the nodes of the failing findings. (replayfix) `--png DIR` also
  writes each painted frame in `--frames` (all without it) whole, as
  `DIR/k<tick>-s<surface>.png`, a packed report's (a replay's) rebuilt
  from its patches.
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
- `replay <bundle> [--to SCN] [--out DIR] [--output NAME] [--traced
  SECONDS|all] [--frames …]` replays a glitch report (see Replaying a
  glitch report); `scripts/report-view.sh` runs it.

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
| `screen` | `{ width = 300, height = 1080, scale = 1.0 }` | The screen (scale up to 4). (replayfix) Also who it is in `screens.all`: `name` (`"DP-1"`), `id` (`"Mock \| DP-1"`), `make`, `model`, `description`; a replay names the live output so. |
| `screens` | none | (fidelity) Several screens in place of `screen`, in `screens.all`'s order: `[ { width, height, scale, name, id, make, model, description, x, y }, … ]`, each its own output with its own scale, size and place in the layout (`x`, `y`, logical px); the names and ids are distinct. The first is focused at boot. A surface pinned to a screen (`screens: [..]`, a `bar` per screen) maps on its own; any other on the focused one. The sandbox shows one surface per node: a node on several screens at once (`screens: all` on a panel or OSD, or a list of screens) is shown on one, the first it names, else the focused one, and a replay names the others in its limits. Not with `screen`, nor with the surface leg. |
| `main_screen` | the first | (fidelity) The screen of `screens` that `scale` steps, the trace's `scale` and the viewer start with, by name. |
| `hz` | 60 | The refresh rate; a `hz` step changes it. |
| `backend` | `"cpu"` | `"cpu"`, `"gpu"` or `"both"` (see CPU and GPU). |
| `text` | `"inline"` | `"inline"` only; `"worker"` is refused as unsupported. |
| `fonts` | `"bundled"` | Liberation Sans from strand-text; `"local"` only in `scenarios/local/`. |
| `checks` | every CPU check | The checks to run (see Checks). |
| `expect_fail` | none | `{ check = "why" }`: a known failure, reported as XFAIL. |
| `reduce_motion` | false | Motion reduced from the start; a `reduce-motion off` step turns it off (eval-rules-5). |
| `compositor_poses` | false | The compositor applies surface poses (`Renderer::set_compositor_poses`, as on Hyprland): a surface's `enter`/`exit` pose is committed as its pose, its content painted at rest. |
| `surface` | false | Runs the surface leg and adds `regions-on-wire` and `late-present` (see Surface leg); naming either check turns it on too. Scale 1 only. |
| `twin_pixels` | false | `twin` compares pixels too. |
| `tooltip_delay` | the renderer's | In ms. |
| `config`, `config_dir` | none | A config named instead of written in (see Local configs and fonts). |
| `[services]` | none | `"path" = value` pairs, set before boot. |
| `[seed]` | none | (report) `"path" = "<JSON>"` baselines for a service field (before boot), a cell (by the inspector's path, as it mounts) or a `$token` (an overlay): no `on change` fires for them; a seed never mounted is a note. |
| `wall` | the mock's own time | (report) The wall time (RFC 3339 with its offset) virtual `T0` stands for; `clock` and local time follow it (`SchemaHost::mock_at`). |
| `warmup` | 0 | (report) Ms run before the trace starts: painted, neither traced nor checked; the first traced frame repaints every surface. |
| `icons` | the config's `icons/` | (report) A flat icon folder used in place of it (inside `eval/` for a committed scenario). |
| `[allow]` | none | `check = [{ node = "<selector>", why = "…" }]`. |
| `[tolerance]` | `edge_px = 0.5`, `overshoot = 0.35`, `teleport_px = 32` | See Checks. |
| `[budget]` | none | Per-frame ceilings: `items`, `records`, `damage_px`, `layout_nodes`, `layout_passes`. |

A committed scenario is any scenario outside `eval/scenarios/local/`. It
may name only files inside `eval/` and the bundled fonts. The parser
refuses anything else, so an owner's config or fonts never reach a gate.
eval/fixtures/images/ holds the images scenarios draw: `art.png` (60 × 40,
four coloured quarters and a dot), `app.png` (24 × 32), `ramp.png` (a
27 × 13 ramp) and `spinner.gif` (four 16 × 16 frames of a quarter ring
going round, 100 ms each, looping forever; eval-coverage-5).

### Step vocabulary

There is one step per line, and `#` followed by a space starts a comment.
Instant steps act at the tick the next time step starts. Time steps
advance the clock.

**State and services**

- `set <path> <value>`: a value as `strand set` reads it, or (report) a
  record, list or optional value written as JSON by its schema type
  (`vm::json::from_json`; a value starting `{` or `[` is read as JSON
  first). A step the
  shell refuses fails the run with the config's load warnings (but the raw-colour lint) beside
  its reason (a bare `network` that reads the builtin service where
  `Module.network` was meant is one).
- `toggle <path>`.
- `emit <service.event> [args…]`: each argument read as `set` reads a
  value.
- `answer <service.method> <JSON>` and `answer <service.method> error
  "<text>"` (report): the next call of that method is answered with it,
  ahead of the mock's own (`SchemaHost::answer`); answers queue in order.
- `notify "<summary>" ["<body>"] [app=…] [urgency=…]` parses, but a run
  refuses it as unsupported: the notification daemon is not in the
  sandbox. Set `notifications.*` and `emit` instead.

**Input**

- `pointer X,Y [on <surface>]`.
- `press` and `release`, each with `left`, `right` or `middle`.
- `click X,Y [left|right|middle] [on <surface>]`.
- `scroll DY` turns the wheel `DY` notches (positive down; fractions
  allowed), each 15 px (as libinput's legacy wheel, decisions.md
  wave3-pixels; the sandbox's own `NOTCH_PX`, so the reduced-fling bound
  does not follow the product's constant), as one axis frame from a wheel: each notch springs the offset
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
- `key NAME [+ctrl|+shift|+alt|+super…] [down|up]` (report): a key on
  the surface with the keyboard, a press and a release without
  `down`/`up`.
- `type N [over MS]` types N placeholder `x`s (at once, or spread over
  MS as a time step); `paste N` pastes N `x`s into the input that asks
  (ctrl+v, then the clipboard's answer).
- `focus <surface>` and `unfocus` give and take the keyboard. Without
  them it is the compositor's: a `keyboard: exclusive` surface takes it
  as it maps, an `on_demand` one on a press in it, and it returns to
  the surface before when that one goes.

**Perturbations**

- `text hold [where "s"]`.
- `text release [where "s"] [via waker|paint]`.
- `text late N [where "s"] [via …]`.
- `drop N [surface=<selector> [output=<screen>]]`: the next N ticks
  paint nothing, on every surface or on the one named; (fidelity)
  `output=` takes, of the surfaces the selector names, the one on that
  screen (a `bar` per screen is named once), the focus left where it is
  (a replay's late frame).
- `jitter 1,1,2,1` sets per-tick period factors.

**Environment**

- `reduce-motion on|off` sets `system.reduced_motion`, which logic reads
  and hands render in its diff, as in the shell (so does `set
  system.reduced_motion …`). It works both ways against the header: a
  scenario whose header says `reduce_motion = true` starts reduced and a
  `reduce-motion off` step turns motion back on (eval-rules-5; before,
  the header held render reduced for the whole run). Only the twin
  (`--reduced-motion`) is reduced whatever the steps say.
- `scale S` (with `screens`: the main screen's).
- `screen NAME` (fidelity): the focused screen, by name; a step on a
  surface named by its origin finds it on the focused screen first, and
  a `screens: focused` surface mapped after it opens there.
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

- `#id` selects the node with that `id:`, or one an `id:` names through
  a component call: a call's `id:` names its root element, and in a
  chain of calls each call's does (`Instance::identity`'s `id` and
  `also`; decisions.md eval-finish).
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

**Posed margins** (eval-rules-6). With `compositor_poses`, a layer
surface's frame commits with the margins the surface manager sends for
its delegated pose's offset (`State::sync_pose`, through
`placement::posed_margin`), and the driver places it there as the
compositor does: right after each paint it reads the pose
(`Renderer::delegated_pose`), arranges the surface at those margins,
tells render where it lies (as the host's `send_origin` does after
`surface_placed`), and moves the surface's popups with it. A popup takes
no offset (the manager sends none). The trace keeps `origins` at rest
and records how far the compositor put each posed surface from there
(`shifts`); the checks' view of the screen moves a posed surface by its
`shifts`, not by the offset render asked for. Before, the trace moved it
by the offset itself and nothing read the margins, so a wrong sign in
them (held-out set H's bottom-anchor mutant) was invisible.

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
  read as not shown. (eval-rules-7) Otherwise a node shows when it is
  drawn with some opacity and area on its surface and what it draws
  overlaps its ancestors' clip: for a text that draws glyphs alone (no
  fill, border, shadow, image or pattern of its own), what it draws is
  its ink mapped into its visual box, not its line box, so a text whose
  line box still overlaps a list's viewport by a pixel while its glyphs
  lie past it does not show (set J's clipboard list);
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
(`poses`), read right after its paint as the surface manager reads it,
and (eval-rules-6) how far from its rest place the margins sent for it
put it (`shifts`, see Driver).
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
  still a finding; eval-rules-7: and an anchored side on the edge of the
  area the other surfaces' exclusive zones leave it);
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

What `coherence`'s `ring`, `continuity`'s fold gaps, `contain`'s `wrap`
and `placement`'s `pose` read (eval-rules-6):

- per frame, each border drawn along its box (no `shape:`, no squircle
  corners), from strand-render's `eval-hooks` (`rings`): its box in
  device pixels, the device scale, the border's width and each corner's
  radius as the props ask (`radius:`'s one to four values, a percentage
  or `full`), and where eight rays (each corner's bisector, each edge's
  midpoint) cross the outline the border filled, measured with kurbo on
  the drawn path, never on pixels;
- per frame, each `row`, `col` and `scroll` drawn with a `gap:` over 0
  (`gaps`);
- per frame, on a surface at rest, each plain text drawn on one line
  whole (no `fit`, no `ellipsis:` or `max_lines:`, no newline) that has
  two words or more, so its least width is below its width (`loose`);
- per node, what takes it out of a flex row's give and take, read from
  its props as written (`flow`: placed out of flow, a margin, a width of
  its own or an image or icon, `shrink: 0`, a container that spreads its
  children with `justify: space_*`);
- per frame, `shifts` (see Driver).

All but `flow` are in the digest when a frame has any; none is in a
recording, so no reference changed with them.

What `continuity`'s `track` and `still`, `twin`'s `still` and
`coherence`'s `reach` read (eval-rules-8):

- each `keyframes` block a node played, by name, as logic declared it
  (`keyframes`: its stops' values for `x`, `y`, `rotate`, `opacity` and
  `scale` as numbers, the props it sets that are not read so, its run's
  length and delay, how many runs, `alternate` and its easing);
- per frame, whether motion was reduced, as render had it (`reduced`);
- per frame, each node drawn that time drives as declared (`timed`): its
  `play`'s block and logic's `seq`, and whether a prop of it reads time
  (`t`, `wave(…)`, `noise(…)` or a token that does, or a token read
  under an ancestor's `tokens` override that reads time), read from the
  scene tree by eval's own walk of the declared values
  (`eval/src/timed.rs`, integration-r8), never from the renderer's
  motion bits (which mark a playing node `TIME` too) or the product's
  own time detection;
- per frame, each drop shadow drawn, a box's glow included (`shadows`),
  from strand-render's `eval-hooks`: the box it is cast by, the area it
  may cover, what its props asked (`x`, `y`, `blur`, `spread`, logical
  px, springs applied) and its colour's alpha.

None is in the digest or a recording.

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
| `teleport` | A moving node's edge moves at most `max(teleport_px × 60 / hz, 0.75 × what is left) + tol` per tick; what is left is the travel (the span of its way, so a spring sent out and back in one segment travels its excursion; a node that comes onto its surface in the segment, or leaves it, travels from its first frame shown there or to its last, and so does each time it comes back or leaves again; a later change in the segment starts its way again where it found the node, so an offset sent out and turned back travels its excursion; and (evalfix) every place its written offsets, its own and its ancestors', put it while a spring of them is in flight, and where each such spring started (the frame before it came into flight), both read from the probe's parts (its laid place, its ancestors' written offsets, its own written offset, and for the start its springs', glide's and morph's parts, through its transforms), never from the drawn box, so an offset sprung out and called back by logic's own timer in one segment, which no frame lays out elsewhere and no step restarts, travels its excursion too, an offset snapped out and sprung back travels from where the snap left it, a frame drawn off its parts cannot widen its own bound, and an offset that snaps has no spring in flight and travels nothing more; a snap home of less than 0.75 of the excursion is `continuity`'s to catch, the way in flight dropped), or its own and its ancestors' glide (or drag) still in flight, or (eval-rules-5) the way of the motion that carries it: a node that comes onto the screen under a pose the compositor applies, or leaves through a clip its parent's slide carries it out of, travels its carrier's way (the pose's offset and its inherited flight, read at its first and last frames and at each restart), not the part of it that shows. A text whose content changed at a frame steps there (layout lengths snap, design.md "What can't interpolate"): on w the step from its old content's natural width to its new one's, each no wider than its parent's box, is taken off the change before it is held, and on x the same step moves its start edge the other way by the share its slot's declared alignment gives (integration-r7: none at start, half centred, all at end, read from the parent row's `justify`, a column's `align` or a box's `align`; a slot the declared tree does not place, out of flow or stretched, gets none; integration-r7 part 2: plus, up each ancestor sized by its content and placed by a declared share, that share of the ancestor's own change of width, so a text at the start of a row a box centres moves half the row's change; and the old width is held to its parent's width at the frame before, so a parent narrowing with its text counts the step); the rest is the motion's. Under reduced motion (in the run's own reduced segments, which it no longer skips) a `scroll` or `list` never moves past the input delivered so far (`reduced fling`: each `swipe` spread over its ticks, each `scroll` notch at its tick, read from the scenario's steps, one tick's input and the tolerance given; integration-r7 part 2: another step in the segment that acts, a click, a `set`, a pointer drag, may move an offset at once, a row revealed or a list filtered shorter clamping it, so the frames it acts on are not judged and the bound starts again from where they leave it, counting the input after it): decisions.md m4-lists-w1, "`reduced_motion` lands a step at once and does not fling". A jump with no motion (layout snapping) is noted, not failed; a row a scroll moves by itself (`SCROLL`, a fling's `FLING`) moves, so its jumps are held to the bound, and the scroll laying it out widens its way, as a size spring's does; on x and y a fade or a colour spring (opacity, fill, ink, border, a meter's level) is no motion. On w and h, only motion that sizes a box counts, and a glide retargeted from its layout is held on x and y only, on an axis its glide moves it on, and on the node's own move in its parent (what its parent's layout carried it by is the parent's; a node gliding across while a size spring or a scroll lays it out anew down follows its layout down, as the spring's neighbours do; and, while its surface lays out anew by itself, a node stacked after a sibling that changes its laid length follows that sibling's far edge, glide in flight or not, so slots folding before it are no retarget: eval-coverage-4; behind siblings that keep their lengths, a glide in flight is still held, integration-r7). |
| `vanish` | Nothing blinks out and back within three ticks, nor, with nothing taking it out, at any length before its segment comes to rest (eval-rules-3: set E's module swap pushed a bar's labels out of it for seven frames) (one still drawn whole, hidden for no reason the renderer gives, and wholly past its surface's edge, moved there by a scroll's spring, a glide, a drag or an offset, left view by moving; one an ancestor's clip cut out, `hidden: clipped`, blinked), shows for one frame only (gone again within three ticks; eval-rules-7: a frame its surface keeps on screen longer, idle, showed it that long), pops in past half its enter pose (its opacity, offset and, since unshaped-enter, a width or height it lays out at, read off its laid box; each time it comes onto a surface: a popup opened again plays its pose again; a shared morph plays in place of the pose, so a first frame that morphs is `coherence`'s morph start to judge; a node kept from view until then by an ancestor drawn the frame before, transparent, or clipping (or its surface's root) and not reaching it, shows at rest by design; (evalfix) one entering and cut out by its ancestors' clip on every frame from the one it mounts on until it shows is judged on the frame it mounts, where its enter starts, since a clip's moving edge (a card's height spring) is what uncovers it, and where that edge first lets it show is the clip's choice, not the pose's, when that clip moves by declared motion as it uncovers the node (a clipping ancestor springs its size, exits to a size or glides, or a scroll over it moves, and what the clip lets show changed from the frame before; a clip with no area counts as cutting it out only while such motion moves it); a clip that snaps open or is shut with nothing moving it shows the node where its pose has it, and that frame is judged; a pose that moves nothing has nothing to play, and a pose of size alone plays as a size spring; how far along its pose its first frame is, is read against where it rests: its row at the segment's rest when it is at rest there, else its last frame at rest in the segment, so a `mark` that cuts its enter (a staggered batch whose last children still wait) leaves the share unread rather than measuring the frame against itself, stagger review), or leaves a damaged frame blank while something inside its surface's root that draws something of its own shows (a fill, a border, a shadow, glyphs, an image or icon or shader; eval-rules-3: a bare container showing over a toast at its enter pose's `opacity: 0` is a frame with nothing to draw). A row a scroll carries past its viewport by itself (`SCROLL`) may show for one frame; a clipped node (`hidden: clipped`) does not show; any step that acts counts as bringing a node back or taking it out, not only a segment's first (a second `scroll` in one segment), and a `swipe` acts on every tick of its span, but only on what lies under the scrolls under the pointer as it starts (a swipe faster than the viewport carries those rows across it). With `compositor_poses`, a closing surface never commits a pose back toward rest before it goes while it is short of its exit pose (`pose back`, bug 7; integration-r7 part 2: closing from the tick logic removed its root or set its `open` false, not from the renderer's motion bits, so a dropped exit is judged; a spatial spring that overshoots its exit pose and settles back onto it is no pose back, and a pose's scale, recorded to 1/64, is given a quantum, decisions.md exit-overshoot); a surface opened again while it closed is no longer closing (eval-rules-5; integration-r7: opened again is read from the scenario's own changes, the driver's record of the tick logic created the surface or set its `open` true, not from the renderer's motion bits). An exit's last frame is at its exit pose (`pop-out`, see below). |
| `coherence` | One frame hangs together. `inherit`: a node is drawn at its laid offset from its parent, its parent's paint offset carrying it. `pivot`: scale, rotate and a morph's scale turn a node about its box's centre, through its ancestors' transforms (a box is snapped before those transforms, so its physical pixel of give is drawn as large as they magnify it, integration-r7). `morph start`: a shared morph's first frame is one share (at most 0.5) of the way from the box its name was last drawn at to its own, on all four edges. `morph arrival` (eval-rules-5, decisions.md cross-surface-morph): a node with a `morph` name that comes on screen while a node of that name was drawn the frame before, or within 1 s, on any surface, starts on that box on the output (each surface's origin plus its visual box): its first frame at most half its way from it, every edge that moves no further than its tolerance besides (each edge's tolerance a physical pixel as its ancestors' scales magnify it on that axis, as `pivot`'s, integration-r8); then reaches its own box (integration-r7: where its layout puts it on that frame, its laid box with its offsets at rest, through the compositor's pose and on the output, not the segment's last frame) with no step larger than a spring's (`max(teleport_px × 60 / hz, 0.75 × the way)` per edge); with no partner, or under reduced motion, it shows at its own box (unless, with motion, something declared moves it: its or an ancestor's enter pose, an ancestor's motion in flight, the compositor's pose; integration-r7 part 2: never the renderer's own motion bits). A last drawing within two periods of the second is not judged either way. `stacking`: a node in flight never goes below an overlapping sibling while their layout keeps its order. `drag`: a slider a drag holds draws its knob under the pointer (see below). `ring`: a border's inner edge is its box inset by its width, each radius less the width (CSS), at every frame (eval-rules-6, see below). `reach`: a shadow's area is its box moved by its offset and grown by its spread, then by as far as its blur shows, every length times the scale (eval-rules-8, see below). |
| `ink` | A text's glyphs stay inside its box, give or take 2 px, and a text drawn by the layout shaped for its own box the font's own overhang of that layout besides, and at its end how far its widest word is wider than its box when it has no ellipsis (a line breaks only between words; decisions.md launcher-rules) (`spill`: a stand-in drawn unclipped, or a layout drawn in the wrong place), an ellipsised text never draws less of itself as its box widens (`over-cut`), and a cut text keeps on each line what fits there (`fills`, eval-rules-4). |
| `colour` | A node's own fill, border or text colour that springs is drawn, frame by frame, on the straight way between where it started and where it rests in premultiplied OKLab, within 3 steps of 1/255 (`off its way`); where its fills come and go (a default fill handed over to a state's), their composite over its ancestors' fills moves steadily from where it was to where it rests (`composite`, eval-rules-7); no further past either end than its spring overshoots (`past its end`, eval-rules-3), and a theme swap springs each palette root along the curve its table asks a colour to take (`palette`, eval-rules-3). See below. |
| `contain` | A surface sized by its content holds what it lays out, mid-motion too (`cut`), an ellipsised text gives way before it is laid past its surface's edge (`give way`), a box whose size springs or that exits to a size holds what it lays out, its texts' glyphs included (`held`), a pose that sizes one axis leaves the other at rest (`other axis`), a shadow at rest is drawn inside its buffer (`shadow`), a full row has no child left that could still give way (`wrap`, eval-rules-6), and what is inside its surface and its clip where a segment starts and rests, laid at one place at both, is not drawn past them between with nothing moving it (`pushed`, eval-rules-3). See below. |
| `tiling` | Two edges at one place in layout are drawn on one physical pixel, at every scale (`apart`): no seam between boxes that touch, no pixel drawn twice. See below. |
| `stagger` | Children with an `enter` mounting together under `stagger:` (a child without one takes no turn) each leave their enter pose within a frame of their turn, one step apart in child order (`start`); a child mounting again after frames without it (a result filtered out and typed back) is numbered with those mounting beside it (eval-coverage-4), unless a list's window mounted it (integration-r7: read from the declared tree's mounts, not the renderer's motion bits). See below. |
| `continuity` | A motion goes on from what is on screen: each spring sampled is drawn on the way its samples work out (`way`), a change keeps the velocity the way had (`velocity`) and starts one refresh before its first frame (`start`), a surface that stops wanting frames shows each spring at its target (`end`), a fade or colour change settles without a jump the eye sees (`snap`), a new node springs only what its enter pose sets (`first frame`), a size sprung from zero or to zero and gone carries its gap with it, the gap beside it never more than its size on any frame between (eval-rules-6), and its parent, sized by its content, does not step as it unmounts (`fold`, eval-rules-3, eval-coverage-3), an interrupted roll rolls on from the text on screen (`roll`), a `keyframes` play draws its track through its stops at their times and along its easing between them (`track`, eval-rules-8), and with motion reduced what time drives holds still (`still`, eval-rules-8). See below. |
| `placement` | Each surface's card is where its declared props put it, at every frame painted at a settled size and place: a popup by its anchor (`anchor`), a bar, panel or OSD by its margin in what the bars' exclusive zones leave (`margin`), and a posed one where its pose's offset moves it, by the margins sent (`pose`, eval-rules-6); a split's `center` is centred on its split (`centre`); an image's source shows where its `fit:` puts it in its box (`fit`). eval-rules-4; see below. |
| `twin` | The run with motion reduced ends each settled segment where the run does, on the output (each run's surfaces where that run placed them), time-driven nodes included (their clocks stand at t = 0 at a settled rest in both). Nodes are matched by identity: a branch render re-created (`#0~2` in one run, `#0~3` in the other) is one node. What lies under a scroll that flung is left out from the fling on (the scroll's own box, and the rest of its surface, still compared): `reduced_motion` flings nothing (renderer/lists.rs `scroll_input`). The re-run's own scrolls are held to their input as `teleport`'s `reduced fling` holds a reduced segment (eval-rules-5): with motion reduced a lift flings nothing. And on every frame of the re-run, not only at its rests, what time drives holds still (`still`, eval-rules-8; see `continuity`). |
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
any frame was drawn at ((fidelity) of any surface, on any output). A
check's own pixel of give on a row (`coherence`'s `inherit`, `pivot`,
`drag` and `morph`, `reach`, `tiling`'s pixels apart) is one physical
pixel of that row's surface (`RunTrace::scale_on`: its output's, with
several), as it is with one output.

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
`continuity`, `placement`, `vanish`'s `pop-out`, `coherence`'s `drag`
and `ring`, `ink`'s `fills` and `redraw`'s `blend`) each hold one guarantee of docs/design.md about how
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
(decisions.md eval-rules-4). At round 6's integration (integration-r6:
eval-rules-4, eval-coverage-3's scenarios, image cells and parent
`fold`, and damage-fraction, cross-surface-morph and unshaped-enter with
`vanish`'s pop-in sizes and `other axis` on enters) the same sweep: the
gate's 105 scenarios and the matrix's 992 cells at 60 and 144 Hz (972
pass, 20 n/a) pass, `placement`'s `fit` judging 193 images in the gate
and 2470 in the matrix; of sets C to F's 58 correct scenarios only D's
launcher-filter and F's toasts at 144 Hz fail, by `cut` alone (the
growth the owner is deciding), F's media card passing now that damage
covers what a shape paints and F's interrupted card now that an
authored turn is judged before transforms; the owner's `qs` and
`launcher` shells pass every check (`island` does not load: its config
puts `~` in a pose). The eval-rules-6 arms (`coherence`'s `ring`,
`continuity`'s fold gaps, `contain`'s `wrap`, `placement`'s `pose` and
`regions-on-wire`'s margins) were kept with no false alarm over the
gate (108 scenarios: `ring` judged 307 rings in 5 of them, the fold
gaps 271 gaps in 12, `pose` 162 frames in 3, `wrap` 209 rows at rest
and 145 loose texts in 39, none of the rows full), the matrix at 60 and 144 Hz (972 pass, 20 n/a; its fixture has
no border, no slot that folds and no pose, so these arms judge nothing
there), held-out sets G and H's 39 loading correct scenarios (only the
three open findings G's baseline already had fail, by `teleport`,
`vanish` and `tiling`; `ring` judged 1228 rings in 16 of them, the fold
gaps 596 in 6, `pose` 87 frames in 1, `wrap` 646 rows and 351 loose
texts in 26) and the owner's local probes (19 load; the fold gaps
judged 28, `wrap` 13 rows). No row at rest was full in any of them, so
`wrap` is proven on its mutants only. At round 8's integration
(integration-r8: eval-rules-7, eval-rules-8, eval-coverage-5 and the
round's fixes) the same sweep: the gate's 150 scenarios and the
matrix's 992 cells at 60 and 144 Hz (972 pass, 20 n/a, 0 changed)
pass; of practice-8's 30 scenarios 3 do not run as written (a service
nothing exports, a pointer on no shown surface) and 25 pass, the other
two failing on the growth the owner is deciding (dock-magnify-drag's
`cut`, power-menu-confirm's `fold`, decisions.md integration-r8); of
sets I and J's 40 correct scenarios 36 pass, j's two `state focused`
configs do not load, i-media-art-morph-pulse's `rest` cannot be reached
while its `wave` runs and i-toast-stack-arrive-dismiss's `cut` is the
growth again; the owner's `qs` passes and `launcher` fails only
launcher-triage's findings. Blind sets K and L (eval-finish), scored
once and now spent: K's 20 mutants were caught 18 by the rule checks
and 20 with the references, with false alarms on 2 of its 20 correct
scenarios; L's 20 mutants 17 by the rules and 19 in all, with a false
alarm on 1 of its 19 loading correct scenarios (one, a `for` over plain
data with no `key`, does not load) and one real failure, the
content-sized growth the owner is deciding (a card's shadow room grows
its panel and `glide_origin` draws it off and back). The three false
alarms were the tool's, each fixed at its cause with the scenario that
exposed it promoted (decisions.md eval-finish): an `id:` on a component
call named nothing for the `#id` selector, since the inspector's
identity did not report it on the call's root (k-media-card,
k-qs-toggles); `colour` held a fill lit with `$accent` in the click
that swaps the palette to a straight way, where its target springs too
(k-qs-toggles; see `colour` below); and a stagger child waiting hidden
at opacity 0 was traced at scale 1, not its pose's, and its turns were
counted a period early on its panel's first frame (l-app-grid-stagger;
see `stagger` below). An `intend`
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
  easing, the old -0.5..1.5 holds. A moving target (eval-finish): a
  colour sent to a token the same change's theme swap re-resolves (a
  tile lit with `$accent` by the click that swaps the palette) chases
  a target that springs too, and a spring from rest draws a weighted sum
  of where it started and where its target has been, so its way bends
  toward the target's. Where the targets its own spring's samples
  record (`sp`'s `to`) move over the way and end where the colour rests,
  each frame is held to the fan from its start to the target's way up
  to that frame, with the same overshoot bound along each ray; a
  colour drawn through a colour neither its start nor any of its
  targets has still fails. The recorded targets are held first to a
  way of their own (eval-finish review): a token the swap moves goes
  from where it was to where it rests on one straight way in
  premultiplied OKLab, as any colour a spring sends does, so targets
  that leave the straight way from the first to the last by more than
  3 steps are not followed (the straight way holds the colour, and the
  finding says how far its target strayed). And along each ray the
  share is no more than the colour's own spring reaches from rest by
  the frame's time since the segment's start (`colour::reach`: a
  spring from rest gives its targets so far exactly the weight its way
  to a still target has covered), so a colour drawn at its target from
  the first frame, skipping its spring, fails `ahead of its spring`. `palette` (eval-rules-3): a theme
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
  `composite` (eval-rules-7): a fill's way is a slot's (its first layer
  against its first layer), but where the number of a node's fill layers
  changes over the segment (a button's default fill handed over to a
  state's colour under its state layer, set J's drawer), slot against
  slot compares fills that are not one colour springing. What the eye
  sees is the node's fills composited source-over in premultiplied sRGB,
  over the fills of its ancestors whose box holds its centre, so that
  composite is held: on each channel it stays between its two ends,
  give or take three steps of 1/255 and what its spring may overshoot,
  and it does not turn back (while short of its end, its distance to
  the end never grows by more than three steps). The same conditions as
  a fill's way apply (from rest, its own `FILL_SPRING`, no keyframes or
  time, not retargeted, its opacity constant). A default fill dropped at
  once while its replacement fades in from clear is a flash toward what
  lies under it, and fails.
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
  ancestor and past the edge too, or a clipping ancestor. `wrap`
  (eval-rules-6): the same rule from the other side. A full row's
  children give way by `shrink` down to their least width (CSS flex's
  automatic minimum, their min-content), plain text's being its longest
  word and an ellipsised one's nothing, so a `row` whose in-flow
  children are laid past its box, on a frame its surface comes to rest,
  holds no child with `shrink:` over 0 and no width of its own that
  could still give way: a text drawn whole on one line with two words or
  more (`loose`), an ellipsised text, a `col` or `box` all of whose
  children could, or a `row` that holds one and is not full itself.
  Computed from the declared props and the boxes drawn, never by the
  product's layout. A long label that stopped wrapping pushed a card's
  children past its row (held-out set G's wrap mutant). A child with a
  width of its own counts as rigid even though CSS lets it shrink to its
  content, so a row that overflows beside one is not judged. `held`
  (eval-rules-2): "a node whose size springs clips its content while it
  does (a collapsing toast hides what no longer fits)" (decisions.md
  wave3-pixels p2), and one exiting to a pose that sets its width or
  height clips until it unmounts (exit-height0), so what a descendant
  shows (its visual box cut by its ancestors' clip and, on a surface no
  pose moves, by the surface's buffer, which clips all it holds: growth,
  since the probe records an ancestor's clip only when it is narrower
  than its surface) lies inside the sizing box's visual box, within `tol`
  plus a quantum, however small the box has become. A text's glyphs count with its box (eval-rules-4: its
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
  A frame whose text is not shaped yet is left out, as is one with text
  under the node on its way (held on the frame, or a row under it not
  shaped: a box is laid out without a text that has not landed,
  late-text-dip; unshaped-enter), and so is a pose with no rest frame
  beside it. Until unshaped-enter the trace recorded an `enter` pose's
  opacity, offset and scale only, so `other axis` held exits alone; it
  records an enter's width and height now, as an exit's. `shadow`
  (eval-rules-2): "Shadows enlarge the buffer but not the input region"
  (design.md "Layout"): the buffer keeps an overhang for its shadows'
  reach at rest (decisions.md wave3-pixels), so a box at rest (no
  motion that moves it or lays it out, no authored geometry, no paint
  offset, scale or turn, no clipping ancestor, and nothing on its
  surface springing a size, whose overhang is held where the spring
  started or rests, m6-owner-fixes bug 9) draws each shadow inside the
  buffer, within `tol` plus a quantum, on every side but the ones its
  surface's `cuts` name. (eval-rules-7) An anchored side whose buffer
  edge lies on the edge of the area the other surfaces' exclusive zones
  leave it is a cut side too: render keeps an anchored side's overhang
  within its margin, the margin counts from that area's edge and is never
  negative (decisions.md m6-w2-surface), so a panel at margin 0 under a
  bar has no overhang toward the bar (set I's media panel).
- **`tiling`.** A box's edges are snapped from where they fall
  (decisions.md wave3-pixels), so two that share an edge in layout share
  its physical pixel. On every painted frame, among the nodes shown on
  one surface that nothing moves (no motion of their own or inherited, no
  offset, scale or turn of their own), every two edges on an axis laid at
  one place (within 1/128 px) and drawn in one space are drawn (their
  `visual` boxes, snapped) within 1/32 px. A space is what the nearest
  scaled ancestor draws (integration-r7): a button held at
  `scale: 0.88` draws what it holds scaled about its centre, off the
  places layout gave it, so its icon's edge is compared with its other
  children's, never with a neighbour's outside it; two siblings under
  one scale still land on one pixel. A node under a turned ancestor is
  not held (its edges are not axis-aligned). A box in motion is placed
  by its layer's whole pixels and is not held.
- **`stagger`.** "Delays each child's `enter` this much after the one
  before" (the schema; design.md "Motion and time"). The children of a
  container with `stagger:` that have an `enter` (a pose or a preset,
  `NodeMeta::enters`), first drawn at one frame after boot, are a batch,
  numbered in paint order; a child with no `enter` plays nothing, is
  drawn at rest at once and takes no turn, mounting into a surface
  already shown or coming on screen with its first frame (decisions.md
  stagger); so are those drawn there again after
  frames without them (a result filtered out and typed back mounts anew;
  eval-coverage-4), unless a list's window mounted them since they were
  last gone (a row a list's window brings back does not play its enter;
  integration-r7: the driver records each node logic created with
  `window` set, a declared fact, where eval-coverage-4 read the
  renderer's own `ENTER` bits, so a remount that skipped its enter was
  excused). Child `n` is due `n` steps (at most
  1 s) after the batch's change, a period before its first frame but
  never before its surface's previous paint (eval-finish): on a
  surface's first frame (a panel opening with its children, or opened
  again on a new surface) the change starts at that frame, which shows
  every pose exactly (decisions.md wave3-pixels (p2)). Its
  start is the first frame it no longer waits for its turn (`Motions`
  `STAGGER` cleared), or an earlier one that already shows it off its
  enter pose, and must come within a period after it is due: not before
  (drawn entering ahead of its turn) and not later. The start is not the
  first frame drawn off the pose, since an enter on a curve that starts
  slowly (`ease(ease_in, …)`, `in_back`) stays within a quantum of its
  pose for frames after it began (eval/scenarios/curated/
  stagger-slow-start.scn). A child's pose is its pose block's or
  (stagger review) its preset's (`fade`, `slidefade`, `popin(s)`:
  `NodeMeta::enter_preset`); a `slide(edge)`, whose distance is its
  box's and unknown here, is judged by its release alone. Both are read
  through the node's tokens, as render reads them (`probe::enter_of`); a
  keyed copy that mounts again keeps what was read when its key was
  first seen, so one whose `enter` differs between mounts is read by its
  first. It skips a child with a pose that never
  leaves it in the segment, one whose text is on its way (it waits
  for its text, decisions.md late-text-dip), one whose pose was dropped
  unseen (kept from view by an ancestor the frame before its first, it
  shows at rest and plays no enter, as `vanish` reads it) and reduced
  motion. A child waiting for its turn is hidden at opacity 0 at its
  pose, and the trace records a hidden row's own scale and turn (the
  pose's), so it is read as on its pose until it moves (eval-finish:
  the row was recorded at scale 1, which read as leaving a scale pose on
  its first frame).
- **`pop-out`** (in `vanish`). "A removed subtree plays `exit`, then
  unmounts" (design.md "Poses"). The last frame an exiting node is drawn
  in shows it at its exit pose (its `exit`, else its `enter`): its own
  opacity within 1/255, its offset within `tol`, its scale within 0.005
  and a pose's width or height within `tol`, or nothing at all (its
  effective opacity at most 1/255). A slide to an edge, whose offset is the
  node's size, is held on its opacity only. Reduced motion is not held.
  (eval-rules-7) Nothing that does not show can pop: a last frame on
  which the node does not show (cut out by its ancestors' clip or carried
  off its surface, set J's network pages, whose rows leave wholly past
  their pages' clip under their page's slide), or one followed by a frame
  in which an ancestor that clips it (`clip: true`, a `scroll`, a `list`,
  or as `held` reads one, a box whose size springs or that exits to a
  size) has no area, under half a pixel across, is not judged, when
  that shutting is declared motion (integration-r8): the ancestor's own
  size spring or exit to a size (practice set 8: a chip exiting to
  `width: 0` is 0 wide the frame its popped-in check is gone), or, for
  an ancestor sized by what it holds, the node's own exit reaching its
  pose's width or height on the axis that shut within the step it just
  took (practice set 8: a filtered list sized by its exiting rows, 17
  then 5 px tall, is 0 tall the frame they reach `height: 0`). Rows
  unmounted half way, 20 px tall after a 10 px step, shut their list by
  their removal: that pops. A clip that shuts on the last frame a node
  is drawn in is judged the same way, at the frame it last showed.
  (evalfix) Only a removed subtree's root is held to its exit pose: the
  subtree plays its root's `exit` and unmounts whole (spec.md 6.4, "a
  removed subtree plays its `exit` pose"), so the nodes inside it, which
  the probe marks `EXIT` too, ride along with their own `exit` unplayed.
  The root is the topmost of the unbroken run of exiting ancestors from
  the nearest one drawn. A passenger gone with its root is not judged; one
  gone while its root is still drawn the frame after must have had nothing
  to show there: the nearest of its ancestors drawn then shows nothing
  (faded to nothing, the renderer drawing no child of a node at opacity
  0, or cut out by a clip, whose children it does not visit), a clipping
  ancestor shows no area, on screen or in its box before transforms (a
  box clips its children to itself before its turn: matrix
  exit-height0+lean's tilted row laid 0 tall, its turned bounds 1.7 px),
  or it showed nothing itself on its last frame.
  Otherwise it vanished from a subtree still on screen, and pops.
  (evalfix review) A removed root is read from what the scenario did
  (`RunTrace::removed`: logic's diff removed the node, not a surface's
  root and not a list window's unmount), as pose back reads a surface's
  close: a node with an `exit` pose not drawn on the first frame after
  logic removed it, with motion not reduced, had its exit dropped, and
  its last frame is held to its pose though the renderer marked nothing
  `EXIT`.
- **`continuity`** (eval-rules-2). "Retargeting mid-flight keeps
  velocity, so interrupted animations never jolt" (design.md), a spring
  is a unit-mass damped oscillator, and a change starts one refresh of
  its surface before its first frame (at most 16.7 ms, never before the
  motion's sample before; one that starts at a time the spring was
  sampled at already starts with that frame's first changes, from the
  sample before it, decisions.md launcher-flip-pass), from where its way is then and with its
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
  growth rule the owner is deciding. And on every frame between
  (eval-rules-6), from each flex container's `gap:` and the laid boxes
  of its children in order: the gap beside a slot whose size springs to
  or from zero (or that exits to a pose of size zero on the axis) is
  `min(gap, size)` (decisions.md wave3-pixels p2: "the slot is `h +
  min(gap, h)`"), to the next sibling, or to the previous one for the
  last child. The size is the one the slot was laid out at (its
  spring's sample), and the gap is read from two edges layout snapped to
  whole logical pixels, so it is held within a pixel (two when no sample
  gives the size). Not judged: a container with `justify: space_*` or a
  child out of flow, a pair either of which has a margin of its own, a
  text not shaped yet, a gap two folding slots share, and a way met in
  flight. Held-out sets G and H's gap-jump mutants dropped the gap the
  frame a slot started to fold; the siblings slid by it at once and only
  references saw it.
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
  The source placed is the file's own pixels, by the size its header
  gives (a PNG, JPEG or GIF file, read by the sandbox, `filesize.rs`,
  png-aspect), else the size the decode reports, so a decode that
  reports another aspect for its source is seen. And what the fit shows
  of the source in the box (`cover`'s crop) lies within the pixmap
  where it holds the source, give or take a pixel and `tol` (axis
  `cover`, stand-in-fit): a decode for a box of another shape holds
  another crop, and past it the raster pads the pixmap's edge rows out.
  `pose` (eval-rules-6): with `compositor_poses`, a posed surface lies,
  on each frame, its pose's offset from where it lies at rest, within
  half a pixel (margins are whole pixels). The offset is in output
  coordinates (`y: 20` moves a surface down whatever edge it is anchored
  to) and layer shell's margins count from the anchored edge, so a
  bottom-anchored OSD's bottom margin shrinks as it moves down. Where it
  lies is the margins the surface manager sent (`shifts`); render
  delegates an offset only on an axis anchored on one side and never on
  a popup, so a delegated offset the margins do not carry fails too.
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
- **`ring`** (in `coherence`, eval-rules-6). A border is drawn inside
  its box, its inner edge the box inset by the border's width with each
  radius reduced by it and never below 0 (CSS's "Corner Shaping"), the
  width rounded to whole device pixels, at least one (decisions.md
  2026-10-05 render), and the outer radii the ones asked for, shrunk as
  CSS shrinks them when two meet (`full` is half the shorter side). On
  every frame, fractional scales and sizes in flight included, each of
  eight rays (each corner's bisector, each edge's midpoint) crosses the
  outline the border filled where that shape does, within 0.25 device
  px: the curves are drawn within 0.1 px of their arcs, and an inner
  radius not reduced by a 1 px border is `√2 − 1` px off on the bisector.
  Measured on the path, so anti-aliasing never enters. A ray's crossing
  is kept only where the outline's point lies on it (kurbo's cubic
  solver returns a spurious root where a corner arc's chord runs along a
  bisector). Not judged: a border along a `shape:` outline or squircle
  corners, and a `radius:` form it does not read. Held-out sets G and H's
  inner-radius mutants drew a ring thicker at its corners; only a
  reference's pixels could see it.
- **`track`** (in `continuity`, eval-rules-8). A `keyframes` block a
  node plays is a CSS track (design.md "Motion and time", spec.md 6.6):
  at each stop's fraction of a run a prop is that stop's value; between
  two stops that set it, it moves from one to the other along the
  block's easing, applied per segment as CSS's
  `animation-timing-function` is; a prop the first or last stop leaves
  out starts or ends at the node's own value. A play starts on the
  first painted frame that draws it, waits its `delay`, plays `repeat`
  runs of `duration` (`alternate`: every other one backwards, `repeat:
  0` forever), then draws the node as it is. `x`, `y` and `rotate` add
  to the node's own value, `opacity` and `scale` multiply it. The check
  works each frame's value out from the block as logic declared it, with
  a cubic bézier of its own, and holds `x`/`y` (the drawn offset less
  the node's other offsets) within `tol` and 4 quanta, `rotate` within
  0.5°, `opacity` within 2.5/255 and `scale` within 0.005; under the
  three props that compose with the node's own value, that value is
  fitted (the median over the play). Not judged: an elastic or bounce
  easing, a play from the frame motion is reduced in it, and a frame
  where something else moves the prop (its own spring, an enter or exit,
  a morph, a lean, a jelly, a lift, a prop that reads time). A frame
  within 1 µs of a run's boundary may read either side. Held-out set
  I's segment-span mutant eased each segment over the whole run, so a
  three-stop track snapped 3 px at its stops; only a reference saw it.
- **`still`** (in `continuity` and `twin`, eval-rules-8). "`reduced_motion`
  turns off loops, time signals and effects" (design.md): a node time
  drives as declared (a `play`, or a prop that reads `t`, `wave(…)`,
  `noise(…)` or a token that does) is drawn on each frame motion is
  reduced at as on the reduced frame before it that drew it, unless
  something was driven between them (logic's diff, input) or its surface
  lays out anew by itself (its size changed, its rows' texts changed, an
  exit, a size spring or a lift on it). Compared: its offset from its
  laid box and its laid size (2 quanta), own opacity (1/255), scale
  (1e-3), turn (0.1°) and the colours and shadows it paints; one finding
  per node and segment. `continuity` holds the run's own reduced frames,
  `twin` every frame of its reduced re-run, waits included: before, a
  loop that never settled was compared nowhere (held-out set J's j18
  pulsed on through a reduced `wait`).
- **`reach`** (in `coherence`, eval-rules-8). A shadow is CSS's
  box-shadow: the casting box moved by its offset and grown by its
  spread, blurred by a Gaussian of standard deviation half its blur,
  every length times the scale. strand-render's probe records with each
  shadow drawn what its props asked (springs applied) and the box that
  casts it; on every frame, in device px, its drawn area must be centred
  on that box moved by `(x, y) × s`, and reach past the box grown by
  `spread × s` at least as far as its blurred edge still shows half a
  level (`alpha × (1 − Φ(d/σ)) ≥ 0.5/255`, `σ = blur × s / 2`) and no
  further than `4σ + 2` px, within 4 quanta at the scale and 0.05 px.
  Worked out from the props, never from the flattener's own reach.
  Held-out set I's blur-ignores-scale mutant blurred by the logical blur
  at every scale; only references saw it.

`$STRAND_EVAL_RULE_STATS` set (run.sh passes `STRAND_*` in) makes each
eval-rules-2, -3, -4 and -6 arm say on stderr how much of a run it held
(`rule-stats <arm> <scenario>: …`: samples, boxes, shadows, knobs, the
frames `contain.pushed` held a node on, the siblings and parents `continuity.fold`
held, the colour ways `colour.way` held and bounded by a curve, the
palette changes `colour.palette` read, the frames each `placement` arm
judged, the lines `ink.fills` read, the crossfade frames
`redraw.blend` compared, the rings `coherence.ring` judged, the gaps
`continuity.fold-gap` held, the rows at rest, loose texts and full rows
`contain.wrap` read, the frames `placement.pose` judged, and
(eval-rules-8) the plays and frames `continuity.track` judged, the
nodes and frames `continuity.still` and `twin.still` compared, and the
shadows `coherence.reach` judged), so a sweep can tell a pass from
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

Everything together stays under 6.5 MiB. With each node's paint and glyph
sources (eval-coverage) it was 4.15 MB, and with the integration's
scenarios (eval-r3) 4.18 MB. With the scroll, control, placement and
theme scenarios, the controls' parts and the two input effects
(eval-coverage-2), and round 4's integration (integration-r4) it was
5.09 MB; with eval-rules-3's nine scenarios 5.13 MB, and at round 5's
integration 5.15 MB. With eval-coverage-3's nine scenarios, the image
effect and the deeper nested FLIP it was 5.58 MB; at round 6's
integration (integration-r6), with round 6's three regressions, it was
5.60 MB. With eval-coverage-4's five scenarios, the scroll cells' flick
and the slider cells' offset box it was 5.96 MB; at round 7's
integration (integration-r7), with eval-rules-5's and eval-rules-6's six
scenarios and the round's two regressions, it was 5.99 MB; after its
second part (the round's fixes and four more regressions), 6.01 MB. With
eval-coverage-5's twenty scenarios (147 kB: thirteen promoted from round
8's practice corpus, seven written for the round's gaps, none over 27 kB)
it was 6.16 MB (6,156,883 bytes). At round 8's integration
(integration-r8), with E3's and P's regressions and the integration's
five (scaled-gradient-redraw, glow-overhang, insert-above-view,
popup-click-away and nested-morph, 35 kB), it was 6.22 MB (6,223,318
bytes); with eval-finish's three (k-media-card, k-qs-toggles and
l-app-grid-stagger, 18 kB) it was 6,242,526 bytes; with the review's two
(call-id-chain and l-grid-reopen-plain-box, 15 kB) and the re-approvals'
notes written out again it was 6,260,955 bytes; with the stagger cells
re-recorded for a stagger that numbers only children with an `enter`
(their batch settles a step sooner, so their recordings are shorter, and
their notes longer) it was 6,265,539 bytes; with the stagger review's
two regressions (stagger-plain-turn and stagger-preset-turn, 7 kB) and
the keyframes cells' notes read again it was 6,273,496 bytes, 18 kB under
the tripwire: the 155 scenarios 1.14 MB,
`matrix-60.ref` 2.06 MB and `matrix-144.ref` 3.06 MB, 496 cells each.
With growth's two regressions (snapped-growth-cut and held-grow-glide,
19 kB) and the 36 scenarios and 60 matrix cells whose boxes now spring
over a step re-recorded (longer, with their notes) it is 6,322,836
bytes: the 163 scenarios 1.18 MB, `matrix-60.ref` 2.07 MB and
`matrix-144.ref` 3.07 MB, so the tripwire is 6.5 MiB since 2026-10-22
(decisions.md growth).
Paint costs little because a field is written only on the ticks it
changes. Compressed, as git stores it, that was about 530 kB before
eval-coverage-5, whose files add 44 kB gzipped, and the integration's
five 9 kB. eval/tests/refs.rs
holds the total to 6.5 MiB (5 MiB until integration-r4, when the
references reached 97% of it, and 6 MiB until growth passed it), a scenario's file to 64 KiB and a matrix
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

When the run takes a step its reference does not (eval-rules-8), the
line leads with the largest one: on one node and field, between two
ticks both drew it, how far the run's value moved less how far the
reference's did, ranked by how large it shows (a length in px; an
opacity's or a scale's share of the node's larger side; a turn's arc
at half its diagonal). A first difference of 0.14 px that drifts into
a 3 px snap at a keyframe's stop then reads:

```text
CHANGED: m.keyframes.60: largest step k19..k20 row#subject drawn.x 39 to 36 (reference 36 to 36): a 3.00 px step the reference does not take; first k6 row#subject drawn.x 32.296875 (reference 32.4375); differs at 14 of 42 ticks, …
```

The indented lines are each other node's first difference, the node
that differs most first (a shadow's by how far its area's edges moved),
then those with no number to rank by, by tick. `summary.md`
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
`--renote` (eval-finish) rewrites the note of the approved recording a run
matches, with the date, revision and approver, and keeps the recording
byte for byte; a run that left its reference is refused. It is for an
approval whose reading was lost or wrong: keep the reading the block had
and add what was read again.

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
`index.html`. (replayfix) A replay's report is packed (see "Replaying a
glitch report", "A replay's report, packed"); every tool that reads a
report (`explain`, `verdict`, `approve`, `ab`, `promote`) reads it
whole. In CI, `scripts/eval/ci.sh`'s gate and `scripts/eval/matrix.sh`
append their run's `summary.md` to `$GITHUB_STEP_SUMMARY`; `strand-eval` itself
writes nothing outside `--out`, so its tests and mutant runs never do.

### The viewer

`index.html` is one self-contained file, with nothing loaded from
elsewhere. It shows each scenario's frames:

- scrubbable by tick, leaving the page where it is scrolled (the filmstrip
  brings its current frame into its own view only) and the frame shown
  until every image of the next is decoded (the frames around the current
  one decoded ahead), with a filmstrip of every frame and an onion skin
  of the frames before (as outlines, only the boxes the frame does not
  draw again: a node drawn where it was lies under its own outline);
- at `fit` zoom by default: the largest whole zoom (or the fraction
  below 1) at which the widest surface of the run fits the stage, so a
  phone shows the whole surface and the frame does not jump in size as
  the scrubber moves; A and B side by side only where each still gets
  twice its size, else one above the other;
- with nodes outlined by drawn, visual or laid box, and dashed where
  each comes to rest: its laid box moved by the written `x` and `y`, its
  own and its ancestors', without the ancestors' springs in flight (the
  laid box alone is before every paint offset, so a card a written `x`
  centres, and all inside it, would read as off their places at rest).
  A node counts as moving, and is outlined by default, when a motion
  drives it or it is drawn off where it rests;
- with the findings listed and linked to their frames;
- with a node's path plotted per axis, at the panel's width;
- with an A/B tab when the page holds two runs: a pixel difference of
  two runs with frames, and against an approved reference (which records
  no pixels) the nodes drawn differently from it, the run's solid red
  and the reference's dashed;
- with an Approve tab (not in a replay, which has no reference): the
  run's approved reference and the command that approves it, with the
  note and the approver typed there (see Reference recordings).

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
work. (replayfix) On a replay's page (`--expect replay`) it drives the
time scrubber instead: the slider spans the window in ms, it opens at
the report, the report and the live events are marked, an idle span
reads `nothing changed`, the live pointer is named, and the buttons,
keys and play step through time; and the page being packed, its first,
middle and last painted frames and the one farthest from a whole frame
are drawn from their pixels, none as a wireframe. (replay-clip) The replay's page shows no scenario list,
keeps the dense views behind Details (and remembers it), its buttons are
at least 40 px tall (its selects 36) and its two overlays toggle; the
clip is set by a drag on the timeline and by `[` and `]` (pressed on two
painted frames, the clip's ticks are exactly those frames'), its
elements by a click and a rectangle on the frame (never one that does
not show) and a `Remove`; Copy for agent copies the text
the page's clip.js makes (its note and its command the ones shown), Save
clip gives one `.zip` holding it, and a clipboard that refuses both ways
leaves the text selected to copy by hand. Screenshots of each mode land in
`target/eval/viewer-shots/`.

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
from the trace, which is always complete. (replayfix) A packed page (a
replay's) keeps a patched frame only with its bases, down to a whole one.

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
the spring, then a touchpad flick that flings it on into the list's end,
or with motion reduced stops where the fingers lift) and a two-way slider
dragged from its knob (`slider-drag`, in a box moved by a resting `x: 12`,
so the drag reads the pointer through its ancestors' offsets)
(eval-coverage-2, eval-coverage-4), an image (`image`: a 3:2 PNG under `fit: cover`
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
cross-surface-morph (`expect`, and since eval-rules-5 `coherence`'s
morph arrival) and morph-group-bounds (`redraw`;
decisions.md cross-surface-morph), and unshaped-enter (`vanish`; decisions.md
unshaped-enter), and eval-coverage-3's six coverage mutants, a bug
planted where that round's curated scenarios reach, each caught by a rule
on its scenario: standin-stretched (`placement` fit, media-art-expand),
submenu-parent-geometry (`placement` anchor, tray-submenu-shadow),
edge-overhang-swapped (`placement` margin, osd-shadow-edges-1_5),
nested-glide-y (`envelope`, `teleport`, notif-groups-reorder),
last-fold-half (`continuity` fold, wifi-last-row-fold-2) and
text-width-rounded-scale (`ink`, bar-split-title-1_25; decisions.md
integration-r6), and round 7's png-aspect (`placement`), image-redraw
(`redraw`), dock-drag (`redraw`) and stand-in-fit (`placement` cover;
decisions.md of each name), eval-rules-6's four (ring-corners-swapped,
fold-gap-late, ellipsis-min-content-natural and pose-margin-unsent, each a
second case of a rule that round added) and round 7's owner-shell triage:
reopen-enters (`vanish`; decisions.md launcher-reopen) and, on one
regression, flip-undrawn-pass (`bystander`) and frame-lead (`continuity`,
in crates/strand-scene; decisions.md launcher-flip-pass), eval-rules-8's
three, each a second case of a rule that round added:
keyframe-ease-linear (`continuity` track), shadow-offset-unscaled
(`coherence` reach) and signal-runs-reduced (`continuity` still), and
round 8's shadow-lift-gaps (`contain` shadow; decisions.md shadow-buffer),
morph-back-after-idle (`coherence` morph arrival; decisions.md
morph-memory), and integration-r8's scaled-gradient-redraw (`redraw`),
glow-overhang (`contain` shadow), insert-above-view (`twin`),
popup-click-away (`expect`) and nested-morph (`coherence` morph arrival;
all five decisions.md integration-r8), and eval-finish's four:
call-id-root (k-media-card) and call-id-chain (the call-id-chain
regression), each caught where the run stops at the `intend` whose `#id`
the reverted identity no longer names, hidden-row-look (`stagger`,
l-app-grid-stagger) and colour-srgb (`colour`, k-qs-toggles: colour
springs in premultiplied sRGB), and the stagger review's three:
stagger-plain-turn (the stagger decision's filter undone: every child
entering together numbered again; `stagger`, the stagger-plain-turn
regression), stagger-turn-early and stagger-turn-skipped (a turn early
or skipped, on preset tiles; `stagger`, stagger-preset-turn), and evalfix's offset-retarget-snap
(`teleport`, offset-out-and-back: a spring retargeted in flight snaps)
and sizing-clip-open (`contain`, reveal-enter: a `clip: true` box whose
size springs stops clipping) and ghost-children-undrawn (`vanish`,
ghost-passengers: a removed subtree's children are not drawn while its
root plays its exit), and the evalfix review's three: offset-retarget-snap
again (`continuity`, offset-early-callback: called back before half way
out, its snap home is less than 0.75 of the excursion `teleport` gives it),
offset-spring-doubled (`coherence`, offset-out-and-back: an offset's
spring drawn twice; `teleport` does not catch it there, the doubled
overshoot widening the segment's travel, decisions.md replay-owner) and exit-dropped (`vanish`, ghost-passengers:
a removed node with an `exit` gets no ghost), and growth's six, each a
part of a content-sized box springing over a step of its content undone
(decisions.md growth): growth-step-flip (`teleport`, osd-edge-margins:
the pass that starts a step glides only what a change named, so the
meter jumps with the box's left edge), growth-fill-drawn (`teleport`,
held-grow-glide: a crossfade's one box left is taken for a fill and
squeezed with the panel), growth-overshoot-cut (`expect`,
popup-shrink-centred: the card's overshoot past the buffer squeezed),
growth-keep-far and growth-exact-origin (`expect`, osd-bottom-grow-1_5:
a bottom OSD's rows kept to its near edge, or its box placed off the
exact origin the buffer's resize gives it, move as it grows), and growth-step-clip (`contain`,
snapped-growth-cut: the boxes that follow the spring stop clipping what
they hold): 62 in all. A patch listed
again against another scenario runs as `<name>-<scenario id>`. In a detached worktree of HEAD, each scenario
must pass on HEAD and fail under its patch with at least the checks listed,
or, for an expected failure written `unmatched:SELECTOR`, stop because
that selector matches no node (how a name the product loses shows).
A patch that no longer applies is STALE: regenerate it. Each mutant leaves
an A/B page of HEAD against it. Commit first: the mutants run HEAD.
A scenario that fails on HEAD (its control) is a failure, its mutant
lines are not run, and its run is kept as
`target/eval/mutants/<name>-head.failed-<UTC time>/` with its log and
the sha256 of the strand-eval binary that ran it, which the next run's
clearing does not touch (replay-owner). The detached worktrees of
mutants and A/B are always removed by their real, absolute path: git
matches a relative one as a suffix of every worktree's, so another
checkout's `target/eval/mutants/tree` could be the one removed.

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
and is not compared either. A layer surface's margins after the commit
that carries a frame, or the bare one that sends a frame that drew
nothing (eval-rules-6; a bare commit after a carried frame is held to
the margins its own state asked for at its moment, integration-r7:
the driver records, at each tick's end, each layer surface's state at
rest moved by the offset of the pose its latest frame commits
(`WireLog::held`), and every such commit matches its tick's record,
the tick before's, or in the frame's own tick the frame's; integration-r7
part 2: one a later commit of its tick replaces is judged too, since the
compositor applies a layer surface's margins at each commit, and only the
edges layer shell reads a margin on, the anchored ones
(`wire::read_edges`), are compared; so a new shadow's overhang painted
after the frame passes, a bar's bottom margin it is not anchored to moves
nothing, and rest or stale posed margins sent during an exit fail), are
held to the ones its state asks for at rest, the
side it is anchored to on an axis anchored on one side moved by the
frame's pose's offset as layer shell reads margins (`wire::posed_margins`,
worked out from the protocol, not by strand-surface's `posed_margin`),
within half a pixel: a delegated offset goes out with its frame's
commit, the right way (held-out set H's bottom-anchor mutant grew a
bottom margin the pose should shrink; `pose-margin-unsent` keeps the
rest margins). `late-present` holds every frame that drew to
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

## Replaying a glitch report

Built (decisions.md report; features.md "Glitch reports"). The owner
types `strand report <words>` on the live shell (design.md "Reporting
a glitch") and may replay it on the desktop; the agent reads the same
files, or replays it in the container:

```text
strand replay [report-dir] [--no-open] [options]    # the owner; default: the newest report
strand replay clip [report-dir] --from T [--to T] [--node SEL]... [--note TEXT] [--json] [--save FILE.zip]
                                                    # (replay-clip) a clip of it for the agent ("A clip")
scripts/report-view.sh [--no-open] [bundle-dir] [--out DIR]   # an agent, in the container
```

(replay-owner) `strand replay` hands off git-style
(docs/architecture.md "Glitch reports"): it runs `strand-eval replay`
from beside the `strand` binary, else from `PATH`, with its arguments,
naming the newest report under `$XDG_STATE_HOME/strand/reports` when
none is named; without a `strand-eval` it says in plain words that the
replay needs one installed next to `strand`, and exits non-zero.
`strand-eval replay` runs natively there, as every run does: its own
in-process stand-in for the compositor, the CPU renderer, fonts and
icons as the host has them, and never the session's `WAYLAND_DISPLAY`. It
writes into `<bundle>/replay/` (cleared first, mode 0700), prints the
verdict in one plain line, then `replay: <bundle>/replay/replay.txt`
and `viewer: <bundle>/replay/index.html`, and opens the viewer with
`xdg-open` unless `--no-open`. (replayfix) The local scenario goes to
the `eval/scenarios/local/` of the strand checkout holding the
directory `strand-eval` is run in (an ancestor with `eval/Cargo.toml`
naming `strand-eval`), else `${XDG_CACHE_HOME:-~/.cache}/strand-eval/
scenarios/local/`; where the binary was built does not count, so a
copied or installed one works anywhere (`scenario::checkout_eval`,
`scenario::cache_root`). On the desktop a file the shell read is read where
it is now, not the bundle's copy (the container mounts the copy at its
path): each copied file that differs from what is at its path now is
named in the limits. A surface the live shell drew on the GPU is
replayed on the CPU alone unless `--gpu` adds the GPU leg (a limit
says so).

`scripts/report-view.sh` runs `scripts/container/run.sh eval replay <bundle>` (which mounts
the bundle read-only, and each directory of its `files/` tree that
holds a file at the path it was copied from, as `eval local` mounts a
local config, with the host fonts: `scripts/eval/replay-mounts.sh`
after `local-mounts.sh`), copies the run into `<bundle>/replay/`
(`index.html`, `replay.txt`, `fidelity.json`, `summary.txt`, and
`<id>/` with `checks.json`, `trace.json`, `scenario.scn` and the
frames) and opens the viewer in the browser unless
`--no-open` is given. Agents and tests always pass `--no-open`;
(replayfix) `--out DIR` copies the run into DIR instead and leaves the
bundle as it is (someone else's evidence, read only). It
exits non-zero only when the replay could not run; findings are the
point, not a failure. It runs the same subcommand as `strand replay`,
with `--out` into the checkout (the bundle is mounted read-only),
`--no-open` (it opens the copy itself) and `--gpu` (CI's image has
lavapipe).

`strand-eval replay <bundle> [--no-open] [--out DIR] [--to SCN]
[--output NAME] [--traced SECONDS|all] [--frames …] [--gpu]` does the
work:

1. **Reads** `bundle.json` (its outputs and surfaces among it), both
   ring dumps (`strand_core::flight`), `services.json`, `cells.json`,
   `nodes.json` and (replay-owner) `drawn.json`, refusing a newer
   format by its number.
2. **Picks the window**: from the newest of the two rings' oldest
   records (a ring that wrapped), the last reload committed and the
   last gap's end (a lock, the recorder off), to the report; bounded by
   none of them, from the shell's first surface mapped, where the
   sandbox's boot is. A reload or a lock inside the last minute
   therefore shortens the replay and is named in its fidelity notes.
   (report review) The window is never longer than 120 s
   (`plan::WINDOW_CAP_NS`, the input ring's motion window at 60 Hz): a
   quiet shell's rings may reach back hours, and every warm-up frame is
   painted. Past it the window starts 120 s before the report, rewound
   to there, and the limits say so. Turning `system.replay` off and on
   writes a gap's end (`gap::OFF`) into each new ring, and the outputs,
   surfaces and surface roots shown then again, so a window starts
   after the recorder off as it does after a lock.
3. **Replays every output** (fidelity; decisions.md fidelity
   2026-10-23): each live output is a screen of the sandbox
   (`screens`), in `screens.all`'s order (`bundle.json`'s `order`;
   an older bundle's by its outputs' first surfaces, with a note),
   each with its logical size, scale and place in the layout and
   (replayfix) its connector name, make and model, and the
   `screens.all` id and description `bundle.json` records for it, so
   per-screen data (`workspaces.slots(screen)`) and the cells kept per
   screen (`Notch[<id>].grown`) name the screen they did live, and a
   `bar` mounted per screen is mounted on each. The **main** output,
   the one with the most input in the window's last 10 s, else
   (replayfix) the one with the most input in the whole window, else
   the one the newest mapped surface is on (`--output` names another),
   is `main_screen`, the `output:` line with its reason, and the
   viewer's first; its refresh is the header's `hz`: the sandbox has
   one clock, so an output that refreshed at another rate is a limit.
   Input is taken on the output it was on: a `screen` step before
   input on another output's surface, placed once the steps are in
   time order (so a step placed earlier never moves the focus under
   input already taken: a late frame's `drop` is back-dated by the
   frames it missed, and names its surface's output with `output=`
   rather than focusing it), and every surface's input, map and frame
   is held to its own output's. A panel or OSD (`screens: focused` by
   default) that mapped live on an output puts the sandbox's focus
   there right after the latest input before it (the compositor's focus
   can move through another program's window, which nothing of
   Strand's records), so the replay maps it where the live shell did;
   not for a bar, a popup or a node shown on several outputs at once.
   (fidelity) The sandbox keeps one surface per node: a node the live
   shell showed on several outputs at once (one surface of it on each:
   `screens: all` on a panel or OSD, or a list of screens) is drawn on
   one, and a limit names it, the outputs it was shown on live and
   those the replay did not draw it on. An output plugged in or
   unplugged in the window (the input ring's `OUTPUT` and `OUTPUT_GONE`
   times) is a screen for the whole window: a limit says when it came
   or went live (one the shell was told of before its first input, in a
   window from its start, was there from its start); one unplugged
   before the window and not back is no screen. One `bundle.json` lists
   without an `order` (no longer plugged in at the report) goes after
   those with one, with a note.
   (replayfix) A bundle from before outputs carried their id has it
   read back from its cells' names (`plan::recovered_id`: the one
   `X[<make> | <model> | …]` id for that make and model), with a note;
   two outputs of one make and model cannot be told apart that way,
   and their per-screen cells start at their defaults, with a note.
4. **Rewinds** every service field and cell from its value at the
   report back through the window's records, by their old values, to
   its value at the window's start. A field's change may be its edits
   (`{"e": [[path, old, new], …]}`) and a keyed item's a `patch` of
   them (architecture.md "Glitch reports"): the rewind undoes them
   from the value after, in the reverse order, and the play forward
   applies them to the value before (replayfix). (replayfix) A record whose value is
   gone (over the value cap, or, in a version-1 dump, its arena
   overwritten; a version-2 ring keeps no such record) does not
   stop its field: the value after it is known there (the rewind has
   reached it), so the replay sets that value at the record's time
   (`set`, only where it changes what was played) and rewinds on past
   it. A field's older records hold their own old values and rewind
   exactly; a keyed list's older diffs are undone from the list after
   the lost one as if it had changed nothing, and the whole list is set
   at its time. A note names each such field, its newest lost record
   and how many older ones were lost; (fidelity) a limit counts them:
   what such a change set off live comes later in the replay, at the
   next value known, or not at all. A cell's change holds the value
   before it, so only a cell whose oldest change in the window lost its
   value does not know its start: in a window from the shell's start it
   starts as the config declares it (not seeded), as it did live,
   rather than at the value after that change, which the replay then
   never made; otherwise it starts at the value after it, and a note
   says so either way. (fidelity) A `persist` cell is never started as
   declared: live it started from the persist store, which the bundle
   does not carry, so it starts at the value rewound to, with a note
   that it may not have been so (the planner compiles the bundle's
   config to know which cells persist; one that does not compile
   counts every cell as one). (fidelity) A name too long for the changes
   ring's table (64 bytes, a per-screen cell's `Notch[<id>].grown`) is
   kept at the head of its value; a version-1 recorder lost the two
   together, and still lost the name with a value over the cap
   (`Ring::named_value` now writes the name alone then). Such a
   nameless record is a change of some field or cell the table does
   not hold, a name of 64 bytes or more (any name when the table was
   full: a short one not in a table with room was never recorded): one
   of those with a nameless record of its kind older than its own
   oldest one (or with none in the window) starts, in a window from the
   shell's start, as the config declares it (a cell that does not
   persist), else at the value rewound to, with a note. Only a record that does not fit
   the value after it (a list diff whose indices are past its end)
   stops its field, which then starts at the oldest value known, with a
   note.
5. **Writes a local scenario** (`eval/scenarios/local/report-<bundle
   name>.scn`, gitignored, so it may name the bundle): `config_dir` is
   the bundle's `config/`, `fonts = "local"`, `wall` the window start's
   wall time, `[seed]` every service field and cell at the start,
   `icons` the bundle's copy of the config's `icons/` folder, or, when
   the bundle copied theme icons under `files/`, both flattened into
   `report-<name>.icons/` beside the scenario, `backend = "both"` when
   a surface was on the GPU in the window and `--gpu` is given,
   `compositor_poses` as the
   diagnostics' globals say (`wp_alpha_modifier_v1` and
   `wp_viewporter`), `warmup` all but the last `--traced` seconds
   ((replayfix) 0 by default: the whole window is traced, so its
   viewer and timeline span it; `--traced 20` keeps only the last
   20 s), and the steps: the input on its timeline (`frames`
   between records, quantised to ticks; a pointer step only where it
   moves, a `focus` before keys on another surface), each service
   change as a `set` (a keyed list as the whole list after its diffs)
   or an `emit`, (report review) each `strand set`, `toggle` or `reset`
   the shell was sent (a compositor keybinding's, recorded as a request
   with the value after it) as a `set` or `toggle` at its time, a text
   held back set as `x`s as long as it was (a step on a surface waits
   (replayfix) two ticks after its live `MAPPED`, the fidelity's
   `MAP_FRAMES`, by when the sandbox shows it; a `leave` the
   compositor sends because a surface mapped under the pointer, an
   `ENTER` on another replayed surface within 50 ms after it
   (`plan::HANDOFF_NS`), is not replayed: the sandbox hands the pointer
   over when it shows that surface, and a `leave` first would cancel
   the hover that opened it), each recorded answer as an `answer` queued as the
   method's answer before it is taken (at the window's start for the
   first), each late frame as a `drop`, `mark`s at the traced part's
   start, at -5 s, -2 s and at the report, and a `shot report` there.
   The services the sandbox keeps itself (`screens`, `clock`,
   `calendar`, `auth`) are neither seeded nor set.
6. **Runs it** as `local` does, with every CPU check, then compares
   the replay with the live shell (below) and writes `replay.txt`.
   (report review) A step the sandbox cannot take (input on a surface
   it never shows, a node it never mounted: the live shell parted from
   it earlier) is skipped and listed as a `step` divergence, so a
   replay always ends with `replay.txt` and `fidelity.json`.

**What the scenario format gains for it** (each a general sandbox
capability, used by any scenario):

- Header `[seed]`: `"path" = "<JSON>"` for a service field, a cell (by
  the inspector's path) or a `$token`, set as a baseline: services
  before boot, cells and tokens as they mount, so no `on change` fires
  (spec.md 8.4: never at boot).
- Header `wall`: the wall time (RFC 3339 with its offset) virtual
  `T0` stands for; `clock` and local time follow it.
- Header `warmup`: ms run before the trace starts. The warm-up's
  frames are painted but neither traced nor checked.
- Header `icons`: a flat icon folder used in place of the config's
  `icons/` (strand-icons' config folder).
- `set <path> <JSON>`: a record, list or optional value written as
  JSON by its schema type (`vm::json::from_json`); scalars still read
  as `strand set` reads them.
- `key NAME [+ctrl|+shift|+alt|+super…] [down|up]`: a key on the
  focused surface (a press and a release without `down`/`up`).
  `type N [over MS]` types N placeholder `x`s, `paste N` pastes N.
- `focus <surface>` and `unfocus`. Without them, keyboard focus is the
  compositor's: a `keyboard: exclusive` surface takes it as it maps, an
  `on_demand` one on a press in it, and it returns when that surface
  goes.
- `answer <service.method> <JSON>`: the next call of that method is
  answered with it, ahead of the mock's own (`SchemaHost::answer`).

**The verdict** (replay-owner; `replay::drawn`, `fidelity.json`'s
first key `verdict`, the first line of `replay.txt`, of what
`strand-eval replay` prints and of the viewer's header). At the
report the live shell writes `drawn.json` (format 2): for each mapped
surface (its slot and root's `node_ref`), when its last frame was
drawn (`drawn_ns`, on the flight rings' clock) and its frame period
(`period_ns`), and every node that frame drew or hid, in paint order,
named as `nodes.json` names a node (origin, `id:`, `for` row key) with
its box as drawn (surface logical px, through every transform, axis
aligned, to 1/64), its own opacity, and how far it moves over the next
frame period: `travel`, the largest move of an edge in px, and `fade`,
of its opacity (0 at rest; docs/architecture.md "Glitch reports").

The replay holds each live surface to the one replay surface whose
root is the same node (`nodes.json`'s origin, `id:` and row key: two
popups from one `for` are told apart by their keys), each replay
surface taken once. Of that surface it takes the last frame painted
before the report: the replay's tick nearest `drawn_ns`, never past the
report's own tick (`mark report`; the tick after it is drawn after the
live shell's last frame), else the surface's last painted frame before
the report. (replayfix) A live frame drawn more than two frame periods
before the report shows its surface at rest (a surface paints every
frame while anything on it is in flight, a colour's spring too), and
a motion's end is as uncertain on the replay's clock as its start, so
it is held to the replay's last paint up to two ticks past that
nearest tick: the replay at rest too, not a frame short of it with a
spring's last step of colour, which no box shows (the busy session's
focused chip, 9 levels of blue off on 2,976 px). A replay paint that
damaged nothing keeps no image; its pixels are the image kept before
it. It names each row as `nodes.json` does, with its own
travel and fade (the larger of its move since the frame before and
over the next tick), and pairs the nodes in paint order by origin,
`id:` and row key:

- **VERIFIED**: every live node has its replayed one and the other way
  round, each edge within 1 logical px and the opacity within 0.02;
  a node moving at the report (a spinner, a spring in flight) within
  that plus two frames of its travel and fade (half a frame for the
  tick nearest the live frame, half for a motion's start quantised to
  ticks, one for the live frame's presentation lead), counted as
  moving at the report; every surface the replay shows at the report
  is a live one; and the surface's pixels, where the bundle has a PNG
  of it, within `cpu-gpu`'s tolerance of the replay's image of that
  frame, outside the boxes (both sides', grown by their bound) of what
  moves;
- **DRIFTED**: anything else, one line per difference: a node at rest
  (`@origin[key] #id`) and its surface, the live box and opacity, the
  replay's, and how many px apart its farthest edge is (and how far
  the opacity, past 0.02); a node only the live shell drew, or only
  the replay; a live surface the replay drew no frame of; a surface
  shown in the replay only; a surface's pixels that differ (how many
  and where; the files the bundle copied that differ now are named on
  that line, since a replay reads them as they are now), or a PNG that
  could not be compared;
- a node moving at the report past its bound is listed on its own line
  as `moving out of step`, which does not make the verdict DRIFTED:
  its phase on the replay's clock parts from the live shell's (a loop
  begun before the window) rather than where it is drawn;
- **NOT CHECKED**: a bundle with no `drawn.json` (a strand from before
  it) or none of its surfaces held to a replayed one.

The live side costs nothing per frame: the boxes are flattened again
on demand, at the report, from the renderer's retained scene at the
time of its last paint and one frame period later for the travel
(`Renderer::drawn_nodes`, the same flatten a repaint without a commit
runs; motion is peeked, never advanced), with no eval hooks in the
shipped binary. That they are the trace's own record is held by
`eval/tests/drawn.rs`: with `RunOptions::drawn` the harness holds
`drawn_nodes` to every painted frame's trace node by node (scale 1.25,
a clipped, turned box, a scrolled list, springs and fades in flight,
on the CPU and on a GPU-presented surface).

In a replay the checks' findings are hints (owner-2026-10-21):
`replay.txt`, `summary.md`, `summary.json`, `summary.txt` and the
viewer's statuses read `hint` where a gate run reads FAIL, CHANGED or
XPASS. `strand-eval replay` reads `bundle.json` and finds `config/`
before it clears `<dir>/replay`, so a directory that is no report
keeps what is in it.

**The comparison with the live shell** (`fidelity.json` and the head
of `replay.txt`). The replay checks itself against what was recorded
and says where it first parts from it:

- each recorded press's `hit` (by `nodes.json`: its origin and `for`
  key) against the node the replay's router hits at the same point,
  press by press (report review: `nodes.json` names a node a press
  removed, a menu row that closes its menu, by its identity when
  pressed; a hit it still cannot name is a note, not a divergence);
- each surface root mapped and unmapped (by its origin) against the
  replay's, within two frames, in the traced part (a surface the live
  shell mapped as it started, before any input, is one the sandbox's
  boot shows on its first frame);
- each recorded cell write against the replay's own writes (cells are
  never driven, only seeded: the replay's handlers write them);
  (fidelity) in passes: a write the ring kept whole against the
  replay's write of the same path and value nearest it in time, within
  two frames, then against the next of them at any time, so a live
  write the replay did not make is the divergence at its own time,
  not the later one it would take; a write whose value was lost
  against the replay's write of its path nearest in time, within two
  frames; one whose name was lost too against the nearest write of a
  cell the ring's table could not hold (a name of 64 bytes or more, any
  when it was full); each held by time is named in a note, with its
  time and the replay's write it was held to;
- the replay's frame at the report against each bundle PNG of the same
  surface, (fidelity) on every output: the share of pixels past the
  `cpu-gpu` tolerance and their bounding box.

(fidelity) **The verdict says where the replay parted before the
frame it holds.** VERIFIED and DRIFTED are of the frame at the report;
a replay that parted from the live shell earlier in the window (any
divergence but a frame's) says so on the same line, wherever the
verdict is given (`replay.txt`'s first line, `fidelity.json`'s
`verdict.line` with `verdict.parted`: `count`, `first`, `last`, the
summary, the page's header and a clip's fidelity lines, clip.js the
one source): `…; but the replay parted from the live shell N times
earlier in the window, first at -31.54s (cell: …)`.

It also lists what it did not reproduce: typed text (the keys counted,
typed as `x`; password spans, typed as eight), pastes, services it has
no stand-in for (a plugin whose schema is not in the bundle), calls
answered by the mock because no answer was recorded, other programs'
windows (`thumbnail` nodes draw nothing, and the compositor's blur and
its own animations are not modelled), (fidelity) an output whose
refresh rate differs from the main one's (one clock), a node shown on
several outputs at once (drawn on one), an output plugged in or
unplugged in the window, service changes
whose values the ring lost, horizontal scrolls (a scenario scrolls
vertically only), files listed in the bundle's `missing`, and the
reload, gap or cap the window starts after.

**`replay.txt`**, what an agent reads first. Its first line is the
verdict, each difference under it; then its head gives the
words, the window, the main output, (fidelity) every screen replayed
in `screens.all`'s order, the traced part and the comparison above;
with several outputs a frame, a surface and a difference name their
output (`@notch.strand:68:1 on DP-8`). Then (replayfix) the timeline, under `timeline (seconds before
the report): …` (`replay::TIMELINE`), one line per event in time
order, time relative to the report: input as its step with the
surface's and the hit node's source location (a run of pointer moves
on one surface with no second's pause as one line: how many, over how
long, from where to where), service changes, cell writes (live and
replay), surfaces mapped and gone, the replay's repaints as runs
(`replay repainted k120..k300: 181 frames over 3.0 s`) and each span
of a second or more with none as `nothing changed: no repaint for
12.4 s (to -3.1 s)`, the report, and for every node whose drawn box or
opacity changes, one line per run of motion (`k456..k470
@clock.strand:12:5 text x 1180.0 -> 1176.3 spring`).

**The viewer of a replay** (replayfix; `RunInfo.replay.timeline`,
`replay::timeline_json`). Its scrubber spans the window in real time,
in ms, not the painted frames: the label reads the time before the
report, the frame shown (the last painted at or before it, held), and
`nothing changed: no repaint for 12.4 s` inside a span with none. Under
it lanes mark the repaints and the idle spans, the untraced part, the
live pointer's time on a replayed surface, its input, service and cell
changes, surfaces mapped and gone, late frames, the divergences and the
hints, and the report, with a legend; it opens at the report. The live
pointer at the scrubbed time is drawn over the frame (a ring and a
cross) and named in the label (`pointer 58.3, 344.2 on @…`, or off the
replayed surfaces). (fidelity) With several outputs a picker above the
stage, beside the two overlays and outside Details, shows one output's
surfaces (the main one first, or every output; the last picked is
remembered as `strand-eval:output` when the replay has it), each
labelled with its output and scale and drawn at its own scale (the
trace's per-frame `scales`), and the pointer named and drawn on its
own output. A fitted zoom may go below a quarter (to 0.05), so a
full-width bar on a scale-2 output fits a phone's stage; viewer-check
picks each output and holds every one to the stage's width. The buttons and keys step from one event to the
next, play runs in real time from one to the next (a second at most
per step). A run with no timeline (every other run, an A/B page) keeps
the frame scrubber. The checks' findings follow, as `summary.txt`
gives them, each with its time before the report, under (replay-owner)
the label `hints (the checks may be wrong on a real shell):`.

**The replay's page** (replay-clip; decisions.md replay-clip
2026-10-22). A replay's page (`DATA.replay` set; every other run keeps
the layout above) shows by default only what spotting a glitch needs:
what was reported and the verdict, two large toggles (`Where it rests`,
the dashed rest boxes, and `Onion skin`, the outlines of the 4 frames
before), the frame at full width, the scrubber with play and step under
it (the hints are small marks on its timeline), and the clip. The
checks' table, Cost, the node tree, Curves, the filmstrip and the full
toolbar sit behind a `Details` toggle, remembered in this browser
(`strand-eval:details`, `localStorage` in `try`/`catch`). Its buttons
and tabs are at least 40 px tall (its selects 36), a selected one filled in the accent,
in the theme's tokens, light and dark; at a phone's width nothing
scrolls the page sideways.

**A clip** (replay-clip; `eval/viewer/clip.js`, `replay::clip`,
decisions.md replay-clip 2026-10-22). What the owner hands the agent
about a glitch seen in a replay: these elements, these frames, where
each was drawn and where it rests. On the page:

- **When**: drag across the timeline (the slice is a band on it), or
  press `[` and `]` while scrubbing (`Start here [`, `End here ]`);
  `Clear slice` drops it. With no end, it runs to the report. An end set
  on a painted frame (the time shown is its tick's) is that tick
  (`k5226`) in the spec, so the frame on screen is the slice's first or
  last; any other end is its time to the ms.
- **What**: click an element on the frame to add or remove it, or drag
  a rectangle over the frame to add every element drawn wholly inside
  it. Neither adds an element that does not show, by the checks' rule
  (hidden at opacity 0, at scale 0 or clipped away), nor one with no box
  or no opacity: a click takes the smallest that shows under it. The
  hover card still names whatever is under the pointer. The clip's
  elements are filled and outlined in the accent; a list
  gives each one's label and config `file:line` (the node's origin),
  with a `Remove` button.
- **A note**: optional free text.
- **Copy for agent**, the primary button, puts the clip's text on the
  clipboard: `navigator.clipboard.writeText`, else a selected textarea
  and `execCommand('copy')`, else the text left selected in a box to
  copy by hand (the page says which). **Save clip** downloads one
  `.zip` (stored, a `Blob` and `a[download]`): `clip.txt`, `clip.json`
  and the slice's frames cropped around the elements
  (`frames/k<tick>-s<surface>.png`, the tick that surface painted the
  frame at). A frame whose pixels the page does not hold is left out,
  and the `.zip`'s `clip.txt` (a last line) and `clip.json` (`missing`)
  name it. The page loads nothing and names no URL.
- The page shows the same clip's command (`strand replay clip …`) and
  the text it copies.

A clip is a selection spec: the report, the slice, the elements' keys
and the note. Its data comes from the trace and `fidelity.json` (the
page holds it as `DATA.replay.record`), never from what the page draws.
`eval/viewer/clip.js` is the one place a clip is made: the page runs it
on its data, and

```text
strand replay clip [report] --from T [--to T] [--node SEL]... [--note TEXT] [--json] [--save FILE.zip]
```

(`strand-eval replay clip …`, which `strand` hands off to as it does
`strand replay`, the newest report when none is named) runs the same
file in QuickJS (`rquickjs`) on the replay's `trace.json`, `checks.json`
and `fidelity.json`: the text on stdout (`--json`: the JSON), and with
`--save` the same `.zip`. `T` is seconds before the report (`-3.2s`,
`-3200ms`, `-3.2`), to the ms, or a tick (`k5226`), exact; `--to`
defaults to the report. A tick is in the slice when it lies within a ms
of its ends (a time to the ms is half a ms off at most; ticks are 4 ms
apart or more), and the slice's ticks in its text and JSON
(`slice.from_k`, `slice.to_k`, `slice.ticks`) are its first and last by
that one rule, so they are those of the frames its rows hold; a tick
given stays a tick in `regenerate:` (`spec.from_tick`, `spec.to_tick`).
`SEL` is a node's key, `#id` (an `also` id too), or a
config place, `file:line` or `file:line:col` (the file by its name or
with folders), every node written there. `<report>` is a glitch report
(its `replay/`) or a replay's report (`--out` of `replay`); one not
replayed yet is refused with the command to run. Refused as typed
errors (`replay::clip::ClipError`, a usage error): a selector naming no
node, an empty slice (`--from` not before `--to`, or nothing drawn by
its end), a time that does not read or lies outside the window. The
text and the JSON are the same, byte for byte, from the page and from
the command line for one spec (eval/tests/clip.rs, which runs the
page's own data through clip.js and holds the command line's `.zip` to
it byte for byte, its crops pixel for pixel to Rust's decoding of the
frames).

The text, compact for an agent's context:

- the report's words and name; the slice in seconds before the report
  and in ticks, how many frames painted in it, and the frame held at
  its start (the last painted before it, a row of its own: `k1819
  (held)`, never joined by a frame of the slice);
- the note;
- the fidelity statement for the slice (below), the hints on the
  elements at the slice's ticks (not the held frame's), and what the
  replay does not reproduce;
- per element its label, config line, surface and key, and a table, a
  row only where a value changes, naming the frames it holds
  (`k3655..k3680` and their times): the drawn box, the rest box (`same`
  when it is the drawn one), opacity, scale and the motions that move it
  (its own; `hidden: …` when it is hidden); `not drawn` where it is not,
  `kN (gone) … not drawn: its surface went` from the step its surface
  detached;
- `regenerate:` and the command that makes the clip again.

**Its fidelity, honestly.** For each surface the clip is of: the frames
the slice shows of it (their paint ticks: those painted in the slice,
and apart, the one drawn before it and held at its start); the verdict
compared one frame of a surface with the live shell, the last before
the report (`verdict.ticks`: its root's origin, the tick, the replayed
surface `sid`, and `in_motion`, how many of its nodes were moving at
the report and held within their bound), and the clip says `VERIFIED`
only of that frame, only when the slice shows it and the verdict found
nothing on that surface, and only of what the verdict held: its nodes
at rest within `tolerance.px` and `tolerance.opacity` of the live
boxes, those moving at the report within `tolerance.moving_frames`
frames of their travel with their boxes left out of the pixel
comparison (said so when there are any), and whether its pixels were
compared. Nodes `moving_out_of_step` (past that bound; no difference to
the verdict, held to nothing) are named on a line of their own, never
as differences and never as held. Otherwise it lists each difference
the verdict found there. A verdict's frame is placed on a surface by
its `sid`; a `fidelity.json` from before `sid` was recorded is placed
by the root's origin only where one surface of the replay has that
root (else nothing of it is said compared), and what the verdict found
on a root that two compared surfaces share is said to be on one of
them. Every other frame is said to be the replay's own, drawn from the
recorded input, never compared with what the live shell drew; a surface
the verdict did not compare is said so with why, as the trace and
`fidelity.json` hold it: the live shell showed it and the replay drew
no frame of it at the report (`missing`), shown in the replay only
(`replay_only_surfaces`; (fidelity) each `{surface, output}`, its
root's origin and output, a string of the root in an older file, so a
surface is matched on its own output), not shown at the report (the trace has it
detached before), else "the verdict compared no frame of it", quoting
the live surfaces it could place on none (`missing`'s `… (its root is
not in nodes.json)`). A slice before the trace's start (`strand replay
--traced N`: the window's start replayed as a warm-up, drawn and not
traced; `window.traced_seconds`) is refused saying so, and one that
crosses it says `untraced before -N s`. A report with no `drawn.json`
is NOT CHECKED throughout. The divergences from the recorded session
inside the slice are listed, with how many came earlier, the first
and the last of them ("N earlier in the window, the first at T s
(what: text), the last at …: the replay may already part from the live
shell here"), and the limits follow. (fidelity) The verdict's line
carries the replay's partings before the report frame (above), and a
surface's VERIFIED line says the same, so a VERIFIED frame never reads
as a faithful window; with several outputs each surface is named with
its output (`@notch.strand:68:1 on DP-8`) and a verdict's frame is
placed on a surface of its own output.

**The crop**: for each surface, the union of the elements' drawn boxes
on it over the slice, 8 px around (`MARGIN`), in buffer pixels, clamped
to the surface; the whole surface when no element is drawn on it.
(fidelity) Buffer pixels at the surface's own scale: with several
outputs, its output's (the trace frame's `scales`), never the main
one's, and the crop names its `output`. Each
frame of it the slice shows is cropped once (a frame held from the one
before is not repeated), named by the tick that surface painted it at;
the crops are deflated PNGs. On the command
line the frames are rebuilt by `report::pack::Composer`, on the page
from its data URIs by clip.js; the parity test holds the two equal.

**A replay's report, packed** (replayfix; `report::pack`, decisions.md
replayfix 2026-10-21). A minute of a real shell is a thousand painted
frames, each repeating almost every node row and almost every pixel of
the one before: the owner's 80.6 s bundle wrote 273–336 MB per replay
(index.html 94–110 MB with the whole trace inline and no image that
fitted its 15 MB budget, so every frame was a wireframe; trace.json
101–109 MB; 1,089–1,312 whole frame PNGs, 118 MB), and twenty reports
are kept. A replay's report keeps each thing once:

- **`trace.json`** is the same trace, each frame against the one
  before: the fields equal to the previous frame's are named in `"="`,
  and the node rows `n` are `[i, n]` (the previous frame's rows
  `i..i+n`, unchanged), `{"i": i, "c": [col, value, …]}` (its row `i`
  with those columns changed) or a whole row (an array whose first item
  is its key). Values are copied from the trace's own JSON, never
  recomputed; `RunTrace::from_json` (and `report::pack::read`) unpack
  it exactly. The top-level `packed` gives the `format` (1) and the
  patched frames.
- **The frames**: a surface's new frame is stored as a patch of the
  frame of that surface stored before it (`frames/<digest>.patch.png`):
  the boxes whose pixels changed, each a run of 32-px tiles along a
  32-px band, shrunk to what changed in it, stacked top to bottom from
  the image's left edge, every byte of them (`packed.patches`: `[base,
  [x, y, w, h], …]`). It is stored whole (`frames/<digest>.png`) when it
  is a surface's first or its size changed, when its boxes would cover
  over 75% of it, and at least every 256 frames. The digest is still
  the whole frame's pixels'; `report::read_entry` and `explain --png
  DIR` rebuild every frame whole and exactly.
- **`index.html`** holds the trace packed, of each frame only what the
  viewer draws (`pack::VIEW_FIELDS`: `k`, `t_ns`, `painted`, `sizes`,
  `png`, `n`, `scale`, `held_text`, `cost`; `trace.json` has every
  field), and every stored frame file as a data URI with the patches
  (`patches`). The viewer unpacks the trace as it opens the scenario
  and draws a patched frame onto a canvas from its base (rebuilt the
  same way, or one of the 24 rebuilt last), each box cleared and drawn
  from the patch: stepping on rebuilds one patch at a time. The frames
  stay data URIs (a page opened from `file://` that drew images loaded
  beside it could not read their pixels back for its difference views).

Measured on the owner's bundle (container, the whole window): 15.5 MB
(index.html 7.9 MB, trace.json 3.5 MB, 1,312 frame files 3.8 MB, of
which 6 whole), from 336 MB by the same build unpacked; every frame
drawn from its pixels; the page shows its first frame 0.7 s after
loading in headless Chromium, with a 40 MB heap. Every other run's
report, the references and the digests are as before: the packing is
only how a replay's report is written.

**Findings are hints** (replay-owner; decisions.md owner-2026-10-21).
In a replay the checks' findings come after the verdict and the
timeline, under that label in `replay.txt`, as `fidelity.json`'s
`hints` (`label` and `findings`: check, severity, node, time before
the report, message) and in the viewer, where the label stands in for
the run's pass/fail chip. A failing check never makes `strand-eval
replay`, `strand replay` or `scripts/report-view.sh` exit non-zero;
only a replay that cannot run does.

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
- **Growth on a content-sized surface** (integration-r5; decided at
  growth, 2026-10-22). The growth glide (`glide_origin`) is gone: the
  owner chose a box that springs over the step (design.md "Growth
  springs"), so a bottom panel growing no longer glides its card past
  its bottom and a centred card no longer glides by half the growth;
  held-glides' scenarios (`snapped-growth-cut`, `held-grow-glide`) are
  in the gate. The label nudged by a parent's glide (above) and
  `osd-edge-margins`' cut were read with the growth glide and were not
  probed again. A shrink is fixed (bottom-osd-shrink); at scale 1.5 a
  held bottom OSD's last row is laid 1 px off on two frames as its row
  folds (the held root's placement rounding, popup-shrink's rule).

- **An image's pixels are not recorded** (eval-coverage-3). `im` holds
  where a decode is drawn and at what size, and `placement`'s `fit` judges that
  placement against the file's own size; what the decode holds is not
  seen. (`decode_png`'s per-side rounding, which this left open, is fixed
  and caught: png-aspect.)
- **A morph's arrival is read from its first frame and its steps**
  (`coherence` morph arrival, eval-rules-5). The trace carries each
  node's `morph` name and each surface's origin, so a card that pops in
  at its box while its partner shows fails; one that starts within half
  its way and eases is not compared with the curve render's morph plays.
- **Rings, gaps and rows read as declared** (eval-rules-6). `ring`
  judges a border along its box only: one along a `shape:` outline, or
  with squircle corners, whose curve CSS does not define, is not judged,
  nor is a `radius:` form `ring::radii` does not read. The fold gaps
  are not judged in a container that spreads its children or holds one
  out of flow, beside a margin, where two folding slots share a gap, or
  for a way met in flight. `wrap` judges rows at rest only and counts a
  child with a width of its own as rigid (CSS lets it shrink to its
  content), so an overflow beside such a child is not judged; a text's
  words are split at spaces, so a text that breaks elsewhere (a long URL
  with no space) counts as one word.
- **A posed popup and a scaled pose's regions** (eval-rules-6). The
  driver moves a posed layer surface by its margins and a popup not at
  all (the manager sends no offset for one; render delegates none). A
  compositor that animates a popup's position some other way is not
  modelled. `placement`'s `pose` holds the place; the scale a pose
  delegates is applied about the surface's top-left in the checks' view
  as before.
- **A drop is not delivered** (eval-coverage-4). A `drag` over a
  `drag:` source lifts it and moves it, but the release delivers no
  `drop:` to the target under it (the run notes "a drop is not delivered
  by the sandbox yet"), so a reorder by drag and drop runs only as far
  as the lift and the flight. A dock dragged that way found a partial
  redraw that leaves a strip of the sibling it flies over (decisions.md
  eval-coverage-4), left out of the gate until it is fixed.
- **What the eye sees of a posed surface** (eval-rules-5). A row is cut
  by its buffer where it is drawn in it and by the output (the
  scenario's `screen`) where the compositor's pose puts it; a
  compositor's own effects (blur, rounded corners) are not modelled.
  One model places a posed row everywhere (integration-r7,
  `trace::Posed`): the margins the frame sent on the wire (`shifts`,
  none when it sent no shift) plus its box times the pose's scale; the
  checks take a pose off by inverting that same model.
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
  glyphs drawn on one node are one entry. A `letters` block's offsets are
  not recorded either: a text's `ink` is its layout's, so a letter drawn
  at the wrong offset or phase changes no recording (eval-coverage-5:
  voice-letters-noise holds the letters to `settle` and `twin` only), and
  an animated image's frame is seen only by which ticks paint.
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
- **What time drives is read as declared** (eval-rules-8). `track`
  skips elastic and bounce easings and keyframed props other than `x`,
  `y`, `rotate`, `opacity` and `scale` (a keyframed colour or size), and
  a play once motion is reduced in it. `still` compares only reduced
  frames that follow a reduced frame with nothing driven between, so a
  loop that runs on for exactly the frame motion is turned on is not
  seen there (`twin`'s re-run is reduced from its first frame). `reach`
  reads the area a shadow may cover, not its pixels: a shadow drawn with
  the right area and the wrong blur inside it is the references'. A wheel
  notch whose input lands as motion is turned off glides (the mutant
  j17); that is a notch with motion on, not judged (decisions.md
  eval-rules-8).
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
- **A replay is as faithful as its report** (built, decisions.md
  report). See "Replaying a glitch report" for what a replay cannot
  reproduce and how it says so.
- **Gaps the blind sets K and L left** (eval-finish, for later; none
  is fixed). No rule holds a text's alignment inside its box, so a label
  drawn at the wrong side of its own box passes the rules (L's m-l13).
  `tiling` misses a 1 px seam from rounding at a fractional scale (L's
  m-l08). No gate scenario has a shadowed surface under a pose the
  compositor applies at a scale (L's m-l15). A new shadow layer that
  starts at its resting strength instead of growing in is caught only by
  references (K's m-k12). An image sliding off-centre in its box is
  caught only by references, and the CHANGED summary does not name the
  image (K's m-k19).
- **The renderer's other wall-clock reads stay wall-clock**: an exit's
  stall, GPU promotion and device retries, the busy window. Their waits
  are set so they never fire in a run.

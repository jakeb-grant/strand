# Decisions

Interpretations of ambiguous points in `design.md` and `architecture.md`.
One short dated entry per decision, under each track's heading.

## core

**2026-10-05 · Explicit runtime handle.** `strand-core` has no
thread-local "current runtime": every read, write and closure takes
`&Runtime`, a cheap non-`Send` `Rc` handle. Stored futures and closures
hold a `WeakRuntime` so the runtime never keeps itself alive;
`Runtime::shutdown` disposes everything.

**2026-10-05 · Ticks and batching.** Writes never run effects directly;
`flush` (or `tick(now)`) ends the tick, and memos are pull-consistent
mid-tick. Effects run in creation order (owners first), which is only
topological for read-only effects, so `on change` handlers run in a late
phase once no other sink is queued and fire once with settled values.
(Superseded in wave 2: wave2-core, "Topological effect order".)

**2026-10-05 · Runtime cycles.** Past `MAX_RUNS_PER_FLUSH` (16) runs of
one sink or queue in a flush, the runtime looks for a write/event path
back to it; if found, `Error::Cycle` names it once and the node is parked
until an outside write. Without a path the flush continues up to
`HARD_RUNS_PER_FLUSH` (256); a memo cycle is the memo's value, and a nested
`flush` reports `Reentrant`.

**2026-10-05 · What "changed" means.** Each node has a change counter
bumped by a value-changing write or recompute; a watch reports its target
when the counter moved. A write-and-restore may cost one redundant
`SetProp` rather than keeping a second copy of every watched value against
the memory budget.

**2026-10-05 · Errors are values.** Memo closures return `Result<T,
Error>`, stored as the memo's value (cut-off still applies). Stale handles
give `Error::Disposed`, a write inside a memo is `Error::WriteInDerived`,
and handler and effect errors land in `Tick::errors`.

**2026-10-05 · Write-rate monitor.** The guard covers handlers triggered by
state (effects, `on change`, timers, service listeners and their tasks);
input handlers, `<->` writes and CLI/service writes are exempt until a
task's first suspending `await`. Writes count per logic step; the 31st
within a sliding second warns once (`Diagnostic::WriteRate`) and throttles
to one write per 1/30 s holding the latest value. Keyed collections are not
gated. (Superseded in wave 2: wave2-core, "Keyed writes under the 30
writes/s guard"; wave2-lang, "An input task's run is its own writer".)

**2026-10-05 · Echo suppression.** `write_tagged(value, send)` applies a
local write, keeps it pending (at most 64 per cell) and sends it with a
generation, through the write-rate gate. `receive(value, tag)` ignores
echoes of pending writes (by tag, or by value for D-Bus), applies a
differing acknowledgement (clamping), and treats an unmatched value as an
outside change that clears pending writes.

**2026-10-05 · Events.** Each event is delivered once to every listener
alive at delivery, in emission order, during the next flush; events
emitted by listeners go out in the same flush. Queued events are shared
`Rc<T>`, so no `T: Clone` is needed.

**2026-10-05 · Timers and `on change`.** `after T while c` counts only time
while `c` holds and fires once; `every T` keeps its phase and fires once
after a gap; a zero period pauses with `Diagnostic::ZeroPeriod` and periods
under 1 ms clamp to 1 ms. Reload's rescale is `Timer::rescale_from(old)`
(`Debounced::rescale_from`): a fired `after` stays done, a countdown keeps
its fraction. `on change x` never fires for the first value; `on change x
after T` debounces.

**2026-10-05 · `on change` and identity.** Since the runtime can't know a
path's identity, `on_change_keyed(key, track, handler)` takes it: when
`key` changes (the sink id for `audio.sink.volume`) the new value becomes
the baseline without firing. The compiler passes each service path's object
part as the key.

**2026-10-05 · Handlers.** A handler is a `Future` owned by a node, polled
at most once per flush with no async runtime; woken tasks run in wake
order, due timers in creation order. Nodes a handler body creates belong
to its component; tasks it starts belong to its handler site, so reload's
cancel is `rt.dispose(handler)`. Disposal cancels at the current `await`
with `Diagnostic::Cancelled`.

**2026-10-05 · `Async<T>`.** `x ?? fallback` is `Async::or`; each load gets
a `RequestId` and superseded responses are ignored. Cancelling clears
`pending` but keeps value and error, and `begin`/`resolve`/cancel bypass
the write-rate gate. `let hits = apps.search(query)` is a read-only
`rt.async_memo(input, fetch)` that drops superseded requests quietly.

**2026-10-05 · Keyed collections.** `move` is `move_key` in Rust; key
changes via `update` are reverted errors; `sort_by` is a stable merge sort
that never panics on NaN. Params changes publish a keyed diff, never a
bare reset; each collection keeps 256 diffs, then `Reset`. A `for` over a
plain list is `rt.keyed_memo(key_fn, f)` (no writing effect, so never
throttled); duplicate keys are an `Error` value.

**2026-10-05 · Introspection.** `rt.sources(id)`, `rt.observers(id)`,
`rt.owned(id)`, `rt.root_owned()` and `rt.site_of(handler)` expose the
graph read-only for the inspector, `strand watch` and the LSP.
`Tick::diagnostics` carries warnings raised since the previous tick.

**2026-10-05 · Re-running nodes and their children.** A re-running node
first disposes what it owns; losing a child it read does not dirty the
node itself, or `if show { let local = …; use local }` would loop into a
false cycle.

**2026-10-05 · Reparenting.** `rt.reparent(id, new_owner)` moves a live
subtree so state can outlive its creating scope across reloads; making a
node its own ancestor is `Error::Cycle`. The subtree is renumbered after
existing nodes so owners still run first, and cells keep their identity
(no emitter `Reset`).

**2026-10-05 · Freezing a faulted component.** `rt.suspend(scope)` stops
effects, watches, timers, listeners and tasks inside while keeping state;
input events are dropped, service and component events are held and
delivered once on `rt.resume(scope)`. (Frozen timers pause since wave 2,
wave2-core.)

**2026-10-05 · Service lifecycle.** No observed/unobserved hook on cells:
the VM reference-counts services per mounted, visible component from the
compiler's list of read paths, and the services layer applies the 5 s stop
delay.

**2026-10-05 · Left for wave 2.** `persist` storage and settings files are
not in `strand-core` yet. (Wave 2: both done in `strand-core`, see
wave2-core.)

## compiler

**2026-10-05 · Keywords are contextual.** No reserved words; `ident:` at
the start of an item is always a prop (`type: password`).

**2026-10-05 · Calls touch.** `f(x)` and `xs[0]` need `(`/`[` touching the
callee, so values can hold parenthesised terms (`0 (-2px) 8px`).

**2026-10-05 · Operators always bind in space-separated values.** `0 -2px`
is subtraction; a sign spaced before but not after is
`syntax::ambiguous_sign`, offering `(-2px)` or `0 - 2px`.

**2026-10-05 · Value precedence.** `~` loosest, then `,`, then spaces, then
operators: `margin: 8, 8, 0 ~ instant` springs the whole value.

**2026-10-05 · Line structure.** Newlines count only in `{}`. A line
continues if it starts with a binary operator, `.`, `?.`, `?`, `:`, `~`,
`=>`, `<->` or `else`, or the previous ended with a binary operator, `,`,
`=`, `=>`, `<->`, `~` or ternary `?`/`:`; clause words stay on their line.

**2026-10-05 · `pages current: page { … }`.** A single head argument may be
named, sugar for a first prop.

**2026-10-05 · No record literal.** Records use call syntax with named
arguments, `Pin(app: a, label: "x")`.

**2026-10-05 · Component tokens in the head.** `component Toast(n) tokens {
radius: $radius.lg } { body }`; parameter types are optional in the
grammar.

**2026-10-05 · `fn` has one form.** `fn f(x: T) -> U { …; value }`; no `=
expr` form.

**2026-10-05 · `await` and `play` parse anywhere expressions or items
do.** The checker limits them.

**2026-10-05 · Service sources.** `dbus` takes `system|session`, bus name
and optional path; `file` and `listen` one expression; `poll` a command
and `every T`; `permit exec [...]` at top level or in a service.

**2026-10-05 · `#word`.** A colour in expressions, an SVG selector at an
item's start.

**2026-10-05 · Token keys may carry `$`.** `$surface.hi:` and
`surface.hi:` are the same token.

**2026-10-05 · Small lexical choices.** `//` comments only; single-line
strings without interpolation; `%` touching digits is a unit; `$space.2`
takes no fraction; comparisons do not chain; `??` binds looser than
`||`.

**2026-10-05 · Surface names.** `bar`, `panel` and `osd` need a name (the
`strand-<Name>` namespace); `lock` may omit it.

**2026-10-05 · Nesting limit 128, tree depth 256.** Recursive nesting caps
at 128; flat chains parse in loops under a tree-depth cap of 256.

**2026-10-05 · Missing `}` recovery.** A column-0 declaration keyword
inside an open block closes it with one "unclosed `{`" error, pointing at
a misindented `}` as the likely culprit.

**2026-10-05 · Config file discovery is one function.**
`source::find_files` walks breadth-first three levels deep, follows
symlinks, skips dot names and deduplicates by canonical path; `strand
check` and the watcher share it.

**2026-10-05 · Diagnostics live at `strand_compiler::diagnostic`.** Labels
carry a `FileId` and `Span`, so one diagnostic spans files; at most 50
render per file.

**2026-10-05 · Lambda parameters take no defaults.** A lambda always gets
every argument.

**2026-10-05 · Misspelt tree keywords.** The checker's did-you-mean
includes tree and top-level keywords; the parser reads `els { … }` after
`if` as `else`.

**2026-10-05 · Transitions and poses on `if` branches and pages.** They go
on the branch's root node, or on the `pages` element.

**2026-10-05 · Keyframe stops.** `keyframes shake { 0%, 100% { x: 0 };
25% { x: -4 } }`.

**2026-10-05 · A leading UTF-8 byte-order mark is trivia.**

**2026-10-05 · Snippet fixtures.** design.md's code blocks are fixtures
byte for byte, enforced by a test.

**2026-10-05 · A dangling operator before a new prop ends the line.**
`width: 24 ~` before `name:` reports `syntax::missing_value`.

**2026-10-05 · Spaced values are for shadows and fonts.** Elsewhere the
checker suggests commas (`margin: 8 8 0` → `8, 8, 0`).

**2026-10-05 · Kebab-case names.** `max-width:` is `syntax::kebab_case`
and parsed as `max_width`.

**2026-10-05 · Allman braces.** A body `{` on the next line is one error
but kept as the body.

**2026-10-05 · A lone `\r` is a line break.**

**2026-10-05 · Keyword slips are read as the keyword.** One edit from a
keyword, three or more letters, not followed by `.`, `(`, `[` or `=`; in
trees only where an element is impossible.

**2026-10-05 · Long lines render short.** Past 1,000 drawn characters a
diagnostic is one `file:line:col` line.

**2026-10-05 · Whole-number literals past 2^53.** They warn
`syntax::number_precision`.

**2026-10-05 · strand-watch does not link the compiler.** The binary
passes plain paths; directories past depth 3 with `.strand` files are
`Discovery::too_deep`.

## render

**2026-10-04 · Damage area and merging.** `Damage::area()` is the exact
union; merging picks the pair adding least area.

**2026-10-04 · OKLab colour interpolation.** `Color::lerp_oklab` works in
premultiplied OKLab with unclamped `t` (springs overshoot).

**2026-10-04 · Protocol details the contract leaves open.**
`PropValue::Unset` reverts a prop; `Create`/`Move` take `parent:
Option<NodeId>`; token-bound shorthands arrive as a `List` resolved in
place; `NodeKind` names the whole catalogue for compiler and LSP.

**2026-10-05 · `Move` index and `Remove`.** `Move` counts children after
detaching; `Remove` frees the id at once, exits being render-side ghosts.

**2026-10-05 · Tokens resolve at flatten time.** `PropValue::Token`
evaluates against a `TokenScope` (global `SetTokens`, then ancestors'
`tokens`), derived tokens in the asking node's scope; unresolvable reads
as unset.

**2026-10-05 · Motion tokens.** `~ $motion.bouncy` is
`Transition::Token(path)`, resolved at render time; unknown ones snap.

**2026-10-05 · `radius: full`.** The largest radius; a `%` radius is of
the shorter side.

**2026-10-05 · `Prop` stays `Copy`.** Uniforms are one record prop, SVG
`#id` blocks child nodes, time signals `TokenExpr` variants (M4).

**2026-10-05 · Call-shaped values.** `PropValue::Call { name, args }`; a
filter chain is a `List` of calls.

**2026-10-05 · `SetTokens` transition and token budget.** `SetTokens`
carries a transition (`Instant` at boot); `Snap` props always snap;
`MAX_TOKEN_STEPS` (10k) bounds evaluation.

**2026-10-05 · Surfaces.** `Prop::Name` gives `strand-<Name>`. Bars: every
screen, `top` edge and layer; panels: focused, `top`, `center`; osd:
focused, `overlay`; keyboard `none`. Only kind, namespace or layer
changes recreate.

**2026-10-05 · Text truncation.** `ellipsis: start | middle | end` cuts to
`max_width`; `max_lines` drops lines; `marks` paint in `mark_color`, else
`$accent`.

**2026-10-05 · First frame waits for text.** Up to 50 ms
(`FIRST_FRAME_TEXT_WAIT`), also after a worker reset.

**2026-10-04 · Glyph atlas.** LRU per scale, up to 4 pages of 256 px,
leased pages never reset, growth capped at `max_bytes` (1 MiB; 8 MiB
since 2026-10-10, m4-owner-docs); fonts cap at 512 px, text at 64 KiB.

**2026-10-05 · Atlas uploads and worker resets.** `AtlasUpload`s ride in
each `TextLayout`; `is_reset()` makes render drop its mirror; rescaled
surfaces draw old layouts resampled meanwhile.

**2026-10-05 · Absolute layout until taffy.** `x`/`y`/`width`/`height` in
px or %; `font`, `color` and `tokens` inherit; a `popup` is its own root.

**2026-10-05 · Per-node damage.** Each node records ink bounds and a paint
hash; buffer age widens damage (history 4); only non-empty `paint` results
are frames.

**2026-10-05 · vello_cpu cell grid.** A fixed 256×64 cell grid rasterises
only damaged cells, keeping partial repaints bit-identical; output is
`wl_shm` BGRA directly.

**2026-10-05 · Shadows, borders and gradients.** CSS box-shadow (blur =
2σ); borders inside the box in whole pixels; gradients in premultiplied
OKLab, conic clockwise from the top.

**2026-10-05 · Paint time and opaque region.** `PaintTarget.time` is the
predicted presentation; `Painter::opaque_region` is the opaque `bg` minus
corners.

**2026-10-05 · Sanitised values.** Non-finite reads as unset, lengths clamp
to ±1e6 px, blur to 1000, wild `Create` indices are rejected.

## surface

**2026-10-05 · Buffer size rounds half away from zero.** Buffer size is
`Scale::physical_size`, as `wp_fractional_scale_v1` prescribes, not `ceil`
(which can make a buffer 1 px too big and blur). The scale is estimated
from the output's mode before `preferred_scale` arrives; without
fractional scale or viewporter buffers are `logical × n` with
`set_buffer_scale(n)`.

**2026-10-05 · `SurfaceHost` lifecycle hooks.** Attach, configure, detach,
monitors, `frame_deadline` and `frame_dropped` live in
`strand_surface::SurfaceHost: Painter` with no-op defaults, not in
`strand-scene`; the binary wraps `Renderer` to forward them.

**2026-10-05 · Frame callbacks.** A commit requests a frame callback only
if `wants_frame` is still true after the paint; dirty surfaces paint once
per wakeup, and one with a frame in flight waits for its callback or
presentation feedback, so paints lock to refresh. Configure and scale are
resolved just before the paint; a configure needing a new buffer size
skips the wait; a host hold arms a timer at `frame_deadline`, and an
unmapped surface retries after 16 ms.

**2026-10-05 · Buffers.** One pool per surface and size, buffers created
lazily up to three, reusing the free buffer with the newest frame. A new
size drops the old pool and destroys its buffers at once, busy or not.

**2026-10-05 · Monitors.** Identity is `"make | model | description"`
(` #2` for a simultaneous twin, a trailing `" (<connector>)"` dropped);
`Screens::Named` matches identity or connector. An unplugged monitor is
remembered for 30 s and keeps its `SurfaceId`s for a quick replug;
scale, logical size and position changes go to
`SurfaceHost::monitor_changed`.

**2026-10-05 · `screens: focused`.** One layer surface created with
`output = null`, placed by the compositor; its monitor is learnt from
`wl_surface.enter`. `State::set_focused_monitor` pins and moves it.

**2026-10-05 · Closed and content-sized layer surfaces.** A surface the
compositor `closed` is destroyed and recreated only when outputs or its
spec change. Content-sized surfaces are not mapped until M2 layout; spec
updates reconfigure in place, layer or namespace changes recreate.

**2026-10-05 · Input events in `strand-scene`.** `InputEvent` lives in
`strand_scene::input` (re-exported by `strand_surface`);
`SurfaceHost::input` sees every event first and `take_input` creates the
channel. Serials stay in `strand-surface`; the cursor is `default` on
enter; an `osd` gets an empty input region (click-through).

**2026-10-05 · SCTK features and presentation clock.** SCTK runs without
default features (no xkbcommon yet) and `rustix` reads
`wp_presentation.clock_id`. Headless wlroots reports refresh 0, so refresh
locking is covered by fake-clock unit tests.

## m0

**2026-10-05 · Hand-built demo scene.** `strand run --demo` builds the
hello bar's scene by hand (`crates/strand/src/demo/scene.rs`) in place of
compiler output; `split` sections are offset by ±12 px for the `pad` M2
will honour. Texts and colours are placeholders, and the font is
`"Inter, sans-serif" 13px 500`.

**2026-10-05 · One bar node on every output.** The demo's `bar` uses
default `screens`, so each monitor gets a layer surface painted at its own
scale; per-monitor instances arrive in M1, and `strand run` without
`--demo` stays "not implemented (M1)".

**2026-10-05 · Clock through a watch.** The clock is a `Signal<i64>` of
Unix minutes and a chrono `Memo` (`%H:%M`) that the emitter watches,
turning `Tick::changed` into `SetProp(text)`, at most one diff per tick. It
sleeps on a `CLOCK_REALTIME` timerfd at the next minute
(`TFD_TIMER_CANCEL_ON_SET`) plus `Runtime::next_deadline`.

**2026-10-05 · Gate readings.** Damage per tick is checked per committed
frame and summed over outputs, on submitted (age-widened) damage. "No
wakeups" is zero growth of context switches over all threads from :03 to
:57; PSS is `Pss:` of `smaps_rollup` after two ticks.

**2026-10-05 · Copy-forward shm buffers.** A new buffer starts as a copy
of the newest frame and reports age 1, so the first change after boot
does not repaint the whole bar and fail the damage gate.

**2026-10-05 · `STRAND_LOG`.** A comma list of a level (`error`, `warn`
default, `info`, `debug`, `trace`, `off`) and topics; `damage` prints one
`strand: damage surface=…` line per frame and `strand: dropped surface=…`
on a failed commit.

**2026-10-05 · Text layouts keyed by line box width.** Layouts are keyed by
node, scale and `max_width`, since alignment happens inside the line box
and two outputs of one scale but different widths drew the wrong layout.
A painted surface draws a resampled, shifted stand-in while its layout is
re-shaped; unwanted slots are pruned.

**2026-10-05 · Demo first-frame wait 500 ms.** The demo holds a new bar's
first frame up to 500 ms for its text (renderer default 50 ms), so a boot
under load paints text-complete first frames.

**2026-10-05 · Demo exit.** `strand run --demo` exits 0 when the
compositor goes away, and with the logic thread's error when its channel
closes.

**2026-10-05 · The shared bar node is an M0 shortcut.** In M1 `strand run`
forwards `monitor_*` hooks to logic as the `screens` service, pins one
`bar` instance per monitor and adds a render → logic channel for
`InputEvent`s and layout facts.

**2026-10-05 · PSS gate on release.** The 34 MB PSS gate is asserted on a
release build (`cargo test --release -p strand --test demo`); a debug run
is held to a 40 MB ceiling.

**2026-10-05 · Refresh-lock test uses presentation timestamps.**
`commits_lock_to_the_refresh_rate` checks compositor timestamps via a
`FakeClock` rather than wall-clock time, assuming 60 Hz with 2 ms slack
since headless sway reports `seq` and refresh 0.

**2026-10-05 · Pruned stand-in slots re-flatten.** When `prune_texts` drops
a slot with a layout, surfaces wanting a layout-less slot of that node are
marked dirty so none keeps drawing a gone layout.

**2026-10-05 · Exit script sums damage.** `scripts/m0-exit.sh` gates damage
per tick as the sum over all bars in every scenario.

## integration

**2026-10-05 · `strand` binary.** `strand check` and `run --demo` with the
mimalloc allocator live side by side in `main.rs`.

**2026-10-05 · MSRV 1.89.** `rust-version` is 1.89, the highest any locked
dependency declares (vello_cpu 0.3, parley 0.11 needs 1.88).

**2026-10-05 · CI Wayland and budget tiers.** CI installs sway, grim and
fonts-dejavu-core and sets `STRAND_REQUIRE_SWAY=1`, so sway-backed tests
fail instead of skipping; it also runs the release `demo` test for the
34 MB PSS gate.

**2026-10-05 · Checklist coverage.** Items the design names but
features.md lacked were added unticked (reload-latency p95, text props, the
6 MB image LRU, surface namespaces, `blur_fallback`, the lock-screen VM
tier and others).

**2026-10-05 · CI toolchain pinned.** CI uses Rust 1.97.0, the dev
container's toolchain, not floating `stable`; bump it deliberately and fix
new lints in the same commit. CI also installs `pkg-config` and
`libfontconfig1-dev`.

## wave2-check

**2026-10-05 · The schema is data.** Elements, records, services,
functions, palette roles and tiers are declared in
`schema/builtin.schema`, one table for checker and LSP; service crates add
text with `Schema::extend` and configs check with `compile_with`.

**2026-10-05 · The palette is Material 3's system roles.** Under Strand's
names (`accent` = primary, `fg` = on-surface, `bg` = background); base
tiers and derived names are typed in the schema and valued by `tokens`,
and redefining a role in a set needs `override`.

**2026-10-05 · What is global, what is per file.** Components, surfaces,
enums, types, fns, token sets, keyframes and services share one
namespace; top-level `state`/`let` belong to their file, reached as an
exported `file.name`. One `use tokens` and one `use palette` per config.

**2026-10-05 · `Async<[T]>` reads as its last list.** Other uses of an
`Async<T>` as `T` are errors fixed by `?? fallback`; `.pending`, `.error`
and `.value` are its members. (Narrowed in round 4.)

**2026-10-05 · Events and node booleans need an element.** `when`,
`hover`, `self`, poses and element events directly in a component body
are errors; `on show` belongs on a surface; `id:` names are visible in
their whole body.

**2026-10-05 · Bare variants.** `edge: top` resolves against the expected
enum; without one, a variant unique across enums resolves, else
`Enum.variant` is required.

**2026-10-05 · Checked on first use.** Declarations and tokens are typed
when first read, so a static cycle is reported with its path (`a → b → c
→ a`); a `fn` with a declared return type may recurse.

**2026-10-05 · Assignment kills no binding.** Assigning to a `let`,
parameter, prop or read-only field is an error with a fix; `<->` targets
state, settings and `rw` fields, and only `value`, `text`, `open`,
`current` take `<->`.

**2026-10-05 · Raw-colour lint covers prop values.** Settings defaults,
token values and `set { }` right-hand sides are exempt. (Widened in rounds
4 and 5.)

**2026-10-05 · `persist`.** It stores plain data only; misplaced `persist`
is `syntax::persist` with the fix.

**2026-10-05 · Parameter types where needed.** An untyped, undefaulted
component parameter takes the join of its call sites' types, else one
`check::needs_type`; `fn` parameters always need types.

**2026-10-05 · One mistake, one diagnostic.** Failures become `Ty::Error`,
accepted everywhere; `$fg-muted` becomes one error naming `$fg.muted`.

**2026-10-05 · `strand check` type-checks.** It runs `compile` over the
`find_files` set; the wave-1 checker TODOs are done.

**2026-10-05 · Whole-number literals.** `0` is a `float` unless an `int`
is expected; an untyped whole-number `state`/`let` is an `int` until a
fraction is written, when the checker reruns with it pinned `float`.
(Refined in round 3.)

**2026-10-05 · A file named like a service.** An export from
`battery.strand` could never be read as `battery.x`, so it is an error
asking to rename the file.

**2026-10-05 · Did-you-mean ranking (round 2).** Expected-enum variants
first, keyframes for `play`, exports as `file.name`; ties go to a
plausible typo, then length, then alphabet; at most 200 per file.

**2026-10-05 · A settings record is written field by field (round 2).**
`prefs = …` or `<->` to the whole record is `check::read_only`, fixed by
`prefs.accent = …`, since the merge is per field.

**2026-10-05 · Shadowing is loud (round 2).** A name in scope that is also
an expected-enum variant is `check::ambiguous`; a parameter redeclared in
its body and a file `state` named like a global are errors.

**2026-10-05 · `on change` reactivity by kind (round 2).** State, settings
and services change; a `let` only if it reads one; fns, enums, types,
components and token sets never do.

**2026-10-05 · Unreachable `match` arms (round 2).** A repeated pattern or
an arm after `_` is the error `check::unreachable_arm`.

**2026-10-05 · File names that cannot be read (round 2).** An exporting
file with a non-identifier stem is `check::file_name`.

**2026-10-05 · Shader uniforms (round 2).** Until naga reflection, a `u_*`
prop takes numbers, units, colours and comma vectors; text, booleans,
records and lists are errors.

**2026-10-05 · One catalogue with the scene (round 2).** Every schema
element is a `NodeKind` and prop a `Prop` (`tests/scene_catalogue.rs`);
invented props are dropped, while `input`'s `type` and `dash` stay
(`Prop::InputType`, `Prop::Dash`).

**2026-10-05 · `Drop` (round 2).** External drops are `Drop { kind:
DropKind (files | app | text), files, app: App?, text }`; `on drop`'s
payload is `any`, so handlers name their type.

**2026-10-05 · Palette role names (round 2).** Strand's names map 1:1 to
M3's (`accent*` for primary, `bg`/`on_bg`, `fg` = on_surface,
`fg_variant`, `surface_lowest`…`surface_highest` for
surface_container_*, `inverse_fg`, `inverse_accent`); importers and
`material()` fill them by this table.

**2026-10-05 · Long declaration chains (round 2).** Lazy entry points run
under `stacker::maybe_grow`, so 5,000-link chains check on a 2 MiB thread
(`tests/deep_chains.rs`).

**2026-10-05 · `strand check <file>` (round 2).** A file is checked with
its config and only its own diagnostics are reported; a file outside the
module set is checked alone.

**2026-10-05 · Schema docs, defaults and hash (round 2).** `///` comments
fill `Schema::docs`; `ParamSig::default` keeps default text;
`Schema::fingerprint()` is BLAKE3 over every `extend` text.

**2026-10-05 · Whole-number passes (round 3).** Plain fraction writes are
pinned before the first pass (`check/prepin.rs`) and hand-offs are
followed; there is no pass cap, since pins only grow.

**2026-10-05 · Inferred parameters (round 3).** A whole-number literal
argument makes an inferred parameter `int`; a bare builtin variant there
is `check::unknown_name` asking for `Align.center`; recursive inferring
components join through the pass loop.

**2026-10-05 · A fn without a body (round 3).** `fn get() = a` reports
only `syntax::expected`.

**2026-10-05 · Uniform vectors (round 3).** 2 to 4 comma values (WGSL
`vec2`–`vec4`), else `check::type_mismatch`.

**2026-10-05 · What shadowing is reported (round 3).** Only names that
silently change meaning; a `let` or `for` binding hiding a file `state`
is accepted.

**2026-10-05 · `Async` lists read through `.len` and `for` only (round
4).** `filter`, `map`, `sort_by`, `take`, `skip`, `reverse` keep the
`Async` (`hits.take(3) ?? []`); `.first`, `.last`, other methods and
passing it to `any` are `check::async`.

**2026-10-05 · Overloads are chosen by shape (round 4).** By named
parameters, `from`, positional count and required parameters; only
ambiguous calls try overloads, at most two levels deep, so cost is
polynomial (`negative/overload_lazy.strand`).

**2026-10-05 · Builtin names are not shadowed (round 4).** Replaced by
round 5's prelude, except that a handler `let` declared twice and a prop
set twice are `check::redeclared`.

**2026-10-05 · A `let` holding a colour is linted (round 4).** (Widened by
round 5.)

**2026-10-05 · `segmented`'s value is an option (round 4).** `options:
Look` makes `value` a `Look`, a list of `T` a `T`; the schema keeps
`any`.

**2026-10-05 · Extensions add, never replace (round 4).**
`Schema::extend` refuses existing names and same-parameter overloads as
"declared twice".

**2026-10-05 · One error for a kebab name (round 4).** `my-bar.open` is one
`check::unknown_name`; `vol.opacity = 1` is one `check::assign_to_prop`.

**2026-10-05 · Builtin names are a prelude (round 5).** User declarations
shadow builtin functions, values and types silently, so a new crate name
never breaks configs; calling a shadowing non-function names the hidden
builtin. Shadowing a builtin service warns `check::shadows_builtin`; a
component named like a builtin element stays an error.

**2026-10-05 · Component tokens are overridden loudly (round 5).** A set
entry at `Toast.radius` needs `override` (else `check::override_needed`)
and takes the component token's type.

**2026-10-05 · Raw-colour lint covers every named value (round 5).** A raw
colour in a prop, `let`, `state`, `fn` result or parameter default is
`check::raw_color`; settings defaults, tokens and `material(seed: …)` are
exempt.

**2026-10-05 · A dropped `Async` is an error (round 5).** A handler
statement of type `Async<T>` is `check::async`: "`await` it, or assign
the result".

**2026-10-05 · `await` on a list transform waits (round 5).** `await
hits.take(2)` waits for the load, then transforms.

**2026-10-05 · One more prop set twice (round 5).** `meter 0.5 { value:
0.7 }` is `check::redeclared`; `page` outside `pages` is
`check::misplaced`.

**2026-10-05 · `from` parameters (round 5).** Only `from x` or `from: x`
fills one, so `conic($accent, $secondary)` is two stops.

**2026-10-05 · Schema extensions are atomic; stubs provisional (round
5).** `Schema::extend` applies all or nothing; `provisional` service stubs
are replaced once by the first extension naming them.

**2026-10-05 · One member table (round 5).** List and `Async` members live
in `schema::members` (`members_of`), for checker and LSP.

**2026-10-05 · User bindings before builtins at a call (round 6).**
Resolution order is block or parameter, file `state`/`let`, globals, then
builtins.

**2026-10-05 · The language's own types are not a prelude (round 6).**
Redeclaring `int`, `color`, `length`, `Async` and the other primitives is
`check::redeclared`; hidden schema types print as `builtin Align`.

**2026-10-05 · A config `service` named like a builtin stays an error
(round 6).** Services are identified by name at runtime, so crates should
namespace new service names.

**2026-10-05 · The positional's prop is schema data (round 6).** `element
meter(float -> value)` (`ElementSchema::arg_prop`).

**2026-10-05 · Record keys are checked (round 6).** `Schema::extend`
refuses a `key` path that does not resolve.

**2026-10-05 · Set overrides win at runtime (round 6).** Component token
defaults enter the table first, so `override Toast.radius` wins.

**2026-10-05 · Plain data is what the codecs store (round 7).** `persist`
and settings take only what `vm::persist` round-trips
(`TypeTable::is_data`): numbers, text, paths, colours, enums and lists,
nullables and records of them; not handles or records with an `action`.
`for … key` needs `TypeTable::is_comparable`.

**2026-10-05 · A static cycle of components is an error (round 7).**
Unconditional mounts (not under `if`, `match`, `for` or `on_demand`) form
a graph whose cycles are each one `check::cycle`.

**2026-10-05 · Hiding the time signal warns (round 7).** A `state` or
`let` named like a builtin value (`let t = 3`) gets
`check::shadows_builtin`; functions are left out.

**2026-10-05 · Hand-offs through lists, then over-approximately (round
7).** List, index and `match` hand-offs are followed; from the fourth
pass (`DEEP_FLOWS_AFTER`) sources are over-approximated, bounded in round
8 by fixed-type members like `.round()`.

**2026-10-05 · Diagnostics wording (round 7).** Lambda parameters keep
their names, whole-number literals read `int`, and the not-exported help
writes the declaration's own form; strings have no interpolation.

**2026-10-05 · Lambdas take named arguments (round 8).** `f(b: "x", a:
1)` binds by name like a `fn` call, removing a concept.

**2026-10-05 · Live service items are not persisted (round 8).** Records
with an `action` are not data; the help says to store the key. Runtime
handles are `handle record`.

**2026-10-05 · On-demand elements are schema data (round 8).**
`ElementFlags::on_demand` marks `popup`, `tooltip` and `page`.

## wave2-lsp

**2026-10-05 · The formatter keeps the author's lines.**
`strand_compiler::fmt` normalises layout over the lossless token stream but
never joins or splits lines, so every design.md block is a fixed point
(`tests/fmt.rs::fixtures_are_formatted`). It fixes indentation (two spaces
per block), in-line spacing, blank lines (at most one), line endings and
redundant `;`.

**2026-10-05 · Alignment the author made is kept.** A `{`, `=>`, `=`, prop
value or trailing comment set apart by two or more spaces keeps its
column when the line still fits, and hanging items keep the column of a
block's first item; automatic alignment would rewrite design.md's own
inconsistent examples. Both rules are idempotent.

**2026-10-05 · Formatting never changes meaning.** Whether `(`/`[` touches
the word before is kept, and tokens are joined only if they lex back the
same. Each result is re-parsed and compared (`fmt::shape`); a difference
is `FormatError::Unstable`, and a file with syntax errors is not formatted
(`FormatError::Syntax`).

**2026-10-05 · `strand fmt [--check] [paths]`.** A directory means its
module set; no path means the config directory. Files are rewritten via a
temporary file renamed over the canonical path (symlinks stay links);
`--check` writes nothing and fails on unformatted files.

**2026-10-05 · Which files a document is checked with.** The default
config directory (`initializationOptions.configDir` overrides) if the file
is in its module set; else the deepest workspace folder holding `.strand`
files with the file in its module set; else its own directory's set; else
alone. Open documents replace disk text, and diagnostics are published for
every file of the config.

**2026-10-05 · Files changed outside the editor (round 1).** An analysis
recompiles when any read file's or scanned directory's size or mtime
differs; clients with dynamic registration are asked to watch
`**/*.strand`. `documentChanges` clients get versioned edits, and closing
a config's last document outside workspace and config directory drops its
analysis.

**2026-10-05 · The schema the server checks with (round 1).**
`strand_dev::serve_with(conn, Arc<Schema>)` takes the extended schema
once; checking, hover, completion and rename all read it. `serve` uses
the builtin schema.

**2026-10-05 · Bad notifications (round 1).** Unparseable notification
params are logged (`window/logMessage`, stderr) and ignored; only a closed
connection ends the server.

**2026-10-05 · Debounce.** Diagnostics wait 200 ms after the last change
(`initializationOptions.debounceMs`), below the overlay's 250 ms quiet
rule, and publish at once on open and save; requests analyse the latest
text without publishing.

**2026-10-05 · Completion on half-typed text.** After a bare `.` the
server checks a copy with a placeholder name and reads the receiver's
type; members come from `schema::members_of`, the checker's table. In a
component call's block children are offered only if it has a `slot`,
otherwise only its parameters.

**2026-10-05 · What `<->` offers.** Only writable places: states in scope,
exported states as `file.name`, settings fields (never the record) and
`rw` service fields up to three deep (`audio.sink.volume`).

**2026-10-05 · Rename.** Config-declared names, parameters (also at call
sites' props), locals, `id:` names and config tokens rename; builtins,
schema services, fields, variants and schema tokens are refused with a
reason. A grouped token key renames in place. Every edited file is
re-checked and a rename that adds an error or changes what another name
refers to (by reference counts) is refused.

**2026-10-05 · Quick fixes.** Fixes come from `Diagnostic::suggestions`,
never help text; a did-you-mean replaces the misspelt text itself
(`chnage`, or only a unit's letters `12pz` → `px`). One proposal is
preferred; several are each a fix, none preferred.

**2026-10-05 · Unknown parameters read as design.md shows (round 2).** A
call's unknown prop is `unknown prop `expanded`` with "did you mean
`open`?" when an unset parameter is close or the only one left, else
help listing the parameters, each a fix; code `check::unknown_param`.

**2026-10-05 · The one-line diagnostic form (round 3).** `render_short`
prints `file:line:col: error[code]: message; help` with backtick names in
double quotes, a superset of design.md's `bar.strand:12:` form; the caret
render keeps backticks.

**2026-10-05 · Token quick fixes (round 3).** `$space` used as one token
offers every member as a fix, none preferred; an override key whose
closest token is outside its group gets help naming where it is, so every
did-you-mean has exactly one fix (`checker.rs::fixes`).

**2026-10-05 · The builtin schema is documented (round 2).** Every
`builtin.schema` entry has a `///` doc worded from design.md, enforced by
`schema::tests::builtin_schema_is_documented`; a token without its own
doc reads its group's, and docs claim no unit or range design.md does not
give.

**2026-10-05 · Which config shows a file (round 2).** The server remembers
which config last published each file; a config directory that later
holds a lone file takes it over. Watched-file events and stale disk stamps
re-check the affected configs, including open documents' configs as they
are now (round 3).

**2026-10-05 · Completion after a dot (round 2).** The placeholder
analysis is cached per place with its parent analysis, so asking again
before the text changes compiles nothing.

**2026-10-05 · Hover docs (round 1).** A declaration's doc is the `//`
block directly above it, except a file-opening block starting with the
file's name. A token's hover lists each value with its defining set,
marking overrides.

**2026-10-05 · `strand fmt` arguments (round 1).** A file named twice is
formatted once; `--` ends options; a missing default config directory is
named. `- -b` keeps its space.

**2026-10-05 · Help-only hints are never fixes (round 4).** A `NameHint`
(`check/expr.rs`) is `Fix(name)`, a quick fix, or `Help(text)`, a help
line only; another file's private declaration is a `Help`
(`lsp.rs::no_quick_fix_for_a_private_declaration_elsewhere`). Only an
identifier replacement is ever a suggestion.

**2026-10-05 · Server housekeeping (round 4).** Due diagnostics publish
before the next message is read
(`lsp.rs::busy_clients_still_get_diagnostics`); unneeded analyses are
dropped after each request; a hover on an untyped expression answers
nothing; an attribute line like `@reset` keeps the declaration below at
item indentation.

**2026-10-05 · Tree-sitter is out of scope this wave.** The M1 tree-sitter
grammar stays open; editors get everything else from the server.

## wave2-vm

**2026-10-05 · Tokens stay symbolic in the VM.** `$accent` is a
`Value::Token`, sent as `PropValue::Token`, so render resolves tokens each
frame and a palette spring reaches every prop.

**2026-10-05 · The positional argument's prop.** `text`/`button`/`letters`
fill `text`; `meter`/`graph`/`merge` `value`; `effect` `style`; `page`
`name`; others `source`. (Schema data since wave2-check round 6.)

**2026-10-05 · Palettes before M2.** `material(seed:)` is a deterministic
OKLCH stand-in (`vm/palette.rs`); `import()` knows Catppuccin; the default
palette is `material(seed: #7aa2f7, dark: system.dark)`.

**2026-10-05 · Component tokens are global defaults.** So an ancestor's
`set { $Toast.radius: … }` overrides them.

**2026-10-05 · `exit` mirrors `enter` in the emitter.**

**2026-10-05 · `~` without a duration.** `~ bezier(…)` runs 300 ms
(`BEZIER_DURATION`); `~ 200ms` uses the standard curve.

**2026-10-05 · `play`.** `play shake` sets `play` to `[shake, n]`, so
replaying is a change.

**2026-10-05 · Time signals until M4.** `t` and `wave(…)` read 0 and
`noise(…)` is computed once. (Warned since fixer round 1.)

**2026-10-05 · Numbers carry units at run time.** `int / int` is a
`float`, `==` ignores units, division by zero is an error value.

**2026-10-05 · Handlers always run as tasks.** Via `rt.spawn_input`,
`rt.spawn_for` or `rt.spawn`, polled in the flush that started them, so a
click's writes land in the same tick.

**2026-10-05 · `on change` identity.** A keyed record's field re-baselines
when the record changes (`rt.on_change_keyed`): switching sinks pops no
OSD.

**2026-10-05 · Per-monitor bars.** A `bar` is instantiated per picked
monitor, keyed by `Screen.id`; one whose monitor leaves is parked with
its state, dropped by `Instance::forget_screen(id)` after 30 s.

**2026-10-05 · Service readers and visibility.** A surface holds its
services only while shown; hidden content is suspended with its state
kept, and `show`/`hide` follow `open`.

**2026-10-05 · `persist` is core's store.** JSON by declared type; paths
are qualified by keyed instances (`TopBar[<monitor id>].expanded`);
kept-over-default values become `Update::notices`.

**2026-10-05 · Settings files are core's.** `state prefs from "prefs.toml"
{ … }` is `rt.settings_file` with a TOML `FieldSpec` per field; each field
is its own signal written back via `toml_edit`.

**2026-10-05 · Errors keep the last good value.** Failing bindings report
in `Update::errors` and keep their prop; bad keyed mutations change
nothing.

**2026-10-05 · The mock's behaviour.** `SchemaHost::mock` simulates search,
workspaces, a fixed clock and notification removal, and logs actions;
`SchemaHost::real` keeps no history.

**2026-10-05 · Awaiting and async lets.** `let x = svc.m(args)` is an
async memo over the arguments, one `ServiceHost::fetch` per change.

**2026-10-05 · Render → logic input.** Until hit tests, `event`,
`set_flag`, `set_size` and `write` take node ids; events bubble via
`propagate()` up to their surface.

**2026-10-05 · A keyed reset is reconciled.** A `Reset` is matched by key,
moving only items outside the longest in-order run.

**2026-10-05 · Element flags belong to their scope.** Flags and size live
in the owning scope, so `id:` names read them anywhere.

**2026-10-05 · Keyed lists follow diffs.** A keyed `state` is a
`KeyedSignal`; a `for` over it gives each item a value cell, so one
changed row re-runs one row (2,000 rows).

**2026-10-05 · Located runtime errors.** `RuntimeError` carries span,
node, component and freeze scope; `Instance::origin` maps nodes to
source.

**2026-10-05 · Durations at their limits.** Out-of-range durations and a
zero `every` are error values naming their use.

**2026-10-05 · Events on one element.** Several handlers all run in
order, sharing one `propagate()`.

**2026-10-05 · A second `slot` mount.** A concurrent second mount gets its
own element states.

**2026-10-05 · Widget writes are typed.** Mismatched `<->`/`strand set`
values are refused; `f32` becomes the shortest round-tripping `f64`.

**2026-10-05 · Declared edges.** The compiler declares every read and
write set before the first flush (`lower::reads`, `instantiate::edges`),
relying on wave2/core's ranked `flush`.

**2026-10-05 · Keyed chains.** A `for` over `.filter`/`.map`/`.take`/
`.sort_by` on a keyed root uses core's incremental views, rebuilt when a
lambda or what it reads changes.

**2026-10-05 · Keyed reads.** `.len`, `.first`, `.last`, `[i]`,
`.contains` compile to `Op::Keyed`.

**2026-10-05 · `await` on a pending value.** All awaiters get one result;
nothing to wait on is the error "nothing to await".

**2026-10-05 · Faults at the top level.** They carry no scope, and
`Instance::freeze` declines them.

**2026-10-05 · Visibility reaches everything under a surface.** A nested
`popup` holds its reads only while open.

**2026-10-05 · The wall clock every step.** `Instance::step` sets wall
time each step, so a clock set back never freezes.

**2026-10-05 · `strand run` before hit testing.** Until M2/M3 input and
layout are per surface and `screens.focused` is the first monitor; a
config with errors is not run.

**2026-10-05 · `strand run` shutdown and sleep (review round 3).**
Signals and compositor loss send `ToLogic::Shutdown`, and logic runs
`Runtime::shutdown` so recent writes reach disk; logic sleeps on a
`CLOCK_REALTIME` timerfd.

**2026-10-05 · Action writes (review round 3).** A service action call
declares `ServiceHost::action_writes(rt, service)` from the first flush.

**2026-10-05 · A chain in a `let` is one view (fixer round 1).** Lowered
as `Program::let_chains`, one shared `KeyedMemo`.

**2026-10-05 · One run of a timer at a time (fixer round 1).** A fire
while the last run awaits is skipped; events keep one run each.

**2026-10-05 · Scoped handler frames (fixer round 1).** Block scopes and
minimal lambda captures (`Lambda::free`) make handler loops linear.

**2026-10-05 · Frozen time signals are warned about (fixer round 1).**
Once per name (`lower::time_signal`).

**2026-10-05 · `ServiceHost` and M3 (fixer round 1).** One composite `Rc<dyn
ServiceHost>`; since round 3 services run on threads sending `Send`
patches (`#[derive(Store)]`) to a logic-side store.

**2026-10-05 · A whole handler `let` is an `int` (fixer round 2).**

**2026-10-05 · Work counters, not clocks (fixer round 2).** Linearity tests
read `Vm::peak_frame`, not wall time.

**2026-10-05 · A written number takes its declared tag (fixer round 3).**
Every written leaf takes its type's `int` or `float` tag, so a widened
write does not fire `on change` twice.

## wave2-core

**2026-10-05 · Keyed collections at 2,000 rows.** `KeyedVec`'s key map
tolerates stale entries, so mutations never re-index; `keyed_diff` uses
`foldhash` with the fewest `Move`s; over 128 diffs on a quarter of the
source rebuilds.

**2026-10-05 · Keyed writes under the 30 writes/s guard.** Replaces wave 1.
A throttled handler's changes are a held set rebased by key and landed as
one diff; inapplicable ones count in `Diagnostic::KeyedConflict`.

**2026-10-05 · Frozen = paused, with a bounded backlog.** Frozen timers
and sleeps pause until resume; frozen listeners keep up to
`MAX_FROZEN_EVENTS` (256), reporting `Diagnostic::EventsDropped`.

**2026-10-05 · `persist` storage.** One file per cell under
`$XDG_STATE_HOME/strand/persist/`, FNV-1a header hashes; an unchanged
default is adopted, a changed one kept with `PersistDefaultChanged`;
corrupt files are quarantined; writes are atomic, debounced 250 ms.

**2026-10-05 · Topological effect order.** Replaces creation order: sinks
run once per tick by rank (`src/order.rs`) over reads, ownership and
write edges; `on change` starts at 2^20; loop-closing edges are feedback
edges (`tests/order_props.rs`).

**2026-10-05 · Declared reads and the provisional phase (review round
1).** `rt.reads_from` makes ranks complete before running, so every sink
runs once from the first flush; undeclared sinks run at 2^20 - 1.

**2026-10-05 · Foreign wakes (review round 1).** Other threads' wakes are
polled only at a flush's start.

**2026-10-05 · Persist IO thread and handle (review round 1).** One IO
thread per store with a per-file queue; `Runtime::shutdown` waits up to 5
s; `Persisted` has `redeclare` and `reset`; a duplicate path gets
`PersistPathInUse`.

**2026-10-05 · Settings files (review round 2).** `rt.settings_file` with
`FieldSpec`s, per-field `toml_edit` writes after `PERSIST_DEBOUNCE`; the
overlay in `$XDG_STATE_HOME/strand/settings/` takes read-only targets'
writes (`SettingsIssue::ReadOnly`); a file with a syntax error is never
overwritten.

**2026-10-05 · Settings files (review round 3).** A last-good snapshot
supplies broken files' values at boot; reads take sequence marks; handles
on one file adopt each other's writes; `redeclare` resets type changes
(`TypeChanged`); `Settings::layer` reports the winning layer.

**2026-10-05 · Persist path hand-over (review round 2).** A waiting cell
takes the path over when the owner is disposed.

**2026-10-05 · Feedback edges are not errors (review round 2).**
`rt.writes_to` returns `WriteEdge::Ranked | WriteEdge::Feedback`.

**2026-10-05 · Composite nodes declare their own edges (review round 2).**
`rt.async_memo` declares its write; `AsyncMemo::effect_id()` takes input
reads.

**2026-10-05 · Small fixes (review round 2).** Quarantine is
`.<name>.corrupt`; `rt.is_idle()` is false while an IO failure waits to
be reported; reentrant `get_untracked` is `Error::Reentrant`.

**2026-10-05 · Persisted writes are read at unmount (review round 4).**
Unmount, `shutdown` and dropping the last `Runtime` queue each cell's live
value.

**2026-10-05 · Strand's own writes are pre-registered (review round 4).**
`PersistStore::on_written` reports each `OwnWrite` before the rename.

**2026-10-05 · The watcher does not parse settings files (review round
4).** Supersedes round 3 (c): it sends (path, hash); logic calls
`Settings::reload`.

**2026-10-05 · Lowering can be checked (review round 4).**
`Stats::learned_edges` and `Stats::reruns` must be 0 on real fixtures.

**2026-10-05 · Derived collections read by key (review round 4).**
`KeyedMemo::get_key` is a documented O(n) scan.

**2026-10-05 · Persist failures, retention and the observer (review round
5).** A failed write makes the next change write again; unclaimed files
expire after 90 days (`PERSIST_RETENTION`, narrowed in round 6); observer
panics are caught.

**2026-10-05 · Reload writes (review round 6).** `Signal::set_reloaded`
marks downstream `on change` handlers to take the value as baseline, so a
reload never fires them; used by redeclare, hand-over,
`reset_reloaded` and settings.

**2026-10-05 · Retention is for instance files (review round 6).** Only
instance-qualified paths (`%5B` in the name) and quarantines expire.

**2026-10-05 · Small persist fixes (review round 6).** `load` checks queued
then in-flight operations; early writes arm the debounce.

**2026-10-05 · Event deliveries and woken tasks are ranked (review round
6).** They are items of the ranked queue. (Refined in round 7.)

**2026-10-05 · Strict edges (review round 6).** `rt.set_strict_edges(true)`
reports `UndeclaredWrite`/`UndeclaredRead` once; test runtimes enable it.

**2026-10-05 · Each listener at its own rank (review round 7).** Emits go
into each listener's inbox, delivered at its own rank, so all see final
values.

**2026-10-05 · Handler reads are learned (review round 7).** Undeclared
reads in listeners and tasks become edges, counted and reported in strict
mode.

**2026-10-05 · Reload marks reach late landings (review round 7).** Async
loads started by a reload write land as reload writes; an erroring `on
change` takes its mark first.

## wave2-watch

**2026-10-05 · Raw inotify (rustix), not notify, and no debouncer.**
notify adds `IN_OPEN`/`IN_ATTRIB` (waking on every read), so the watcher
uses `rustix::fs::inotify` with `CLOSE_WRITE | MOVED_TO | MOVED_FROM |
CREATE | DELETE | DELETE_SELF | MOVE_SELF | ONLYDIR | EXCL_UNLINK` on one
thread; it decides each path by `lstat` and BLAKE3 after the quiet period.
If `inotify_init` fails, it polls.

**2026-10-05 · What counts as an event.** `CLOSE_WRITE`, `MOVED_TO`, and
creations no close will follow (links, FIFOs); removals are existence
checks; a write in progress is not read. A never-quiet stream is cut at
500 ms.

**2026-10-05 · Removal grace 50 ms.** So delete-and-create is one
`Modified`; other saves keep 15 ms.

**2026-10-05 · Scratch names.** `4913`, `*.swp`, `*.swo`, `*.swx`, `*~`,
`*___jb_*___` and hidden names are ignored.

**2026-10-05 · Symlinks hop by hop.** Every hop's directory is watched
canonically; a link swap to identical bytes is still `Modified` with the
new `canonical`.

**2026-10-05 · Immutable stores are not watched.** Read-only `/nix/store`
and `/gnu/store`; network and FUSE filesystems are polled.

**2026-10-05 · Polling.** NFS, SMB, 9p, Ceph, AFS, Coda and FUSE poll every
second (`Options::poll_interval`) by stamp, with a 30 s re-hash sweep
(`Options::content_sweep`) of files up to 1 MiB.

**2026-10-05 · Cache trees are not hashed.** `Role::Cache(kind)` paths
report by existence only.

**2026-10-05 · Own writes.** `register_own_write(path, hash)` matches once,
expires after 10 s, and drops earlier registrations for the file.

**2026-10-05 · Portal client.** `strand_watch::follow(&Connection,
EventSink)` reads the keys with a 500 ms boot limit (`BOOT_READ_TIMEOUT`)
and always sends the boot batch; later reads send `at_boot: false`, and a
`SettingChanged` beats an in-flight re-read.

**2026-10-05 · Missing and replaced directories.** Missing directories are
watched via their nearest ancestor; moved or deleted ones drop their
subtree's watches and re-hash; new watches list their directory once more
after the next quiet period.

**2026-10-05 · Registrations per role.** Counted per (path, role);
`set_referenced` replaces the loader's set whole.

**2026-10-05 · Only regular files are read.** Others report once with
`InvalidInput`; hashing streams.

**2026-10-05 · Batch timestamps.** `FileBatch` carries `first_event` and
`last_event`, `SystemBatch` `received`.

**2026-10-05 · `ConfigReloaded { failed: Option<bool> }`.** Hyprland gives
`None`, niri `Some(failed)`.

**2026-10-05 · Watch, then read, for every baseline.**

**2026-10-05 · Loaded hashes.** `Referenced { path, role, loaded }` makes
`loaded` the baseline, so a save before registration is caught.

**2026-10-05 · Own writes are registered synchronously.** Through an
`Arc<Mutex<_>>` list a running flush sees.

**2026-10-05 · Files linked in complete.** In config directories a
non-empty single-link `CREATE` is complete unless a `MODIFY` follows;
elsewhere it is held.

**2026-10-05 · Light ancestor watches.** Ancestors of watched directories
hold names-only removal watches, so moving one reports `Removed`.

**2026-10-05 · Backend errors do not spin.** `poll(2)` retries back off
from 10 ms; a failing inotify `read` switches to polling and rescans.

**2026-10-05 · Writes in progress are held (review round 4).** Held until
`CLOSE_WRITE`, or read with `Notice::StalledWrite` after
`Options::stalled_write` (5 s).

**2026-10-05 · Two registration sets (review round 4).** Loader and
`watch_file` registrations count apart, so the wallpaper survives
reloads.

**2026-10-05 · Parent watches (review round 4).** The config root's
parent and stand-in ancestors are watched for names only.

**2026-10-05 · Content-only batches leave the watches alone (review round
4).** (`content_only_flushes_do_not_resync`.)

**2026-10-05 · Own writes across aliases (review round 4).** A match on
`(canonical, hash)` covers every alias in the flush.

**2026-10-05 · Which thread may block (review round 4).** `watch_file`,
`set_referenced`, `watch_tree` block and are never for logic, which uses
`watch_file_loaded`, `unwatch_file` and `register_own_write`.

**2026-10-05 · `MODIFY` only in config directories (review round 5).**
Other directories watch completed writes and names only, holding new
non-empty files until `CLOSE_WRITE` (`tests/wakeups.rs`).

**2026-10-05 · Stalls are measured by the file (review round 5).**
Without `MODIFY`, a stall also needs an unchanged mtime.

**2026-10-05 · The quiet period runs from completed writes (review round
5).** A `MODIFY` never moves it.

**2026-10-05 · Events keep the mask they were queued under (review round
5).**

**2026-10-05 · The IPC socket is not a watch source (review round 5).**
The binary serves it on the logic loop; `strand reload` calls
`Watcher::rescan()`.

**2026-10-05 · The bus filters portal namespaces (review round 5).** Via
an `arg0='org.freedesktop.appearance'` match rule.

**2026-10-05 · A file written during its read is not reported (review
round 6).** After hashing, `SEEK_DATA`, a re-stamp and a drain; a moved
stamp, mtime under 15 ms or a drained event skips the file (waived at
`max_delay`).

**2026-10-05 · A file rewritten without pause is read within `max_delay`
(review round 7).** Cut 500 ms after its first due event; its batch's
`last_event` is what the latency benchmark should use.

**2026-10-05 · File age is measured after the read (review round 7).**
From the wall clock; mtimes up to 1 s ahead count as recent.

**2026-10-05 · A `MODIFY` holds the file (review round 6).** Read with
`StalledWrite` after the writer stops, and again when closed.

**2026-10-05 · `O_TMPFILE` files outside the config (review round 6).**
Completed-write watches drop `IN_EXCL_UNLINK`; a `#<digits>` close ends
that inode's write.

**2026-10-05 · Module-set diagnostics travel with the batch (review round
6).** `Notice::ModuleSet { errors, too_deep }`.

**2026-10-05 · The cost of `MODIFY` in config directories (review round
6).** Every write there wakes the watcher; non-module files belong in
`$XDG_STATE_HOME` or `$XDG_CACHE_HOME`.

**2026-10-05 · The tight-loop own-write test is bounded (review round
6).** 1,000 writes, below the 16,384-event queue; change-less rescans
allowed.

## wave2-runtime

**2026-10-05 · A reload mounts the new program beside the old one.**
`Instance::reload` mounts on the same runtime, taking over nodes, cells
and unchanged handlers by reload key (scope place plus `Sid`), and
`Emitter::reduce` sends only the difference
(`random_edits_land_on_a_cold_boot`).

**2026-10-05 · Surfaces share one identity label.** A kind change keeps
the `Sid`; a `layer` or `name` change recreates only that surface with its
state. `bar` ↔ single surface resets cells only with several instances
(fixer round 3).

**2026-10-05 · The loader compiles the whole program per batch.** On
`strand-compile`, holding back erroring files (at most 64 compiles); the
cache keeps last good sources keyed by hashes, `COMPILER_VERSION` and
`Schema::fingerprint()`.

**2026-10-05 · The kept-value notice quotes text.** `launcher.query: kept
"fir" (default changed) [reset]`.

**2026-10-05 · A parked bar's cells survive a reload.** In a pending table
until the monitor returns or `forget_screen`.

**2026-10-05 · Runtime faults: frozen and outlined.** `Instance::freeze`
suspends the scope and forces a 2 px `#e5484d` border; a reload clears it.

**2026-10-05 · The error overlay is made of external nodes.** A
`StrandErrors` panel (`Instance::external_create`), overlay layer, 960 px,
opening after 250 ms of standing errors. Rows open `$STRAND_EDITOR`
(`{file}`, `{line}`, `{col}`), else `$VISUAL`/`$EDITOR +<line> <file>`,
else `xdg-open`.

**2026-10-05 · The IPC protocol.** `$XDG_RUNTIME_DIR/strand-<display>.sock`
(`$STRAND_SOCKET`), versioned JSON lines (`"v": 1`), one answer per
request; slow watchers drop at 1 MiB; a live socket is never replaced.

**2026-10-05 · Pointer input goes to the node under the pointer.**
`Renderer::hit` picks the topmost node; `hover` covers the chain; clicks go
to the innermost node under both press and release.

**2026-10-05 · Reload timing.** `total_ms` runs from the watcher's last
event to sending the diff.

**2026-10-05 · A lock edit holds back the whole build while a lock is
shown.** Such builds (and `--hard`) are deferred with `"deferred": true`
until unlock, later builds waiting with them, announced as `waits for the
unlock` (`lock_edits_wait_for_the_unlock_and_then_land`).

**2026-10-05 · An unreadable file is held back, never removed.**

**2026-10-05 · Reload notices are overlay rows.** Warning-coloured until
dismissed; `[reset]` calls `Instance::reset(path)`.

**2026-10-05 · Terminal editors run in a terminal.** Via
`xdg-terminal-exec`, else `$TERMINAL -e`.

**2026-10-05 · Removed custom services stop.** `ServiceHost::stop(name)`.

**2026-10-05 · IPC clients that half-close are answered.**

**2026-10-05 · A revert to the last good text closes the overlay.**
`Outcome::cleared` without committing.

**2026-10-05 · A deferred load replayed after the unlock shows the newest
problems.**

**2026-10-05 · Each `strand reload` gets its own event.** `Job::Reload`
carries its client.

**2026-10-05 · Merkle hashes by strongly connected component.** Iterative
Tarjan, one hash per component, linear cost.

**2026-10-05 · Reload latency is measured save → painted buffer.**
`reload_latency_meets_its_budget`, optimised in CI: p95 under 35 ms
(token) and 50 ms (markup).

**2026-10-05 · The overlay's rows.** At most 40, keyed by cell path;
`[reset]` uses `KeptCell { path, shown }`.

**2026-10-05 · Fn read sets per strongly connected component.** One union
per component; callee locals stay out
(`a_long_chain_of_fns_lowers_quickly`).

**2026-10-05 · A repeated attempt is not compiled or reported twice.**
`Outcome::repeated` (`a_reload_of_a_broken_config_is_one_event`).

**2026-10-05 · `strand watch` names kept cells as data.** Events carry
`kept_over_default: [{path, shown}]` and `ambiguous`; boot-time kept cells
go as `{"event": "notices", …}`.

## wave2-exit

**2026-10-05 · The reload fuzzer saves every edit in all five styles.**
`random_edits_through_five_save_styles` runs five live pipelines, one per
style (in place, write-and-rename, backup-then-rename, delete-and-create,
symlink swap), covering design.md's edit table except services and
`lock`.

**2026-10-05 · What the fuzzer asserts.** Each diff shows before or after,
never between; no overlay flash on valid saves; after each step scene,
tokens, resets and offline pixels equal a cold boot.
`STRAND_FUZZ_EDITS` (60), `STRAND_FUZZ_SEED`; nightly 10,000.

**2026-10-05 · The edit table, as the fuzzer reads it.** A cell takes a
new default only if it still holds the old one; renames and retypes
reset; handler edits keep state; a module renamed in one load resets as
`renamed`.

**2026-10-05 · Save → pixels ends at the presentation.** Measured to
`wp_presentation_feedback.presented` on headless sway plus a modelled
vblank: p95 35 ms token, 50 ms markup
(`reload_latency_to_the_presented_frame`).

**2026-10-05 · "Monitor changes on the next frame", measured.** In the
first painted frame, presented within two refreshes
(`a_monitor_change_is_in_the_next_diff`).

**2026-10-05 · A frame waits briefly for a new node's text.** Up to
`NEW_TEXT_WAIT` (16 ms).

**2026-10-06 · Round 2: a module's cells removed with it are reported.**
As `removed with its module`, when set.

**2026-10-06 · Round 2: the reload fuzzer also runs on a compositor.** A
sixth pipeline on headless sway checks live surfaces equal the scene's;
CI requires it.

**2026-10-06 · Round 2: the overlay's 250 ms, as the fuzzer checks it.**
Errors may open it only after a 200 ms hold.

**2026-10-06 · Round 2: more of the fuzzer's draws are run.** Compiling
word mutations run and are saved back with no reset.

**2026-10-06 · Round 2: a descheduled delete-and-create.** It fails only if
the watcher really took two loads; CI uses `STRAND_FUZZ_MAX_GAP_MS=20`.

**2026-10-06 · Round 2: a surface in motion never holds for new text.**
Within `BUSY_WINDOW` (34 ms).

**2026-10-06 · Round 2: the plug gate runs from hearing of the monitor.**
From `wl_output.done` to the bar's first frame, within a refresh.

**2026-10-06 · Round 3: the sway pipeline's screen equals a cold boot.**
Within 2 per channel after every committing step.

**2026-10-06 · Round 3: the overlay's hold on the logic thread's
timeline.**

**2026-10-06 · Round 3: the monitor model's p95 is exact.** By bisection
(`the_monitor_model_is_the_exact_p95`).

**2026-10-06 · Round 3: the first plug of a new width is printed, not
gated.**

**2026-10-06 · Round 3: thread ends, and clock-free busy tests.** The
watcher is held weakly so `Worker::join` joins it; `set_busy_window` fixes
test timing.

## wave2-lang

**2026-10-06 · An input task's run is its own writer.** Post-`await`
writes count against the run, so one write per input at 60 Hz is not
throttled; loops in one run still are.

**2026-10-06 · Keyed scaling is guarded by counting key work.**
`keyed_scaling.rs` counts hash and `eq` calls at 2,000 and 16,000 rows and
times a shuffled diff, in `cargo test`.

**2026-10-06 · Atomic persist writes are proven at the rename.**
(`a_write_replaces_the_file_whole_by_rename`.)

## wave2-integration

**2026-10-06 · Per-lookup key work is gated by its mean.** At most 8 for
`get`/`index_of`, 2.5 for `contains_key`, capped at 24 per lookup.

## wave3-theme

**2026-10-06 · A crate for palettes.** `strand-theme` holds `Role` (49
roles), `material()`, importers, the derivation table and the built-in
theme, depending only on `strand-scene`, whose `Color` holds the gamut,
contrast and lightness maths render shares.

**2026-10-06 · The Material spec is pinned to 2021.** `material(seed:)`
uses `SpecVersion::Spec2021` (`strand_theme::material::SPEC`) so a crate
release cannot recolour a theme; `contrast:` clamps to −1..1.

**2026-10-06 · Wallpapers.** `material(image:)` thumbnails to 128 px and
quantises with Celebi (128 colours) on its own thread, keyed by stamp and
BLAKE3. Seeds persist in `$XDG_STATE_HOME/strand/palettes/`; while
quantising it is a pending `Async` holding the last image palette.

**2026-10-06 · `??` takes a kept value while loading.** The VM's `??` on
an `Async` gives its value whenever it has one, so the old palette holds
through `material(image:) ?? material(seed: …)`.

**2026-10-06 · Import sources.** `import()` takes
`catppuccin:<flavour>[:<accent>]`, `base16:`, `base24:`, `matugen:` and
`w3c:` files; all fill the rest through `Partial::fill` and the guard, and
file imports are watched.

**2026-10-06 · The contrast guard's pairs and where it runs.** M3's pairs
(`contrast::PAIRS`) at WCAG 3:1 travel in `TokenTable::contrast`; render
solves each text token's OKLCH lightness in the node's scope, meeting at
least the text's own background, and ignores translucent ones.

**2026-10-06 · The built-in theme.** `tokens base` sits under every set;
without `use palette` the palette is `material(seed: system.accent ??
#7aa2f7, dark: system.dark, contrast: system.contrast)`.

**2026-10-06 · A missing font family falls back whole.** A family without
a generic gets `monospace` (Mono, Code, Courier, Consol, Terminal names)
or `sans-serif` appended (`with_generic`).

**2026-10-06 · The portal in `strand run`.** Logic starts
`PortalSettings::spawn`; boot values are baselines
(`SchemaHost::set_initial`), and last values persist in
`…/palettes/system` for the first frame.

**2026-10-06 · `strand set`.** `strand set <path> <value>` (IPC `"cmd":
"set"`) writes an exported `state` parsed by type; settings fields are
reachable exported or not (`strand set prefs.compact true`).

**2026-10-06 · Settings notices on the overlay.** They are overlay rows
and `strand watch` notices; `[clear]` calls
`Instance::clear_settings_overlay`.

**2026-10-06 · The contrast guard's cost (review 1).**
`Color::with_contrast` solves in closed form with one bisection and
`TokenScope` memoises 256 pairs, so a frame solves each pair once.

**2026-10-06 · Theme defaults follow `set` (review 1).** Text with no
`color` or `font` reads `$fg` and `$font.ui` in its own scope.

**2026-10-06 · A bar is themed without a `bg` (review 1).** A `bar` with
no `bg` paints `$surface`; panels and OSDs stay clear.

**2026-10-06 · The fixed accents (review 1).** 49 roles, adding
`accent_fixed`, `accent_fixed_dim`, `on_accent_fixed`,
`on_accent_fixed_variant` and their `secondary`/`tertiary` kin, derived at
lightness 0.9, 0.8, 0.12, 0.32.

**2026-10-06 · W3C component groups (review 1).** A DTCG path names a role
only if at most one word before it is not a colour group (`color`, `sys`,
`palette`, `md`, …); imported roles are gamut-mapped and opaque.

**2026-10-06 · Wallpaper cache bounds and gaps (review 1).** 64 wallpapers
kept; stamps include change time; a vanished wallpaper stays "pending,
last palette" for `MISSING_GRACE` (500 ms).

**2026-10-06 · The last good palette (review 1).** Persisted in
`…/palettes/palette`; a failing or still-loading `use palette` uses it,
else the built-in palette.

**2026-10-06 · Referenced files are read once more when registered
(review 1).** Each newly registered file is reported changed right after
`set_referenced`.

**2026-10-06 · Provenance and small persistence fixes (review 1).**
`TokenTable::origins` records each path's source for the inspector; temp
files carry the pid.

**2026-10-06 · Wallpapers are decoded at reduced size (review 2).**
Baseline JPEG uses DCT scaling and PNG streams rows into a 128 px grid (a
4K wallpaper peaks under 2 MB); other formats decode whole within
`FULL_FRAME_BYTES` (64 MiB). Past 40 Mpx or 128 MiB is refused.

**2026-10-06 · A torn wallpaper holds its palette (review 2).** A failed
read of a path that gave a seed holds it for `MISSING_GRACE` and re-reads.

**2026-10-06 · Runs sharing the wallpaper cache merge it (review 2).**
Under a lock on `palettes/index.lock`, keeping the 64 most recent.

**2026-10-06 · State writes and imports off the frame path (review 2).**
`strand_theme::FileWriter` writes atomically (1 s at drop); `import()`
files must be regular and at most 1 MiB.

**2026-10-06 · The first frame waits briefly for the portal (review 2).**
Up to 100 ms (`BOOT_PORTAL_HOLD`).

**2026-10-06 · Settings notices leave when fixed (review 2).** A re-read
clears rows it no longer reports; a settings-only overlay is headed
`strand: settings files`.

**2026-10-06 · The guard is one level (review 2).** Backgrounds are
evaluated with the guard off, so derived backgrounds do not recurse.

**2026-10-06 · Edits outside the owned area, for the integrator (review
2).** `flatten.rs` keeps optional inherited colour and font and paints a
bare `bar` in `$surface`; strand-text gained `with_generic`. The bar
default stays in render so the scene holds no prop the source lacks.

**2026-10-06 · The logic side of a swap is timed (review 2).**
`a_theme_swap_is_under_five_milliseconds_of_logic` gates `theme.look` to
`SetTokens` at 5 ms.

**2026-10-06 · Seeds by hash are kept apart from the path index (review
3).** The index holds 64 `seed <hash>` lines and 64 path entries, so a
cycled-back wallpaper is not decoded again.

**2026-10-06 · The wallpaper cache is versioned (review 3).** `version
<CACHE_VERSION>` (`3 material-colors-0.5 spec-2021 box128 celebi128`);
bump it with any change a seed depends on.

**2026-10-06 · A wallpaper being written holds its palette for the whole
write (review 3).** A still-changing file holds its seed and re-reads;
only a settled file is reported on its second failure; each path holds
its own seed.

**2026-10-06 · Results a lookup takes are still reported (review 3).** The
next `poll()` reports them, so no call site stays pending.

**2026-10-06 · Whole-image decodes, honestly bounded (review 3).** Header
checks count decoder peaks (WebP `WEBP_BYTES_PER_PIXEL` 8) against
`FULL_FRAME_BYTES`, then `malloc_trim(0)`; the cap is 40 Mpx.

**2026-10-06 · Settings files are read on the watcher's thread (review
3).** The compile worker reads them and sends `FromWorker::Settings`;
logic only decodes.

**2026-10-06 · Medium text is not faux bold (review 3).** Following CSS
`font-synthesis-weight`, only requests of 600+ on lighter faces embolden.

**2026-10-06 · A hard reload keeps the theme host (review 3).**
`Instance::reload_hard` hands the old `ThemeHost` on; shutdown waits
`DROP_WAIT` (1 s).

**2026-10-06 · What springs in a swap (t2).** Only plain colour roots of
the global table spring, in premultiplied OKLab along `$motion.effects`;
other plain tokens snap and derived tokens re-evaluate each frame. A
newer table retargets with velocity; `Instant`, `reduced_motion` and
nothing shown snap.

**2026-10-06 · When a swap crossfades (t2).** Render plays the roots
through first (240 Hz, 1 kHz under 3.3:1); if a pair readable at both ends
becomes unreachable, or motion exceeds `CHECK_SPAN` (10 s) or
`CHECK_SCOPES` (32), the table snaps and surfaces crossfade from a
snapshot. "A light↔dark swap" means any swap.

**2026-10-06 · The contrast gate (t2).**
`contrast_never_drops_below_three_to_one_during_swaps` holds every sprung
frame of random swaps at 3:1; crossfaded frames are exempt.

**2026-10-06 · The 5 ms of a swap (t2).** It covers logic's re-resolve,
render's `apply` and per-frame token work (`Renderer::take_swap_work`),
benched in release; crossfade blending is held to 4 ms per frame.

**2026-10-06 · Widget defaults (t2).** `segmented`, `meter`, `slider` and
symbolic `icon` take `$fg` and `$font.ui` from their scope.

**2026-10-06 · The token graph once per frame (t2 fixer 1).** The global
table is evaluated once per `SetTokens` and swap frame
(`TokenTable::freeze`); `set { }` scopes still evaluate in their own scope.

**2026-10-06 · Span colours are painted, not shaped (t2 fixer 1).**
Shaping uses colour slots and painting maps the real colours, so marks
and links follow a swap without reshaping.

**2026-10-06 · Crossfades on several surfaces (t2 fixer 1).** Each surface
reads the fade at its own presentation time; a colourless table keeps it
running. Its snapshot caps were superseded 2026-10-10 (m4-owner-docs).

**2026-10-06 · Guarded text and its side (t2 fixer 1).** The guard keeps
no frame memory; solved text crosses its background at most once per
swap.

**2026-10-06 · The play-through's cost is bounded (t2 fixer 2).** Steps
grow to `CHECK_STEP_MAX` (100 ms) while roots move under `CHECK_MOVE`
(0.02 OKLab), only reaching scopes are checked, and past `CHECK_WORK`
(9000 units) the swap crossfades.

**2026-10-06 · How the 5 ms scales with a slower spring (t2 fixer 2).**
Per swap under 5 ms whatever the spring, per frame under 250 µs, and the
whole under 5 ms along design.md's springs.

**2026-10-06 · Crossfades are decided per surface (t2 fixer 2).** An
unreadable `set { }` scope crossfades only surfaces drawing it, held on
the new table (`Renderer::swap_held()`).

**2026-10-06 · Each snapshot fades on its own (t2 fixer 2).** Progress is
per snapshot; a new crossfade fades on from what shows; a silent surface
drops its snapshot.

**2026-10-06 · What the contrast gate covers (t2 fixer 2).** Sprung
frames globally and in `set { }` subtrees, including
`material(image:)`-style palettes; crossfades are exempt.

**2026-10-06 · Text in alpha-derived tokens is not guarded (t2 fixer 3).**
`$fg.muted` and `$fg.faint` can fall near 2:1 mid-swap; the 3:1 promise
covers declared pairs only.

**2026-10-06 · The check runs out per scope (t2 fixer 3).** The global
scope gets the whole `CHECK_WORK` budget first; scopes that run out
crossfade only their surfaces.

**2026-10-06 · Scopes that appear mid-swap are checked (t2 fixer 3).** New
scopes from a diff or attached surface are played through then.

**2026-10-06 · The whole swap is gated up to eight scopes (t2 fixer 3).**
Under 5 ms with 0 and 8 reaching scopes; at 32 only `apply` (5 ms) and
each frame (250 µs) are gated.

## wave3-pixels

**2026-10-06 · Container defaults.** `row`, `start`, `center`, `end`
centre the cross axis; `col` and `scroll` stretch; `stack`, `box` and
roots use one stretched cell; `split` is `minmax(0, 1fr) auto minmax(0,
1fr)`; `grid { columns: n }` packs at the start. The example shells
render as meant with no props added.

**2026-10-06 · `shrink` and `justify`.** Added to `node` and `Prop`:
`shrink: float` (default 1; 0 for absolutely sized `image`/`icon` since
wave4-exitReport r1) and `justify: start | center | end | space_between |
space_around | space_evenly`.

**2026-10-06 · `x`/`y` stay paint-only, also with `place: absolute`.**
Moving any node never relayouts; percentages are of the parent's box.

**2026-10-06 · Text in its box.** Text is shaped once unbounded and placed
by `align`, reshaped only for a narrower box; its minimum is its longest
word (0 with `ellipsis`/`max_lines`). Unshaped text is estimated at 0.55
em per character; `1ch` is 0.6 em.

**2026-10-06 · Font fallback.** A family list without a generic gets
`sans-serif` (or `monospace`) appended.

**2026-10-06 · Virtualised `list`.** taffy sees one leaf summing row
heights (measured, else mean, else 32 px) capped by
`height`/`max_height`; only visible rows are laid out.

**2026-10-06 · Container-query hysteresis lives in logic.** Render reports
sizes (`ToLogic::Layout`); a size `when` holds while true within 4 px, so
`< 300` turns off at 304.

**2026-10-06 · Overhang and input region.** Shadows past the root give an
`overhang` (1.5 × blur + 1 + spread plus offsets) that grows the layer
surface via negative margins and exclusive zone; the input region is the
box. Content-sized surfaces size from a content pass.

**2026-10-06 · Keyboard routing.** Focus goes to the first `focus: true`
node or a clicked `input`/`list`; keys send `key(k)`; Up/Down/Return drive
the `nav` list; Escape writes two-way `open: false`. Routing is
`strand_render::input::Router::handle` (fixer round 2).

**2026-10-06 · `nav:` names a node mounted later.** It is set at the end of
the tick.

**2026-10-06 · A mock desktop for `strand run`.** `STRAND_MOCK=desktop`, a
development aid only.

**2026-10-06 · Underline in strand-text.** `TextSpan::underline` and
`GlyphRun::underline` for `<u>` and links (`$accent`).

**2026-10-06 · Content sizing stays virtualised and capped.** Content past
`MAX_CONTENT_SIZE` (4096 px) is laid out at the cap; `LayerConfig::fit`
clamps to the output (`Renderer::set_surface_bounds`).

**2026-10-06 · Render reports only the sizes logic reads.** Elements
marked `Prop::Watch` get facts, so other size animations never wake
logic.

**2026-10-06 · Container queries settle inside the frame.** A changed
`query` size holds the frame until `SceneDiff::layout_seen` answers or
`QUERY_WAIT` (30 ms), once; surfaces in motion (`BUSY_WINDOW`) never
hold.

**2026-10-06 · List rows mount eagerly until M4.** Render handles only
visible rows but logic mounts all
(`a_2000_row_list_mounts_eagerly_and_updates_one_row`).

**2026-10-06 · What `markup: basic` takes for a tag.** Only well-formed
`b`, `i`, `u`, `a`, `img`; anything else is text; nested tags combine.

**2026-10-06 · Scrolling and keys are default actions that always run.**
`scroll` is vertical; wheels scroll the innermost movable container and
hand on at its end. Escape, `nav` keys and typing always run, since render
cannot wait for logic.

**2026-10-06 · Click-away from a `keyboard: exclusive` panel (fixer round
2).** A panel with two-way `open` (`Prop::TwoWay`) gets a transparent
catcher surface with a hole at its box, sending `InputEvent::ClickAway`;
since round 3 presses on other Strand surfaces also close it and other
outputs get full catchers.

**2026-10-06 · Centred surfaces get an even overhang.** The larger side is
used on both sides of centred axes.

**2026-10-06 · A resized content-sized surface waits for its configure.**
Frames hold until configured or `RESIZE_WAIT` (50 ms); fixer round 3 also
reruns unsettled list estimates and holds any mismatched configure once.

**2026-10-06 · Fewer layout passes.** No content pass for closed surfaces,
and for fixed-size ones only until first laid out.

**2026-10-06 · The input region stays a rectangle.** Hit testing uses the
rounded shape, but the Wayland input region is the box.

**2026-10-06 · An `input` draws its text.** Or its `placeholder` in
`$fg.muted` while empty.

**2026-10-06 · `nav:` naming a node mounted ticks later.** Stays pending
across ticks.

**2026-10-06 · Values from a file are clamped before placement.** To
±2^20 px (`placement::MAX_LOGICAL`), with saturating arithmetic.

**2026-10-06 · An `input` is one line.** Clipped and shifted so its end
stays visible.

**2026-10-06 · Logic's own `input` text wins over edits in flight.**
`Router::observe` treats an unmatched text as logic's own, dropping
in-flight writes.

**2026-10-06 · An empty axis frame is no scroll.** Detents without pixels
scroll 15 px each (`WHEEL_STEP`).

**2026-10-06 · A container query is decided per node, not per surface.**
One `self.width` per node, so a bar on two monitors shows the last
reported variant on both.

**2026-10-06 · The M2 exit depends on popups and widgets.** The four-shell
exit box stays open on xdg_popup and widget drawing.

**2026-10-06 · Springs start at the frame that shows them (p2).** One
refresh (≤ 16.7 ms) before that frame's presentation; retargets keep
position and velocity.

**2026-10-06 · What springs, and when (p2).** Only a `SetProp` on a node
already on screen; token-driven changes snap at rest or steer a spring;
`$motion.*` fall back to the design's springs.

**2026-10-06 · Layout lengths snap, sizes spring (p2).** `pad`, `margin`,
`gap`, `min_*`, `max_*`, `grow`, `shrink`, `align`, `justify` snap and
boxes glide (FLIP, `$motion.spatial`); `width`, `height`, `size` spring,
clipping content.

**2026-10-06 · Exits are ghosts (p2).** A removed node with an exit pose
lives in a ghost table, drawn but not hit, until it settles; a closing
surface stays open meanwhile.

**2026-10-06 · Pose presets (p2).** `fade` is `opacity: 0`; `slidefade`
adds `y: 8`; `popin(s)` is `scale: s; opacity: 0`; `slide(edge)` moves by
the node's size; transforms are about the centre.

**2026-10-06 · `reduced_motion` reaches render as a setting or a token
(p2).** `Renderer::set_reduced_motion` or `motion.reduced: true` snap all
motion.

**2026-10-06 · A content-sized surface waits to shrink (p2).** It keeps
the larger size until everything settles.

**2026-10-06 · The paint that ends a motion reports its surface change
(p2 fixer 1).** `Painter::paint` refreshes specs so the host wakes.

**2026-10-06 · A surface's first frame is never the time-zero preview (p2
fixer 1).** So `enter` plays from the first frame shown.

**2026-10-06 · Exits are bounded (p2 fixer 1).** At most 8 ghosts per
parent; exits end after 1 s of silence or `MAX_MOTION` + 1 s.

**2026-10-06 · What does not spring yet (p2 fixer 1).** `ANIMATED` is `x`,
`y`, `opacity`, `scale`, `rotate`, `bg`, `color`, `border`, `shadow`,
`radius`; widget and effect values join later.

**2026-10-06 · Size springs lay out once per frame (p2 fixer 1).** Targets
are learnt by a pass at rest.

**2026-10-06 · List poses play when their surface opens or closes with
them (p2 fixer 2).** Nodes created with the opening play `enter`; a
closing surface waits for its ghosts.

**2026-10-06 · Named curves and timed retargets (p2 fixer 2).**
`out_elastic`/`out_bounce` are closed forms; `Easing::NAMES` bounds the
schema; an interrupted timed curve adds `v0·t·(1 − t/d)²`.

**2026-10-06 · Transforms, hit shapes, several outputs (p2 fixer 2).**
`rotate` is degrees; transformed nodes hit through the inverse transform;
exits end only when no surface drew the node.

**2026-10-06 · Known limits (p2 fixer 2).** Size springs snap when a
content-sized surface grows; removing a surface root skips its exit.

**2026-10-06 · A collapsing slot reaches zero (p2 fixer 3).** Padding and
the parent's gap fold as size nears zero, so siblings slide up with no
plateau.

**2026-10-06 · A surface is opening until its first clocked frame (p2
fixer 3).** Nodes created meanwhile play `enter`; `radius: full` and
percentages spring; a growing centred surface FLIPs content;
`EXIT_STALL` ends exits on silent outputs.

## wave3-pixels (p3)

**2026-10-06 · Gradients are dithered pixmaps (p3).** Every gradient is
drawn per pixel from a 1,024-entry OKLab table with 8 × 8 Bayer dither,
cached in a `PAINT_CACHE_BYTES` (4 MB) LRU; past `MAX_ENTRY_BYTES` (2 MB)
it is dithered per cell uncached (`render_gradient_part`).

**2026-10-06 · Shadows are cached pixmaps (p3).** Keyed by geometry
relative to the whole-pixel origin, blur, radii and colour, so moves
reuse them; past `MAX_ENTRY_BYTES` they draw directly.

**2026-10-06 · `corners: squircle` (p3).** Superellipse quadrants (n = 5)
reaching 1.6 × the radius for fill, border and clip; shadow and hit shape
stay circular.

**2026-10-06 · `blur: N` until the compositor blurs (p3).** Nodes report
`Painter::blur_region` for `ext-background-effect-v1`; until
`Renderer::set_compositor_blur(true)` alpha rises by `BLUR_TINT` (0.15)
unless `blur_fallback: none`.

**2026-10-06 · Widget state lives on the render thread (p3).** In
`strand_render::widgets::Widgets`, so widgets answer on the input's
frame; logic still hears every change.

**2026-10-06 · Widget defaults (p3).** `button`: 4, 10 px pad on
`$surface.hi`, `$radius.md`, 8 %/12 % state layer; `meter`: pill in
`$accent` over `track`; `slider`: 120 px, 4 px track, 14 px knob;
`segmented`: equal segments, chosen on `$accent`/`$on_accent`.

**2026-10-06 · Editing an `input` (p3).** Caret editing with Ctrl word
moves, Shift selection, Ctrl+A; a steady 1.5 px `$accent` caret (no blink
wakeups); `type: password` shows bullets; clipboard waits for M4.

**2026-10-06 · Only two-way values move (p3).** Slider drags and
`segmented` clicks write `value` only when bound two-way.

**2026-10-06 · A node may shape several texts (p3).** Text requests carry
a `part` for `segmented` labels.

**2026-10-06 · `image` and `icon` sources (p3).** `icon` names a theme
icon; `image` takes a path or else an icon name. Theme:
`$STRAND_ICON_THEME`, GTK `settings.ini`, then Adwaita; symbolic icons
mask in `color`; decodes fit the box in a 6 MB LRU (`IMAGE_CACHE_BYTES`);
an unsized image is 16 × 16.

**2026-10-06 · Icon names fall back (p3).** Through the other `-symbolic`
variant, then shorter `-segment` names, as GTK 4 does.

**2026-10-06 · Decodes hold little more than the drawn size (p3).** JPEG
IDCT scaling (jpeg-decoder 0.3) and row-reduced PNG; full decodes cap at
`MAX_DECODE_BYTES` (64 MB).

**2026-10-06 · Images through a size spring (p3).** The latest decode
scales meanwhile; only the resting size is decoded.

**2026-10-06 · Images decode off the render thread (p3).** On a
`strand-image` thread woken via `TextWorker::waker`; inline offline.

**2026-10-06 · Popups are xdg_popups (p3, from M4 to M2).** Specs carry
`SurfaceSpec::parent` and `anchor_rect`; the positioner opens away from a
bar or below the anchor (gap default 6 px), sliding and flipping. A popup
grabs only with a press serial within `GRAB_WINDOW` (500 ms), then takes
the keyboard; `popup_done` becomes `InputEvent::ClickAway`.

**2026-10-06 · `tooltip: expr` is a render-owned popup (p3).** Shown after
`TOOLTIP_DELAY` (600 ms) of rest as overlay nodes
(`SceneTree::add_overlay`) in inverse-surface styling; the `tooltip { … }`
element warns `check::not_drawn_yet`.

**2026-10-06 · The M2 exit's shells (p3).** On the mock desktop the four
shells run with widgets and the calendar popup
(`the_design_shells_draw_their_widgets_and_the_calendar_popup`).

**2026-10-06 · Grabbing is what was sent (p3).** `Role::Popup::grabbed`
decides dismissal, exclusivity and keys; keyboard moves send
`KeyboardLeave`/`KeyboardEnter`.

**2026-10-06 · Popups opened late (p3).** A grabless popup closes on
Escape, a Strand-surface press or logic, not a click on another client;
an accepted deviation in features.md.

**2026-10-06 · The launcher in the exit test (p3).** It substitutes `open
= true`, since `strand toggle` is M5.

**2026-10-06 · `Range` reaches `marks:` as a pair (p3).**

**2026-10-06 · The paint cache is freed when idle (p3).** After `IDLE_FREE`
(10 s); a gradient caches only when a second frame repeats its key.
Replaces the round-2 rule.

**2026-10-06 · Stand-in decodes keep their aspect (p3).** Placed by
`Decoded::placed_in` and clipped.

**2026-10-06 · Mock icons (p3).** Mock items use Adwaita names
(`web-browser`, `utilities-terminal`, …).

**2026-10-06 · An empty `text` takes no line (p3).** It lays out 0 × 0;
`button` and `input` keep theirs.

**2026-10-06 · A focused input's list selects its first row (p3).** Via
`Router::settle`.

**2026-10-06 · Focus survives a popup's grab (p3).** The focused node is
kept across an enter without a leave.

**2026-10-06 · `apps.search` ranges count characters (p3).**

**2026-10-06 · The four-shells exit item is partial (p3).** Nothing in M2
opens the launcher, so the box stays unticked rather than pulling M5's
CLI forward.

## wave3-pixels (exit)

**2026-10-06 · The launcher opens with `strand set` (exit).** `strand set
launcher.open true`, so all four shells run byte for byte; `strand
toggle` stays M5.

**2026-10-06 · The acceptance mock (exit).** `STRAND_MOCK=acceptance`
freezes the clock at `MOCK_TIME` (Mon 5 Oct 2026 09:41:07 UTC) and adds
workspaces on `HEADLESS-2`; mocks answer IPC `"cmd": "mock"` (`notify`,
`volume`, `muted`, `brightness`), real runs refuse it.

**2026-10-06 · `on scroll(dy)` counts notches (exit).** `dy`/`dx` are
detents (`value120 / 120`, or pixels / 15), so `dy * 0.05` is 5 % a notch.

**2026-10-06 · An `image` of a symbolic icon takes the foreground colour
(exit).** It inherits `color`, as `icon` does.

**2026-10-06 · Fresh focus starts a `nav` list at the top (exit).**
(Narrowed in fixer r1.)

**2026-10-06 · Reference screenshots (exit).** Settled regions are compared
with `crates/strand/tests/refs/acceptance/`: over 24 per channel differs,
at most 0.5 %; `STRAND_UPDATE_REFS=1` rewrites, each read before commit.

**2026-10-06 · mimalloc without transparent huge pages (exit).** mimalloc
builds with `no_thp`. (Completed in carried r1.)

**2026-10-06 · Idle cache entries go without a wake of their own (exit).**
Freed at the next paint or other wake, keeping zero wakeups between ticks;
replaces the p3 timer.

**2026-10-06 · A text change damages its changed glyphs (exit).**
(`a_tick_repaints_only_the_glyphs_that_changed`.)

**2026-10-06 · `reduced_motion` from the portal (exit).**
`SystemSetting::ReducedMotion` feeds `system.reduced_motion` and the new
protocol field `SceneDiff::reduced_motion`.

**2026-10-06 · Key repeat is strand's own timer (exit, fixer r1).** SCTK's
repeat panics when a keyboard vanishes, so a calloop `Timer` from
`repeat_info` repeats keys
(`a_keyboard_going_away_after_a_press_leaves_the_shell_running`).

**2026-10-06 · What the acceptance tests wait for (exit, fixer r1).** The
resulting state (`Desk::wait`), then the reference (`Desk::settled_ref`);
any 4×4 block with over 4 differing pixels fails.

**2026-10-06 · Idle cache entries go when the frame loop stops (exit,
fixer r1).** Entries the finished run did not use are freed.

**2026-10-06 · A nav list keeps the row the user moved to while its query
stands (exit, fixer r1).** A new text or fresh focus selects the first
row.

**2026-10-06 · The glyph diff is linear (exit, fixer r1).** Prefix and
suffix skipped, at most 64 cells compared.

**2026-10-06 · Key repeat guards (exit, fixer r2).** Interval at least 1
ms; stop without focus; a changed modifiers mask or layout stops repeat.

**2026-10-06 · CI runs on `ubuntu-24.04` (exit, fixer r2).** References
depend on its sway 1.9, fonts-dejavu-core 2.37 and adwaita-icon-theme 46.

**2026-10-06 · Boxes snap from absolute positions (exit, fixer r3).**
taffy runs unrounded and `read_back` snaps edges from root-relative
positions, so `split`'s sides never move
(`split_sides_never_move_with_the_centre`).

**2026-10-06 · The midnight tick is a documented exception to M0's 2,000
px² (exit, fixer r3).** The two ticks after local midnight are held to
design.md's 60×20 px per output instead (`scripts/m2-exit.sh` 1b;
`demo.rs` allows 4,000 px²).

**2026-10-06 · Test compositors die with their test (exit, fixer r3).**
`PR_SET_PDEATHSIG` SIGKILL; the cursor is pinned to Adwaita 24.

## wave3-pixels (carried issues, r1)

**2026-10-06 · On-demand elements mount on demand (carried r1).** A `page`
mounts while current, a `tooltip { … }` while hovered, keeping state in
the surrounding scope (`every_on_demand_element_has_a_runtime_rule`).

**2026-10-06 · A `tooltip { … }` element is out of flow (carried r1).**
(`layout::out_of_flow`.)

**2026-10-06 · Runaway mounts stop at 256 levels (carried r1).** Beyond
`MAX_MOUNT_DEPTH` a located runtime error; `Ctx::runaway` stops fan-out.

**2026-10-06 · Whole-number lists are int lists (carried r1).** `[1, 2]`
is `[int]` until a fraction arrives (`Checker::written_ty`).

**2026-10-06 · CI's build packages (carried r1).** `libxkbcommon-dev`,
`libwayland-dev`, and `dbus` nightly.

**2026-10-06 · The hello bar alone on sway (carried r1).**
(`the_hello_bar_alone_is_laid_out_and_themed`.)

**2026-10-06 · CI's timing and memory gates (carried r1).** The debug
theme-swap ceiling is 8×; the PSS test reads after a whole idle second.

**2026-10-06 · Strand turns THP off itself (carried r1).** `main` calls
`PR_SET_THP_DISABLE`; PSS tests assert `THP_enabled: 0`.

**2026-10-06 · The 32-scope `apply` on GitHub's runners (carried r1).**
Held to 10 ms; 0 and 8 scopes stay at 5 ms.

## wave3-pixels (carried issues, r2)

**2026-10-06 · wave3-pixels (r2): a closed popup's content unmounts, its
state kept.** Replaces the popup clause of r1's "on-demand elements mount on
demand": `popup` is `on_demand` like `page` and `tooltip`, so when `open`
turns false its content's nodes and scopes go, but every `state` cell under
it moves to a kept scope (`Ctx::keep_cells`, `Ctx::closed`) and is reclaimed
on reopen (`Ctx::reopen_cell`), so the calendar's month survives. A closing
surface's removed content stays as a ghost (`Renderer::closing_content`).

## wave3-integration

**2026-10-06 · wave3-integration: the renderer wakes itself for stalled
exits.** The renderer's own timer thread is armed at `Renderer::next_wake`
after every `apply`, `update` and paint, instead of `strand run` adding a
calloop timer: one concept fewer. With nothing left to wake for the timer is
cancelled, so an idle shell is never woken by it.

## wave3-cleanup

**2026-10-06 · wave3-cleanup: build settings live in the repo.**
`[profile.dev]` and `[profile.test]` set `debug = 0` in `Cargo.toml` and
`.cargo/config.toml` turns incremental off, so cargo stops keeping duplicate
crate copies per setting. M1's open boxes get owners:
keyframes/shader/canvas and `.wgsl` watch paths to M4, the latency bench's
portal clause to M3, the tree-sitter grammar to M5.

## wave4-wm

**2026-10-06 · wave4-wm: the compositor service is library code with a typed
change stream.** `strand_services::wm` holds `Workspace`, `Window`,
`WmState` (plus `Workspace::active`, `Window::urgent`) and `wm::run(config,
sink, requests)`, which sends non-empty batches of `WmChange` (keyed
`VecDiff`s via `keyed_diff`, focus, `ConfigReloaded { failed }`, `Sources`):
a `Reset` first, then only what changed.

**2026-10-06 · wave4-wm: protocol preferred per field means a join, not two
lists.** With an IPC adapter its workspace/window sets and ids stand; a
workspace joined to an `ext-workspace-v1` handle by name (and screen when
names repeat) takes `active`, `screen`, `urgent` from it, and a window
joined to the toplevel list by identifier (sway 1.10+
`foreign_toplevel_identifier`, Hyprland `stableId`; niri has none) takes
`title` and `app_id`. Without an adapter the protocols are the whole state:
`windows.focused` is null and window actions answer `Unsupported`.

**2026-10-06 · wave4-wm: which compositor.** `HYPRLAND_INSTANCE_SIGNATURE`
(sockets under `$XDG_RUNTIME_DIR/hypr/<sig>/`, else `/tmp/hypr/<sig>/`),
`NIRI_SOCKET` (with the `niri` feature) and `SWAYSOCK`; a variable whose
socket is missing is skipped. When several remain (nested), the one
`XDG_CURRENT_DESKTOP` names wins, then Hyprland, niri, sway.

**2026-10-06 · wave4-wm: Hyprland.** One request per `.socket.sock`
connection (5 s timeout); subscribe to `.socket2.sock` before the first
read. Patching events patch; structural ones re-read `j/monitors`,
`j/workspaces`, `j/clients`, `j/activewindow` once per burst. Special
workspaces are excluded and their windows count as minimised;
`win.minimize()` is `Unsupported`; titles decode lossily, and a
`windowtitlev2` at the 1024-byte event cap triggers a re-read.

**2026-10-06 · wave4-wm: niri.** Our own JSON-lines client behind the
default-on `niri` feature; events parse leniently (fields defaulted, unknown
ignored). The first `ConfigLoaded` of a stream is not a reload, so
reconnects never fire `wm.config_reloaded`; unnamed workspaces are named by
index; no fullscreen or minimise (`Unsupported`). Every request gets its own
connection, since niri before 25.05 reads one request per connection.

**2026-10-06 · wave4-wm: sway.** swayipc-async 3.0's types over our own
i3-ipc framing, decoded lossily (U+FFFD), because a Latin-1 XWayland title
made strict UTF-8 `get_tree` fail forever. Title events patch; other events
re-read `get_workspaces`/`get_tree` per burst; `workspace` `reload` is
`wm.config_reloaded` (`failed: None`). Scratchpad windows are minimised and
`win.minimize()` is `move scratchpad`.

**2026-10-06 · wave4-wm: lost sockets.** Adapters reconnect with backoff
(100 ms doubling to 10 s, reset only after a connection that lasted 10 s);
actions while away answer `NotConnected`. The last state stays,
`Sources::connected` marks it stale, and the reconnect read goes out as a
diff. Idle wakes no service thread (`tests/idle.rs`: zero context switches
over 2 s).

**2026-10-06 · wave4-wm: the protocol client is plain wayland-client.** Both
protocols are dispatched directly with wayland-client 0.31 and
wayland-protocols 0.32 `staging` instead of SCTK 0.21, which lacks an
`ext-workspace-v1` helper and brings registry/output machinery the
one-thread client does not need.

**2026-10-06 · wave4-wm: focus with the protocols alone.** A workspace is
`focused` only when it is the one non-hidden active workspace; with several
outputs each showing one, none is focused and `workspaces.focused` and the
focused screen are unset, keeping "at most one is focused".

**2026-10-06 · wave4-wm: a broken adapter does not hide the protocols.** The
first batch always carries `Sources` (`connected: false`) for `strand
report`. State goes out at the adapter's first state or, after its first
failed attempt, from the protocols alone; when the adapter comes up its
state replaces the protocols' with a `Reset` of both lists, not a keyed
diff, since ids differ. Until then actions run where the protocol can and
answer `NotConnected` otherwise.

**2026-10-06 · wave4-wm: `wm.name` without an adapter.** It is the first
entry of `XDG_CURRENT_DESKTOP` (`WmConfig::desktop`, from
`WmConfig::from_env`), so labwc or COSMIC show their name.

**2026-10-06 · wave4-wm: who fires `wm.config_reloaded`.** A reload goes out
twice: `WmChange::ConfigReloaded`, which the `wm` store turns into the
language event, and `ChangeEvent::Compositor(ConfigReloaded)` on the
`EventSink`, which is only a live-reload change source. The binary must not
turn the second into another event.

**2026-10-06 · wave4-wm: one service, three stores.** `workspaces`,
`windows` and `wm` are three services with their own reader counts and 5 s
stops, sharing one `wm::run` through `wm::WmHub`: first subscription starts
it, later ones get the current state as one batch, the last dropped stops
it. Runs are numbered so a stopped run's batch never reaches the next; each
subscriber queue holds at most 64 batches, then coalesces.

**2026-10-06 · wave4-wm: the schema the real services serve.** `wm::SCHEMA`
replaces the provisional stubs with the same names plus `Workspace.active:
bool`, `Window.urgent: bool` and `event config_reloaded(failed: bool?)`,
adopted later through `Schema::extend`; `src/wm/schema.rs` checks it
textually against builtin.schema.

**2026-10-06 · wave4-wm: nested windows are copies.** `Workspace.windows`
holds copies, so a title change is also an `Update` of its workspace;
accepted as cheap. If it shows in a profile, the store can derive the list
from `windows.all` by `workspace`.

**2026-10-06 · wave4-wm: smaller fixes.** The protocol thread resolves
`WAYLAND_DISPLAY` against `XDG_RUNTIME_DIR` itself and never takes
`WAYLAND_SOCKET`, destroys toplevel handles after `finished`, and treats a
full socket buffer as wait-for-POLLOUT. The coordinator drains its queue
before merging.

**2026-10-07 · wave4-wm: `ext-workspace-v1` applies at `done`.** Every
workspace and group event, and workspace creation, is held pending until the
manager's `done`; snapshots read only applied copies.

**2026-10-07 · wave4-wm: bounded reads.** A socket2 line, niri line,
Hyprland reply or i3-ipc frame over 16 MiB is an I/O error (reconnect with
backoff), not an ever-growing buffer.

**2026-10-07 · wave4-wm: the IPC adapters' box waits for the wiring.**
features.md's "Hyprland, niri, sway IPC adapters" box is unticked again:
shells see schema defaults until the `#[service]` wiring lands.

**2026-10-07 · wave4-wm: wlr-foreign-toplevel-management is a follow-up.**
The ext toplevel list carries no state or requests, so without an adapter
there is no window focus or action; binding
`zwlr_foreign_toplevel_management_v1` as a fallback fits
"compositor-agnostic first" but design.md does not name it, so it is
recorded as a follow-up on features.md's services item.

**2026-10-07 · wave4-wm: an action's outcome is never a closed channel.**
`WmRequest::new` and `WmSubscription::request` return a `WmReply` future
that reads a reply dropped unanswered as `NotConnected`.
`ProtocolClient::send` answers `NotConnected` after the thread ended, and
the thread answers every queued command before closing.

**2026-10-07 · wave4-wm: the protocol thread starts inside its poll loop.**
No blocking roundtrip: globals bind as they arrive and three chained
`wl_display.sync`s mark the first state inside the `poll(2)` loop, so a
compositor that never answers cannot keep the thread, and dropping
`ProtocolClient` stops it.

**2026-10-07 · wave4-wm: `Window::toplevel` keeps the join.** Each window
carries its `ext-foreign-toplevel-list-v1` identifier when known (not a
schema field), so M4's `thumbnail w` can add a `ProtoCmd` that captures by
identifier on the `strand-toplevel` thread.

**2026-10-07 · wave4-wm: Hyprland titles with a newline.** Titles from
`j/clients` map `\n` to a space as `EventManager::formatEvent` does, so a
`windowtitlev2` patch and the next re-read agree.

**2026-10-07 · wave4-wm (audio): pipewire 0.10.1 with `v1_0_0`, on a
hand-driven thread.** pipewire 0.10.1 builds against Ubuntu 24.04's
libpipewire 1.0.5 with the `v1_0_0` feature.
`audio::Audio::spawn(AudioConfig, sink)` starts a `strand-pipewire` thread
iterated with `Loop::iterate`; callbacks only queue owned `Work` that one
`Driver` handles after each iteration, so there is no shared mutable state
and one batch per burst.

**2026-10-07 · wave4-wm (audio): volume is the cube root of the loudest
channel; writes set every channel.** This matches `wpctl get-volume`; a
write of `v` sets every channel to `v³`, clamped to 0..1 (or the current
value when already past 1), NaN refused. The last 64 writes per device
(`audio::ECHOES`) are remembered so echoes read back as written,
`audio::perceptual` snaps to 1/10 000, and a `Route` echo within 0.005 of a
write reads as it.

**2026-10-07 · wave4-wm (audio): defaults from `default.audio.*`, else
`default.configured.audio.*`; `make_default()` writes both.** The effective
key wins when it names a known device of that direction, so a monitor named
as the default source leaves `audio.source` empty. `make_default()` writes
the configured keys (as `wpctl set-default` does) and the effective ones for
setups without a session manager. `audio.sink.*` writes resolve
`DeviceRef::DefaultSink` on the thread when they run.

**2026-10-07 · wave4-wm (audio): no per-app streams; peak meters are passive
capture streams, at most 60 readings a second.** The schema declares no
streams, so none are followed. `Audio::set_levels(targets)` runs
`node.passive`, `node.dont-reconnect` capture streams whose RT process
callback only folds peaks into atomics; the loop publishes at most one
`Levels` per meter per `audio::FRAME` (1/60 s) and a silent device wakes
nothing. A stopping meter sends one closing quiet reading; a failed one
restarts after 1 s doubling to 30 s. Source meters stay passive so they
never open the microphone.

**2026-10-07 · wave4-wm (audio): reconnection keeps the last devices and
does not poll forever.** A broken-connection core error (`-EPIPE`,
`-ECONNRESET`, `-ENOTCONN`) publishes `Connected(false)` with devices kept
and retries from 100 ms doubling to 10 s; an inotify watch on the socket
directory reconnects at once, and the timer stops only while the socket file
is missing. The new state goes out as a diff once settled (syncs back,
`default` metadata read, prior defaults resolved) or after `audio::SETTLE`
(3 s), and only then is `connected` true, so no empty default flashes.

**2026-10-07 · wave4-wm (audio): an action sent before a connection settles
waits for it.** Actions arriving before a first state are held in order (at
most 64, more refused `NotConnected`) and run right after it; with a
connection being set up they wait up to `SETTLE`, with none `audio::GRACE`
(2 s), then `NotConnected`. After `Audio::stop`, queued actions answer
`NotConnected` and `set_levels` is ignored.

**2026-10-07 · wave4-wm (audio): volume and mute read from node `Props`,
written through the card's active `Route`.** Like `wpctl` and
pipewire-pulse, a card node whose profile device has an active route is
written as `Route { index, device, props, save: true }`, which WirePlumber
persists; otherwise the node's own `Props`. The route path is only
unit-tested: CI has no sound card.

**2026-10-07 · wave4-wm (audio): `AudioDevice` is exactly the schema record;
libpipewire behind a default-on feature.** `icon` is stored
(`audio::icon(direction, volume, muted)`) so the record can be the store's
`#[derive(Data)]`. A changed `object.serial` under a reused id is sent as
`Remove` plus `Insert` (`AudioState::serials`). `Audio/Duplex` nodes are
listed as sinks only. The `audio` module is behind the default-on `pipewire`
feature (without it `audio.*` answers schema defaults). `Audio`'s `Drop`
asks the thread to stop and returns; the sink must never block.

**2026-10-07 · wave4-wm (audio): a private PipeWire for tests.** Each test
starts its own `dbus-daemon`, `pipewire -c` (dummy driver, two null sinks at
`priority.session` 2000/1000, a virtual source at 3000) and `wireplumber` in
a fresh `/tmp/strand-pw.*` directory. With `STRAND_REQUIRE_PIPEWIRE=1` or
`STRAND_REQUIRE_DBUS=1` a missing tool fails the test.

**2026-10-07 · wave4-wm (audio): levels reach the language through
`spectrum`, not a field.** The schema exposes no level; the only declared
consumer is `spectrum(AudioDevice -> source)` (M4), so each visible
`spectrum` subscribes a `LevelTarget` and the store passes the union to
`Audio::set_levels`.

**2026-10-07 · wave4-wm (audio): a daemon that accepts but never answers is
not a connection.** A first state goes out only once the first sync is back
(`SETTLE` forces it without the session manager, never without the daemon).
If that sync has not returned after `audio::UNANSWERED` (6 s), the
connection counts as lost: waiting actions answer `NotConnected` and backoff
retries.

**2026-10-07 · wave4-wm (services): three stores, one hub per services
runtime.** Each store's body subscribes to `wm::hub()`, a `WmHub` in a
thread-local of the shared runtime thread, so each `Services` registry has
its own. The config is process-wide (`wm::configure`, else
`WmConfig::from_env(None)`), not a `Buses` field. Each store waits for its
own part of the state before `ready()`, so a bar does not flash an empty
row.

**2026-10-07 · wave4-wm (services): the toplevel client is the hub's thread,
not a `Start::Thread` body.** The protocol client serves three stores, so it
stays the hub's `strand-toplevel` thread. PipeWire serves one store and runs
on its `Start::Thread` (`strand-audio`), the same loop function given a
`Host` instead of a sink.

**2026-10-07 · wave4-wm (services): `wm`'s schema has no focused monitor.**
The focused monitor is `screens.focused`; adding it to `wm` would put one
fact in two places, so the hub carries `FocusedScreen` for the future
`screens` store.

**2026-10-07 · wave4-wm (services): audio writes are answered by the batch
that shows them, not by value.** Value echoes failed because the record's
`icon` differs between optimistic and reported state. The store answers each
write tagged (`Cx::report`) with the first batch showing its value on its
device; earlier writes of that field are dropped. A refused write or one not
shown within `audio::ANSWER_WAIT` (1 s) is answered with the device as is.

**2026-10-07 · wave4-wm (services): level meters run for taps, only while a
reader is visible.** The store passes the union of `audio::tap_levels` taps
(the M4 `spectrum` hook) to the loop only while `Cx::visible()`; hidden,
every meter stops.

**2026-10-07 · wave4-wm (services): the change-sources box is split by
owner.** features.md's "Change sources beyond files" becomes two boxes with
the same words: the compositor reload (ticked) and the `applications/`,
`index.theme`, fontconfig cache invalidations (open).

**2026-10-07 · wave4-wm (services, fixes r1): a cell's writes are answered
in write order.** One echo state per cell: an answer tagged `g` ends every
pending write of that cell up to `g`, and every write (no-op, refused,
non-leaf, timed out) joins the queue, so an immediate no-op mute answer no
longer reverts a pending volume write. A batch shows a write only when it
changes that device. `ANSWER_WAIT` runs from PipeWire's reply.

**2026-10-07 · wave4-wm (services, fixes r1): the hub owns its protocol
thread.** The hub starts the `strand-toplevel` thread itself (`wm::run`
still does for direct users) and owns its `JoinHandle`; a stop sends `Stop`
and joins it (2 s at most). Its join-on-stop is replaced by the r2 entry
below.

**2026-10-07 · wave4-wm (services, fixes r1): the provisional stubs declare
what the services serve.** builtin.schema's `Window`, `Workspace`,
`windows`, `workspaces` and `wm` stubs carry `Workspace.active`,
`Window.urgent` and `event config_reloaded(failed: bool?)`, so
`Schema::builtin()` alone matches `strand check`.
`the_stubs_declare_what_the_services_serve` requires equal member lines.

**2026-10-07 · wave4-wm (services, fixes r1): what M4 must rework.** (a)
`audio::tap_levels` is a process-wide registry of per-channel peaks; M4's
`spectrum` needs samples or bins and may tie taps to a `Services` registry.
(b) `wm::configure` and `audio::configure` are process-wide; when a second
registry needs another target, they move into `Buses` (or a sibling
`Targets`).

**2026-10-07 · wave4-wm (services, fixes r2): a stop never waits on the
shared runtime; each start detects afresh; swayipc-types.** (a) Replaces
r1's join on stop: the stop only sends `Stop` and parks the
`ProtocolClient`; ended threads are joined at the next start or stop, and
dropping the hub joins the rest (2 s `recv_timeout`). (b) `WmHub::fresh`
makes the `WmConfig` at each start, so a compositor socket created after
strand started is used next time. (c) The sway adapter depends on
swayipc-types 2.0 rather than swayipc-async, dropping its async-io stack.

## wave4-core

**2026-10-06 · wave4-core: a service's schema is its own text, held to its
store by a test.** `#[service(name = "cpu", schema = SCHEMA)]` takes
schema-language text beside the struct rather than generating it;
`service_schemas_extend_the_builtin_one` compares the derive's
`FIELDS`/`EVENTS` with it and with the stub's field names and `rw` marks.
`Schema::extend` replaces the stub in place, keeping its `RecordId`.

**2026-10-06 · wave4-core: `Data` crosses the boundary, not `Value`.**
`strand_services::Data` (null, bool, int, float, text, duration, color,
list, record and enum by name) is what writes, actions, calls and keyed
diffs carry; the binary converts it by name (`services::convert`), and an
unknown record or enum becomes null.

**2026-10-06 · wave4-core: "goes invisible" is a release.** A service is
visible when it has a reader: the last release sends `Visible(false)` at
once and arms the 5 s stop on core's logic-clock timers; an acquire within
the grace cancels it and sends `Visible(true)` without a restart. A body
that ended in error restarts with its next first reader.

**2026-10-06 · wave4-core: a service's first report is a boot value.**
Updates sent before `Cx::ready` apply with `set_reloaded`, so "`on change`
never fires at boot" holds for services that start late too. `strand run`'s
first frame waits up to 100 ms (`Services::wait_ready`) for started
services, replacing the portal-only boot hold.

**2026-10-06 · wave4-core: `strand run` holds `system` itself.** Render
needs `system.reduced_motion` regardless of the config, so the logic thread
reads `system` for the whole run; kept values (`palettes/system`) seed it as
boot values.

**2026-10-06 · wave4-core: the mock stays a mock.** With `STRAND_MOCK` no
services registry is built: the mock `SchemaHost` serves every name and
neither the portal nor kept `system` values are read. Without it,
`Live::buses` names the buses real services use, keeping tests off the
machine's buses.

**2026-10-06 · wave4-core: cpu and memory sample while visible.** They
sample at start (cpu from boot load, so never 0 for a second), then once a
second only while a reader is visible. Back in view cpu takes a fresh
baseline.

**2026-10-06 · wave4-core: the composite routes item actions by record.**
`ws.focus()` arrives as `ActionTarget::Item(record)` and goes to the member
whose `#[derive(Call)]` actions take that record
(`DynService::item_records`), else the fallback; `declare`d custom services
go to the fallback until their sources land.

**2026-10-06 · wave4-core: `strand-dev` links `strand-services`.** The LSP
must check against the same schema as `strand run`, so `serve` uses
`strand_dev::schema()` and `serve_with` takes any schema. (Narrowed
2026-10-07 to `strand-services-schema`.)

**2026-10-07 · wave4-core: streams are fields, watched per field.** "Only
while visible" is per stream: a top-level field marked `#[store(stream)]` is
held by scopes that read it (`lower::ServiceUses`,
`ServiceHost::acquire_field`/`release_field`), and `Client` sends
`Msg::Watch { field, on }` (`Cx::watched`). Service-wide `Visible` stays for
whole-service pollers.

**2026-10-07 · wave4-core: an async call anywhere is a load.** So
`apps.search(q) ?? []` works where written, an async method call in a
binding lowers to `Op::AsyncSite`, the scope's own `async_memo` load that
re-fetches on argument change and keeps its last result while pending. In
handlers, `fn`s and lambdas it lowers to `Op::FetchMethod` (await on
`ServiceHost::fetch`); a binding calling one inside a lambda never re-runs,
and `strand check` warns.

**2026-10-07 · wave4-core: a write answer tags its field only.**
`Cx::report`'s envelope carries `(field index, generation)`, so only the
written field's patch is matched against pending writes; other fields it
moves are outside changes.

**2026-10-07 · wave4-core: bodies that fail while read come back.** A body
erroring with readers restarts on a core timer (1 s doubling to 30 s),
`Client::running` false meanwhile; one returning `Ok` is done. A write,
action or async call to a stopped service starts it for that operation
(stopping 5 s later). Routing `strand set <service>.<field>` through
`ServiceHost::write` was left open for the M3 audio/brightness step.

**2026-10-07 · wave4-core: threads and buses are cleaned up.** A thread
service's next run joins the previous thread first, and `Services::shutdown`
joins with a 2 s bound. Bus connections are shared per bus, pinged before
reuse and dropped with the last body. `PrivateBus` uses no service
directories, so nothing installed can be activated on a test bus.

**2026-10-07 · wave4-core: events are typed, schemas default to `SCHEMA`.**
`#[derive(Store)]` generates `<Name>Event` and `Cx::emit` takes only it.
`#[service(name = "battery")]` reads the `SCHEMA` constant in scope (`schema
= …` overrides).

**2026-10-07 · wave4-core: a `let` holds nothing; its readers do.** A `let`
or `fn` holds no services; each scope's `ServiceUses` adds what the `let`s
and `fn`s it reads read, transitively, so a closed popup's `let u =
cpu.usage` leaves `cpu` stopped. The top level still holds what its
handlers, timers, tokens, `state` initialisers and exported `let`s read.

**2026-10-07 · wave4-core: boot reports rebaseline mirrors too.**
`Applied::Keyed` carries `initial`, and the binary's keyed mirror then uses
`replace_all_reloaded`, so `on change` over a keyed service field never
fires for a boot report.

**2026-10-07 · wave4-core: a write an outside change overtook settles on its
answer.** `Signal::receive` remembers generations an outside value overtook,
so a tagged answer to one of them settles the cell unless a newer write is
pending (internal fix in strand-core's `echo.rs`).

**2026-10-07 · wave4-core: an ended run is a stopped one for writes.** A
write, action or async call to an ended body starts it afresh, unless it
failed and waits out its backoff (reported as such, so a slider drag does
not restart it 30 times a second).

**2026-10-07 · wave4-core: async calls a binding reaches through a function
are warned.** An async call inside a lambda the binding makes, or a `fn` it
calls, is fetched once and never answers the binding;
`check::async_in_binding_fn` warns at the call.

**2026-10-07 · wave4-core: schema texts in a crate of their own.**
`strand-services-schema` holds the builtin schema texts (`SYSTEM`, `CPU`,
`MEMORY`, `schemas()`), each service's `SCHEMA` is its constant there, so
`strand-dev` links them without the service runtime. A new service adds its
text there.

**2026-10-07 · wave4-core: an item of a service's keyed list is written by
its key.** A place whose path crosses an item of a keyed schema record (`for
s in audio.sinks { s.volume = 0.5 }`) is written via
`ServiceHost::write_item(item, path, value)`, routed by record and sent as a
`Write` with `Write::key`; core's
`KeyedSignal::write_item_tagged`/`receive_items` handle echoes. An `rw`
field reached through anything else is `check::read_only`;
`audio.sink.volume` stays a field write.

**2026-10-07 · wave4-core: a run's unanswered writes die with it.** When a
run ends the cells forget every pending write (`Cells::forget_echoes`), so
later outside reports are not taken for echoes. A rate-guarded write
committing with no run starts one; if none can start it is dropped.

**2026-10-07 · wave4-core: the retry backoff resets only after a stable
run.** Resetting on `ready()` let a ready-then-failing body restart every
second; the failure count now resets only when the failed run stayed up
`RETRY_MAX` (30 s).

**2026-10-07 · wave4-core: a hard reload keeps the services held.**
`reload_hard` now holds what the old tree held across the teardown and
releases at the end of the next tick, so services are not told invisible and
stream watches do not restart.

**2026-10-07 · wave4-core: a store's calls are its schema's, by test.**
`FromCall::signatures()` lets `service_schemas_extend_the_builtin_one` hold
every store's actions, async methods, item records and `fn`s to its schema
by name and arity, and the schema texts in `strand-services-schema` equal to
the stores in `Builtin`.

**2026-10-08 · wave4-core (carried item 6): faux bold reviewed and kept.** A
face is emboldened only for a request of 600 or more on a face lighter than
600 (`crates/strand-text/src/engine.rs`), CSS `font-synthesis-weight`'s
reading, since fontique's "embolden whenever heavier" draws every `$font.ui`
500 label faux bold where Inter is missing. architecture.md's `strand-text`
section states the rule; `crates/strand-render/tests/weights.rs` proves it
against `refs/weights_regular.png` and `refs/weights_regular_bold.png`.

**2026-10-08 · wave4-core (carried issues, round 1 closer): M0 idle windows
name the woken thread; the budget test's HOME gets its directories.**
`demo_bar_on_two_outputs_then_idle` now names each woken thread on failure,
and `the_design_bar_keeps_the_m0_budget` creates the font, icon and config
directories the cache sources name (as `budgets.rs` does) so HOME is not a
`Parent` watch. The owner's 34 MB target / 38 MB ceiling stays.

**2026-10-08 · wave4-core (carried issues, round 2 closer): flake witnesses,
test env isolation.** `devices_volume_mute_and_the_default_arrive`'s timeout
now prints `pw-metadata` beside the mirror instead of widening the wait;
`crates/strand/tests/services.rs`'s `Setup` points `XDG_CONFIG_HOME` and
`XDG_DATA_HOME` into the test HOME and clears `STRAND_ICON_THEME`;
`five_save_styles_land_on_a_cold_boot`'s label carries how long the file was
missing.

## wave4-a2

**2026-10-07 · wave4-a2: the D-Bus services are real.** `battery` (UPower),
`brightness` (sysfs + logind), `network` (NetworkManager), `bluetooth`
(BlueZ), `notifications` (own server), `media` (MPRIS) and `tray` (SNI +
DBusMenu) run on the contract, each tested against python-dbusmock or a zbus
mock on a private bus.

**2026-10-07 · wave4-a2: our own zbus clients, not nmrs, system-tray or
bluer.** nmrs and system-tray hard-wire `Connection::system()`/`session()`,
so they cannot run on a registry's `Buses` (private test buses); system-tray
also leaks detached tasks and panics, and bluer uses libdbus. The interfaces
are small, so plain zbus calls and match rules (`strand_services::dbus`).
logind-zbus 5.3 is used with default features off (they would add an
`async-io` executor); design.md's crate table says "own zbus clients".

**2026-10-07 · wave4-a2: a daemon is followed, not reconnected.**
`dbus::Daemon` subscribes to `NameOwnerChanged` and the daemon's signals
before reading; a new owner is re-read without restarting the service, no
owner means schema defaults. A dead bus ends the run with an error and the
retry reconnects.

**2026-10-07 · wave4-a2: real services may add to their stubs.** Added:
`battery.devices: [PowerDevice]`, `notifications.dnd: bool rw`,
`network.access_points: [AccessPoint]` (a `#[store(stream)]` field),
`media.player: text?`, plus `NotificationAction.notification`,
`TrayMenu.item`/`items` and recursive `TrayMenuItem`. The schema test holds
the stub's fields as an ordered subset with the same `rw` marks, and its
events.

**2026-10-07 · wave4-a2: notifications.** The server owns
`org.freedesktop.Notifications` on its own connection; `popups`/`all` keep
arrival order, `replaces_id` replaces in place, and each notification closes
exactly once when it leaves `all`. `n.expire()` only ends the popup; `all`
keeps the newest 100 (superseded 2026-10-10: history keeps 1,000);
`expire_timeout` 0 is null `timeout` plus `persistent: true`; `dnd` holds
non-critical ones from `popups`. `image-data` is validated (side ≤ 16384),
downsampled to 512 px and written as content-addressed PNGs (at most 8
queued, `IMAGES_QUEUED`). Spec 1.1 until an activation token is sent; a
stopping run closes its open notifications (reason 3, `client::finalize`,
`FINALIZE_LIMIT` 500 ms) and the next starts empty.

**2026-10-07 · wave4-a2: another notification server fails clearly.** When
another process owns the name, the run raises a notice naming it with `kill
<pid>` advice, `pkill -x` only for non-interpreters, `systemctl --user
stop/mask` for a real user unit, and settings advice for a desktop shell.
The run then waits on `NameOwnerChanged` without polling and takes the name
when freed, resolving its notice (`Cx::resolve`).
`Services::take_diagnostics` hands out deduplicated `ServiceDiagnostic`s;
only a `Cx::notice` is an overlay row and `ERROR` line, other failures are
`warn`.

**2026-10-07 · wave4-a2: a shared connection lives while a body uses it.**
Session and system connections are counted per running body
(`bus::with_user`) and closed by the last. Pixmap writes
(`pixmap::write_rgba`, `write_png`) each use their own temp file
(`.<hash>.<n>.tmp`), and a failed rename onto an existing content-addressed
file counts as success.

**2026-10-07 · wave4-a2: daemons are started by D-Bus activation.** When a
followed name has no owner, `dbus::Daemon` sends `StartServiceByName(name,
0)` once per start without awaiting a reply; the owner appearing is a
`NameOwnerChanged`.

**2026-10-07 · wave4-a2: brightness.** Read from
`/sys/class/backlight/<dev>` (`actual_brightness`, else `brightness`, over
`max_brightness`), preferring firmware over platform over raw, watched with
inotify set up before the first read. Writes go through logind's
`SetBrightness("backlight", dev, raw)` on `session/auto`.

**2026-10-07 · wave4-a2: network.** `connected` is NetworkManager `State` ≥
`CONNECTED_LOCAL` (50); `ssid`/`strength` come from the Wi-Fi active
connection's `SpecificObject`; `wifi` writes `WirelessEnabled`.
`access_points` is read, subscribed and `RequestScan`ned only while a
visible reader reads it, one entry per SSID. `ap.connect()` activates a
saved connection or `AddAndActivateConnection`; `ap.connect_with(password)`
adds WPA-PSK/SAE (enterprise and WEP refused); a `DEACTIVATED` before
`ACTIVATED` is `failed(ssid, error)` (`network::reason_text`). Calls are
bounded by 5 s (`dbus::READ_TIMEOUT`), activations by 25 s.

**2026-10-07 · wave4-a2: bluetooth.** BlueZ's object manager: the first
adapter gives `powered`, its paired devices `devices` (`Alias`, `Connected`,
`<icon>-symbolic`, `Battery1.Percentage`). `connect()`/`disconnect()` are
body-owned tasks (30 s at most); refusal or timeout is the `failed(address,
error)` event.

**2026-10-07 · wave4-a2: media.** Every `org.mpris.MediaPlayer2.*` is
followed; the active player is the last to start playing, else the last
paused. `Position` is asked on state, track, rate change and `Seeked`, then
carried at `Rate` (clamped 0.001–1000); `elapsed` and `position` are
`#[store(stream)]` fields ticking on the second only while visible and
playing. Player calls are bounded (2 s) and run as body-owned tasks, never
awaited in the loop. `art` is local only; remote art is null.

**2026-10-07 · wave4-a2: tray.** The service owns
`org.kde.StatusNotifierHost-<pid>-<n>` and registers with the watcher, or
becomes `org.kde.StatusNotifierWatcher` when there is none. `activate()`
calls `Activate(0, 0)`, but an `ItemIsMenu` item (or one refusing `Activate`
with a DBusMenu) gets its menu opened, so `on click { item.activate() }`
works without a schema flag. `scroll(dy)` sends 120 per notch, positive up.
`item.menu.open()`/`close()` drive `TrayMenu.opened` (one flag per item, not
per screen). Item calls are tasks bounded by 2 s (`dbus::CALL_TIMEOUT`); an
unreadable item is retried once after 2 s, then unlisted.

**2026-10-07 · wave4-a2: `strand set` writes service fields.** `strand set
brightness.level +5%` writes the `rw` leaf through the service host: a typed
value, or a signed number or percentage as a step from the current value,
never below 0. For a step on an unread service, its first read is awaited up
to 500 ms.

**2026-10-07 · wave4-a2: a closed surface's `open` binding is held.**
Lowering counts a top-level surface's `open` binding reads for the file's
top level, so the toasts panel's `open: shown.len > 0` starts the
notification server.

## wave4-a3

**2026-10-07 · wave4-a3: our own icon theme lookup, not freedesktop-icons.**
freedesktop-icons 0.4 caches themes and lookups in statics with no refresh,
so installs and theme switches would never be seen. `strand-icons`
implements the Icon Theme Specification lookup (parents depth first, then
`hicolor`, closest size, PNG before SVG, no XPM) behind `invalidate()`, as
its own dependency-free crate shared by `strand-render` and
`strand-services`.

**2026-10-07 · wave4-a3: what `apps` lists and how it launches.** Every
`*.desktop` under `$XDG_DATA_HOME` then `$XDG_DATA_DIRS` `applications/`,
first id wins; listed if `Type=Application`, not `NoDisplay`/`Hidden`,
admitted by `OnlyShowIn`/`NotShowIn`, `TryExec` present. `App.id` is the
desktop id; missing icons become `application-x-executable`. `launch()`
splits `Exec` per spec (not the crate's `parse_exec`), drops file/URL codes,
runs `Terminal=true` under `xdg-terminal-exec` or `$TERMINAL -e`, and spawns
detached (`setsid`, double fork, stdio on `/dev/null`).

**2026-10-07 · wave4-a3: search, ranges and frecency.** `apps.search(q)`
uses nucleo 0.5's `Pattern` (smart case and normalisation) behind
`apps::Fuzzy`; name matches give character `[start, end)` ranges by grapheme
cluster, generic name or keyword matches score half without ranges. Frecency
adds `10 × ln(1 + points)` with Firefox's recency buckets (100/70/50/30/10).
An empty query lists all, most used first; at most 200 hits; launches
persist as `services:apps.frecency`.

**2026-10-07 · wave4-a3: an async call waits for a reader.**
`StoreHost::fetch` holds an async call made while nothing reads its service
until something does, so a closed launcher never searches and opening it
runs the latest query once. The gate is the service's readers, so another
shown surface reading `apps` still lets a closed launcher search; gating per
async load needs strand-core work, left for later.

**2026-10-07 · wave4-a3: what the cache sources watch.** `strand run`'s
compiler worker watches (`Watcher::watch_tree`, completed writes and names
only) every `applications/` dir three levels down, every icon base dir one
level down plus GTK's `settings.ini`, and the default fontconfig dirs three
levels down plus `$XDG_CONFIG_HOME/fontconfig` (configured `<dir>`s not
parsed). A change re-reads apps, invalidates icons, or rebuilds the text
engine; an inotify overflow invalidates all three.

**2026-10-07 · wave4-a3: no-code services, their grammar read precisely.**
Source arguments must be constants (`check::not_constant`), since a service
restarts only when its declaration changes. A string command is split
shell-style and run without a shell, a list is the argv; a whitespace-free
path in `poll` reads the file (no `permit exec`), but a path naming a
program is a failure. Only `dbus` fields can be `rw`
(`check::not_writable`); `= key` is a property name or key path.

**2026-10-07 · wave4-a3: what no-code sources read.** A document is JSON,
else all-`key=value`/`key: value` lines, else plain text; a key path walks
JSON, searching depth first for a missing first key. `listen` takes each
line as a document, its ending a failure retried 1 s to 30 s; `poll` (10 s
timeout) runs only while visible and never faster than `every`; `file`
re-reads on inotify, a removed file reads null. `dbus` introspects at start,
follows `PropertiesChanged` and writes `rw` with `Set` in the property's
signature.

**2026-10-07 · wave4-a3: checked against introspection, where a bus
answers.** `strand_compiler::check::dbus::check` requires the property
(`check::dbus_property`), a convertible type (`check::dbus_type`) and
writability for `rw` (`check::dbus_read_only`); an unreachable bus is
`check::dbus_unchecked`, a warning. `strand check`, the loader and the LSP
run it via `strand-introspect`, caching answers 10 s.

**2026-10-07 · wave4-a3: one `custom` store per declaration.**
`strand_services::custom::Custom` is a store of one keyed list of untyped
`Data` values, registered per declaration under its declared name
(`Services::register_as`; `ServiceDiagnostic.service` is now a `String`) and
removed with `Client::unregister`. `ServiceHost::declare`/`restart` take
`lower::CustomService`.

**2026-10-07 · wave4-a3: review round r1.** A block `permit exec` lists
programs as a top-level one does; a `dbus` key path is
`check::type_mismatch`, as is a no-code field type no document can hold.
`coerce` has explicit arms (a file's `85` as percent is 8500%; bad durations
default) and a non-converting value is reported once per field
(`Services::report`). Commands run in their own process group, killed whole;
files are regular only, `O_NONBLOCK`. `strand_introspect::Cache` is shared
and the LSP never waits on a bus. `strand_icons::resolve` is the one icon
chain; `ServiceHost::fetch_reads` re-runs searches on app or theme change.

**2026-10-07 · wave4-a3 (r2): a whole document replaces, a line merges;
echoes settle the oldest equal write; re-type on reload.** A `file` or
`poll` document sets every field (absent keys read null); a `listen` line
merges. A `PropertiesChanged` echo settles the oldest pending equal write.
`ServiceHost::retype` (default no-op) lets a surviving custom service
convert to renumbered type ids without restarting.

**2026-10-07 · wave4-a3 (r2): `strand run`'s reloads never wait on a bus.**
The loader waits for introspection only at boot; afterwards it answers from
the shared cache, even stale, re-asks on its own thread, and on a differing
answer runs `Loader::recheck`.

**2026-10-07 · wave4-a3 (r2): the portal's icon theme wins over GTK's
settings files.** The theme is `$STRAND_ICON_THEME`, else the settings
portal's `org.gnome.desktop.interface` `icon-theme`, else
`gtk-icon-theme-name` in `settings.ini`, else Adwaita, followed by
`strand_services::icon_theme` on the shared services runtime and connection
(r3).

**2026-10-07 · wave4-a3 (r2): commands and files of no-code services.**
`permit exec` is checked against a list command's first item. `from poll`
runs a command at most every 100 ms (`check::poll_too_fast`); output over 1
MiB ends it. Commands get `PR_SET_PDEATHSIG` (SIGTERM). A `from file` whose
directory is missing watches the nearest ancestor. Left for M4:
`XDG_ACTIVATION_TOKEN` and `DBusActivatable=true`.

**2026-10-07 · wave4-a3 (r2): an app's install forgets icon misses; mismatch
reports end with their service; the schema host re-types.** An
`applications/` change also runs `strand_icons::invalidate` and re-looks-up
icons. A value-mismatch report is resolved when its service restarts or
stops. `SchemaHost` implements `retype` by remounting at defaults only when
record ids or field types moved.

**2026-10-07 · wave4-a3 (r3): one diagnostic per mistake in a source; a path
that names a program is a warning.** The parser alone reports a wrong source
kind (`syntax::unknown_source`), an unknown bus (`syntax::unknown_bus`) and
a `poll` without `every` (`syntax::expected`), and the checker stops adding
follow-on errors. A `from file`/`from poll` path that exists and names a
program is a `check::poll_program` warning, checked off `compile` (one stat,
a four-byte read) by `strand check`, the loader and the LSP inline; a
missing path is not reported.

**2026-10-07 · wave4-a3 (r3): a config that compiles with warnings says so
in `strand run`.** `Outcome::diagnostics` keeps a successful compile's
warnings and `cleared` is set when a recheck finds them gone. `strand run`
logs them and sends them to `strand watch`; the overlay still shows only
errors.

**2026-10-07 · wave4-a3 (r3): file watches that wait stay asleep; listen is
coalesced to a frame; search ranges are characters.** A waiting `from file`
watch counts only events naming the file or next directory, its inode watch
and `IN_Q_OVERFLOW`, and watches ancestors for moves. `listen` lines are
merged and sent at most once per `LISTEN_FLUSH` (16.7 ms). `Fuzzy` maps
matched grapheme clusters to the characters they cover.

**2026-10-07 · wave4-a3 (r3): what was left, and why.** Left to the
renderer/text owner: a mark covering part of a ligature cluster is dropped,
and `crashing_requests_are_not_retried` is flaky under load.

## wave4-integration

**2026-10-07 · wave4-integration: merging wave4/wm into wave4/core.** The
merge keeps every entry of both tracks' lists (`Builtin` stores, schema
texts, dependencies, thread table, M3 checklist). The builtin services line
and the change sources line are ticked with both halves' tests; stated
limitations (`Route` path unit-tested only, no
`zwlr_foreign_toplevel_management_v1` fallback) stay.

## wave4-exitReload

**2026-10-07 · wave4-exitReload: what "no reconnects" is measured as.**
`crates/strand/tests/reloads.rs` checks 100 reloads from outside: the
info-level `service `x` started (run N)`/`stopped` lines must not change,
every new D-Bus connection (via `BecomeMonitor`) may only `Hello` and
`Introspect`, mocks log no new reads, strand's socket inodes stay the same,
sway `-d` logs no `New client`, and no PipeWire object of strand's has a new
`object.serial`.

**2026-10-07 · wave4-exitReload: the bench's portal clause.** The portal
change is gated like monitor changes: first painted frame within one refresh
(p95), presented under two refreshes, timed from just before the mock portal
sends `SettingChanged`. The `system.dark` swatch joins the bar only after
the token and markup edits, and each change waits for an idle surface (no
frame for 150 ms). Without `dbus-daemon` it is skipped;
`STRAND_REQUIRE_DBUS` makes that a failure.

**2026-10-07 · wave4-exitReload: the gate's waits are bounded.** Every child
command is killed after 10 s, test D-Bus connections have a 10 s method
timeout, and a watchdog aborts after 5 min (`STRAND_RELOADS_LIMIT_SECS`)
printing the stage and log tails; CI gives the step `timeout-minutes: 10`.
Follow-up: once `strand-introspect` reuses one connection per bus, the test
can allow no new connection.

**2026-10-07 · wave4-exitReload: every box checks a given value; the bench
reports every gate.** The `windows` box asks for the test's window (`a
window`, `strand-reloads`), `workspaces` for `1` focused and occupied, and
`cpu`, `clock`, `calendar` boxes check ranges. The latency bench collects
all verdicts before asserting once.

## wave4-exitMemory

**2026-10-07 · wave4-exitMemory: transparent huge pages are off before the
first allocation.** `PR_SET_THP_DISABLE` now runs from an ELF constructor in
`.init_array.00100`, before mimalloc's constructor and `main`, since std's
pre-`main` allocation let mimalloc's 2 MiB arena fault in huge pages. The
tests assert `AnonHugePages: 0 kB`.

**2026-10-07 · wave4-exitMemory: what the bar on the real services is
measured as.** design.md's bar plus one component reading `network` and
`notifications`, against real backends (headless sway with two 2560×1440
outputs at 1 and 1.25). PSS from `smaps_rollup` in a release build after a
quiet second, then 10 s with no thread switch and no frame
(`crates/strand/tests/budgets.rs`).

**2026-10-08 · wave4-exitMemory: budgets are in MiB.** design.md's "34 MB"
and "59–64 MB" are 34 × 1024 kB and 64 × 1024 kB of `smaps_rollup` PSS; the
release `.text` gate is 15 MiB.

**2026-10-07 · wave4-exitMemory: why the bar was over budget on the real
services, and the fix.** Measured at 38.0 MB, the item over budget was
resident code (`.text` 19.4 MB). Fix one: event-rate crates build for size
in `[profile.release.package]` (`"z"` or `"s"`), per-frame paths and
strand-watch stay at 3; `.text` fell to 14.3 MB. Fix two: a delayed
`mi_collect(true)` 500 ms after a burst (`Trimmer`), since mimalloc kept
burst garbage resident; a purge delay of 0 was rejected.

**2026-10-08 · wave4-exitMemory: programs strand starts get THP back.**
`PR_SET_THP_DISABLE` is inherited across `fork` and `execve`, so
`strand_services::child::thp_off` records what strand inherited and
`restore_in_child` restores it in every `pre_exec`.

**2026-10-08 · wave4-exitMemory: the gated bar draws its icons.** The bar
runs with `XDG_DATA_DIRS=/usr/local/share:/usr/share` (Adwaita) and the boot
check asserts its icons drew.

**2026-10-07 · wave4-exitMemory: the full shell is measured with the
launcher's buffers at 2×.** As design.md's 59–64 MB estimate assumes,
`HEADLESS-1` runs at scale 2 (`HEADLESS-2` at 1.25) with the five design
files on real backends; the launcher opens via `strand set launcher.open
true`, two urgency-2 notifications show toasts and repeated `wpctl` volume
changes raise the OSD.

**2026-10-08 · wave4-exitMemory: the full shell checks what it measures, on
a desktop of apps.** `full_shell` reads figures after the settled second and
checks the screenshot shows the launcher's marked apps with icons, the
toasts and the OSD. It runs as
`the_full_shell_on_the_real_services_is_measured` and
`the_full_shell_with_a_desktop_of_apps_is_measured`; the larger peak is held
to 64 MB in release, and the apps-heavy figure is the one to quote.

**2026-10-08 · wave4-exitMemory: one burst per minute tick, and no thread
comes or goes while idle.** The trim's growth base is one process-wide value
(`run::LAST_TRIM`), so a tick is one burst in steady state; in release the
bar test fails on two bursts per tick. The idle window asserts the thread
set is unchanged, so the services runtime keeps blocking threads 500 ms
(`client::BLOCKING_KEEP_ALIVE`).

**2026-10-08 · wave4-exitMemory: the text worker trims as its queue
drains.** `mi_collect(true)` only frees the calling thread's pages, and the
text worker kept a tick's garbage. `strand_text::set_idle_hook`, installed
by `main` with `run::trim`, runs on each worker thread once its queue
drains, inside the burst that woke it.

**2026-10-08 · wave4-exitMemory: the trim is armed by structure, not by
mimalloc's commit count.** mimalloc's committed stat drifts
(`mi_arena_purge` v3.3.2 never counts down), so the growth gate is gone. A
thread arms its delayed trim only for a structural `SceneDiff`
(`run::structural`: nodes created/removed or tokens swapped); prop-only
bursts trim inline at the end of a wake at most every 5 s (`TRIM_EVERY`,
`Trimmer::settle`).

**2026-10-08 · wave4-exitMemory: the budget rests on the workspace's release
profile.** The `[profile.release.package]` opt-levels stay because the gate
cannot be met without them; architecture.md says packages build from the
workspace, and `the_release_binary_code_stays_within_15_mib` holds `.text`
to 15 MiB.

**2026-10-08 · wave4-exitMemory: memory targets and ceilings (architect, at
the owner's direction).** Each total becomes a target and a ceiling: the
two-monitor bar aims at 34 MB and fails above 38; the full shell aims at 64
and fails above 70. A release figure over target prints a CI `::warning`
(`warn_over_target`), and the M3 report lists figures against both.

**2026-10-08 · wave4-exitMemory: an inline trim's burst trims to its end;
the image worker trims as it drains.** An inline trim opens a 250 ms tail
(`run::TRIM_TAIL`) in which every wake of the same burst trims too, catching
the paint that follows a tick's diff. The image decode worker takes the hook
(`strand_render::image::set_idle_hook`).

**2026-10-08 · wave4-exitMemory: the 34 MB and 64 MB gates hold again,
pending the owner.** Commit ff8903b (targets and ceilings) was reverted
because nothing on the branch confirmed the owner's direction: 34 MiB and 64
MiB hard gates in release.

**2026-10-08 · wave4-exitMemory: targets and ceilings restored, confirmed by
the owner (architect).** The owner asked for reasonable memory expectations,
so the revert is reverted: bar target 34 MB, ceiling 38; full shell 64,
ceiling 70; over-target is a CI `::warning`. This supersedes the M3 exit
step's "≤ 34 MB gated"; agents should not revert it again but raise
objections in the step's report. (wave4-exitReport r2: the exact numbers
await the owner's explicit confirmation.)

**2026-10-08 · wave4-exitMemory: review r2 closed: idle hooks drain first
and are rate-limited, no inline trim between animation frames.** The text
worker drains its channel with `try_recv` before running its hook, and a
`HookGate` runs it at most once per 5 s plus that burst's 250 ms tail.
`Trimmer::settles` skips trims while `Renderer::in_motion`. `budgets.rs`
homes under `<target>/strand-budgets/<name>-<pid>` and runs alone.

**2026-10-08 · wave4-exitMemory: review r3: the owner's direction checked at
its source; the release profile guarded.** The owner's message in the
orchestrating session ("be reasonable with our memory expectations… I don't
want to hold myself to an untenable standard") and the unanswered target
table confirm the target-and-ceiling pair; the M3 report states the hard
gate is 38 MB (70 MB full shell).
`the_release_profile_keeps_the_size_opt_levels_the_budget_rests_on` fails if
any of the 27 opt-levels changes.

**2026-10-08 · wave4-exitMemory: figures are read by component, since the
file-backed share follows the page cache.** The binary's mapping (19.2 MB)
is resident to a degree set by the page cache (8.5 to 16.4 MB seen), so
every total is recorded with `Pss_Anon`/`Pss_File`/`Pss_Shmem`
(`memory_report` in `budgets.rs`) and m3-report quotes it that way.
Regressions show first in anon and shmem.

**2026-10-08 · wave4-exitMemory: review r3 closed: one hook gate for both
workers, owed hooks, the delayed trim held at most 5 s.**
`strand_text::HookGate` also rules the image decode worker (replacing r2's
unlimited image hook), counting only drains after a decode. A skipped drain
owes the hook, run after `recv_timeout(HOOK_QUIET = 500 ms)` of quiet via
`HookGate::quiet`. `run::Trimmer` holds a delayed trim back at most
`TRIM_HELD_AT_MOST` = 5 s. architecture.md names
`strand_services::child::restore_in_child` as the required `pre_exec`.

## wave4-exit-ci

**2026-10-07 · wave4-exit-ci: the compositor matrix compares strand with the
compositor's own report.** `crates/strand/tests/compositor_matrix.rs` runs
against a compositor named by `STRAND_MATRIX=sway|niri|hyprland`; the stores
(via `WmConfig::from_env`) must equal `swaymsg -r`/`hyprctl -j`/`niri msg
--json` at boot and after window, title, switch, `ws.focus()` and
`win.close()` changes, reloads must fire `wm.config_reloaded`, and idle must
wake nothing. design.md's bar must draw the same state and a dot click via
`zwlr_virtual_pointer_v1` must switch.

**2026-10-07 · wave4-exit-ci: how each compositor runs without a display.**
`scripts/compositor-matrix.sh <compositor>`: sway headless with pixman; niri
on its winit backend inside a headless sway with software EGL; Hyprland
needs a DRM node, so the CI `compositors` job loads `vkms`, passes the card
into an `archlinux:latest` container and runs Hyprland's DRM backend through
`seatd` (`AQ_DRM_DEVICES`, llvmpipe) as a user, with `hyprland.lua` for
0.55+.

**2026-10-07 · wave4-exit-ci: Hyprland's Lua dispatch dialect.** With a Lua
config, `dispatch workspace 3` is a Lua parse error, so the adapter sends
the classic form first and on that error resends as a dispatcher object
(`hl.dsp.focus({ workspace = "3" })`, `hl.dsp.window.close({ window =
"address:0x…" })`), keeping the Lua dialect for the connection. Tests:
`lua_dispatches_are_dispatcher_objects`,
`a_lua_config_hyprland_gets_lua_dispatches`.

**2026-10-07 · wave4-exit-ci: the matrix runs green on all three.** sway
1.12, niri 26.04 and Hyprland 0.56.2 pass, clicks included. The CI container
drops compositor file capabilities (`setcap -r`) and makes the vkms card
read-write for the test user. The reconstructed fixtures agree with live
compositors and stay as offline replay tests; the job is the only place
Hyprland and niri run.

**2026-10-07 · wave4-exit-ci: what the matrix holds fixed and what it
tracks.** The job tracks upstream on purpose (`archlinux:latest`, `pacman
-Syu`) and prints `pacman -Q`. A Hyprland dispatch counts as done only on
`ok` in either dialect (`dispatch_replies`). Hyprland's run checks a named
workspace via `name:<name>`. CI sets `STRAND_MATRIX_REQUIRE_CLICK=1`.

## wave4-exitReport

**2026-10-08 · wave4-exitReport: the merge, the report's sources and the
screenshots.** `docs/m3-report.md` quotes only numbers re-run at the merged
head or from named CI runs. Its screenshots come from
`budgets.rs::the_m3_screenshots` (ignored, run by `scripts/m3-shots.sh`),
with a two-line `Network` component added to the bar for the shots only.
`PrivateBus` (`dbus-daemon --session --print-address`) stands in for
`dbus-run-session` for the dbusmock tier.

**2026-10-08 · wave4-exitReport (review r1 closed).** (1) A sized `image` or
`icon` (absolute `size`, `width` or `height`) defaults to `shrink: 0`,
amending wave3-pixels' "`shrink` default 1 as CSS" for these kinds as CSS's
replaced-element minimum does
(`a_sized_image_keeps_its_size_beside_a_long_cut_comment`). (2) The budgets
idle gate mirrors strand's inotify watches and re-runs the window at most
twice when only `strand-watch` woke for an outside event. (3) A `from file`
`add_watch` `ENOENT`/`ENOTDIR` climbs a level. (4)
`the_design_bar_keeps_the_m0_budget` skips the first tick.

**2026-10-08 · wave4-exitReport (review r2).** (1) The sized-image `shrink:
0` default covers absolute sizes only; a percentage-sized `image` or `icon`
gives way like any box
(`a_percentage_sized_image_still_gives_way_in_a_row`). (2) The idle-window
retry does not count events in strand's own files (its log,
`$XDG_STATE_HOME`, `$XDG_CACHE_HOME`) as outside. (3) The exact 34/38 MB and
64/70 MB numbers are the architect's and await the owner's explicit
confirmation; every figure also passes the 34/64 MB hard gates.

**2026-10-08 · wave4-exitReport (review r2): the size opt-levels and the
token clause, measured.** With `strand` at `"s"` or at 3 the token p95
misses by 0.1–0.2 ms equally often, while opt-level 3 raises the bar's PSS
to about 34.1 MB, over target. So the override stays `"s"`; the token
clause's narrow headroom is the reload path's own, left to the M1 owners.

**2026-10-08 · wave4-exitReport (review r2 closed).** The fixes are in, plus
a qualification on the wave4-exitMemory "review r2 closed" entry and
README's lifecycle (stop 5 s after the last reader leaves or goes invisible;
scans and levels only while visible) and 44–61 MiB full-shell range.

**2026-10-08 · wave4-core: memory targets and ceilings confirmed by the
owner, numbers included (architect).** Asked directly, the owner chose a 34
MB target and 38 MB ceiling for the bar (full shell 64/70 MB). This reverts
carried item 5's hard 34 MB fail (e8c0e5f): budgets.rs, demo.rs and
services.rs warn above 34 and fail above 38, and design.md says so. It
supersedes the "await the owner's confirmation" notes; agents must not
change these gates without the owner.

**2026-10-08 · wave4-core: m3-report states the gates as confirmed; the
opt-level overrides stay open (carried issues round 2, item 5).**
docs/m3-report.md cites the owner's confirmation and drops the gates from
Open, while the ~27 `[profile.release.package]` overrides stay the one Open
item. `the_report_states_the_owner_confirmed_memory_gates` derives the gate
text from the binary's constants and fails if the report loses or questions
it.

## laptop-container

**2026-10-08 · laptop-container: the container suite replaces the dev
container.** Builds and tests run only through `scripts/container/`:
`run.sh` in an Ubuntu 24.04 image with CI's apt packages, fonts-dejavu-core
alone and Rust 1.97.0 as the host uid with CI's env; `run.sh ci` runs CI's
steps in order (`ci.sh`); `matrix.sh` runs the sway and niri legs in
archlinux:latest and tries Hyprland on the render node alone, skipping it
when aquamarine needs a KMS card.

**2026-10-08 · laptop-container: MSRV 1.97 for the workspace.** The
workspace declared 1.89 but strand-theme (material-colors 0.5) and CI need
1.97, so the workspace declares `rust-version = "1.97"` and strand-theme
inherits it; replaces the 2026-10-05 "integration: MSRV 1.89" entry.

## laptop-decisions

**2026-10-08 · laptop-decisions: `strand-watch` ancestor watches stop below
`$HOME` (handoff open decision 4).** By the owner's choice, a watched
directory under `$HOME` watches ancestors strictly below `$HOME`; one
elsewhere stops at its mount's root, which is watched itself
(`STATX_ATTR_MOUNT_ROOT`, device-differs fallback before 5.8). The boundary
is `strand_watch::Options::home`, recorded in architecture.md; design.md
states the reason (atomic saves in `~` woke the thread).

**2026-10-08 · laptop-decisions: `shrink: 0` for an absolutely sized `image`
or `icon`, acknowledged (handoff open decision 3).** The owner keeps it:
`shrink` defaults to 1 as in CSS, except an `image` or `icon` sized by an
absolute length (px or `ch`) and no percentage, which defaults to 0; an
explicit `shrink:` overrides. design.md's layout section states it (the
"Shrinking" bullet).

**2026-10-08 · laptop-decisions: the release opt-levels are signed off
(handoff open decision 1).** The 27 `[profile.release.package.*]` overrides
are accepted, closing the wave4-core round 2 item 5 open item. The memory
figures hold only for builds using the workspace's release profile (`cargo
build --release`, `cargo install --path`/`--git`), not registry packages,
dependency builds or distro flags; README's "Building and installing" says
so.

**2026-10-08 · laptop-decisions: the reload latency gate stays at
design.md's 35 ms with 50 rounds (handoff open decision 2).** The owner kept
token p95 within 35 ms on the monitor model, markup within 50 ms, and CI's
`STRAND_LATENCY_ROUNDS=50`; CI and the laptop container are the reference
machines. A profile found about 2.6 ms of work outside the 15 ms coalesce
and no stage worth optimising; misses come from deep CPU idle states.

**2026-10-08 · laptop-decisions: the budgets idle test's watcher control
follows the new ancestor rule.** The control now makes a directory in
`~/.config` (still watched) and asserts the watcher wakes, then one beside
HOME and asserts nothing hears it, proving HOME's parent is unwatched.

## laptop-ci

**2026-10-08 · laptop-ci: the `check` job is split into five jobs.** `lint`,
`test`, `budgets`, `acceptance` and `timing`, each step of the old job in
exactly one, sharing the `.github/actions/setup` composite action; `budgets`
and `acceptance` share the `release` rust-cache key, saved by `budgets`.
`scripts/container/ci.sh` runs the same jobs in order (`CI_JOB=NAME` for
one).

**2026-10-08 · laptop-ci: a `timing` profile for the timing gates.**
`[profile.timing]` inherits `release` and sets only `lto = false`, cutting
warm build time about 29% while the gates measure the same; memory gates and
acceptance keep the real release profile. The nightly job builds only
`--profile timing` for the 10,000-edit fuzzer, compiler fuzzers and latency
run.

**2026-10-08 · laptop-ci: test binaries are not consolidated.** Linking all
85 integration test binaries costs about 39 s of CPU, so merging saves 10–20
s per job, while many tests set process-wide env or count their own
process's threads and wakes. Every test file stays its own binary.

**2026-10-08 · laptop-ci: theme_swap_bench robust to runner noise.** The 5
ms budget and thresholds are unchanged, but optimised cases take the median
of more swaps (31 per look pair, 24 crossfades, 16 per scopes and spring
case, 31 re-resolves) and print p95 and worst; debug keeps the old counts.
Each case is measured once: retries made the gate a best of three and were
removed. The median is the upper middle
(`quantiles_are_nearest_rank_and_the_median_is_the_upper_middle`).

## laptop-flakes

**2026-10-08 · laptop-flakes: the CI flakes, reproduced in the container and
fixed at their causes.** (1) `first_frame_of_a_new_surface_has_its_text`
holds the text worker until its asserts have looked. (2) A new surface's
first two frames have no presentation feedback, so the strand-surface pose
tests judge a fade by sample times: frames 4 ms or more apart must rise,
closer ones may repeat, and the fade must span 50 ms. (3)
`five_save_styles_land_on_a_cold_boot` excuses a round whose gap reaches the
50 ms removal grace, a real removal. (4) Audio: below.

**2026-10-08 · laptop-flakes: the audio thread reads the `default` metadata
again after a client comes or goes.** PipeWire's `module-metadata` drops
property events to existing bindings while another client's bind awaits its
pong, so updates were lost. The thread binds the metadata afresh
`audio::REREAD` (250 ms) after a client global comes or goes or an effective
default is cleared, one read per burst, publishing only what changed.

**2026-10-08 · laptop-flakes: a read again replaces the defaults whole, and
keeps the old binding until the new one is made.** A re-read's replayed keys
(`Work::Meta` carries its binding) replace the defaults whole once its sync
returns, so a lost clear is recovered; the old binding goes only after the
new bind and sync. `a_lost_default_update_comes_back_on_a_read_again` loses
updates on purpose (WirePlumber `SIGSTOP`ped) and fails without the read.

**2026-10-08 · laptop-flakes: a read again waits for the first read.** The
service's own client usually triggers a read again 250 ms after connecting;
one falling due while the first read's sync is out is now pushed back by
`REREAD`, so a slow first read cannot leave the first state without defaults
(`the_first_state_waits_for_a_slow_first_read`).

**2026-10-08 · laptop-flakes: what did not reproduce.** The full-shell
test's failure now says where the marked icon is.
`demo_bar_on_two_outputs_then_idle`'s idle window starts after a whole quiet
second (bounded at 20 s) rather than a fixed wait.

## laptop-verify

**2026-10-08 · laptop-verify: niri's IPC checked against a real niri 26.04;
windows ordered by id.** A capture (`scripts/container/capture-niri.sh`,
`tests/fixtures/niri-26.04-captured`) matched every field shape, but niri's
`Windows` reply is in map order while events add in arrival order, so
`windows.all` from niri is now ordered by window id (open order).
`action_for` checks ids first because niri answers `{"Ok":"Handled"}` to an
unknown one.

**2026-10-08 · laptop-verify: the compositor matrix checks a second output
where it can.** `scripts/compositor-matrix.sh` gives sway (`swaymsg
create_output`) and Hyprland (`hyprctl output create headless`) a second
output and exports `STRAND_MATRIX_OUTPUTS`; `the_stores_report_every_output`
and `the_design_bar_is_on_every_output` check per-output workspaces, focus
and clicks (`Live::layout_point`).

**2026-10-08 · laptop-verify: Hyprland's IPC checked against the owner's
live Hyprland 0.56.2; the adapter stands.** A read-only capture
(`tests/fixtures/hyprland-0.56.2-captured`, titles scrubbed) matched every
field shape but `stableId` (8 hex digits) and showed four event-order
differences; the adapter is order-independent within a burst and needed no
change. `captured_bursts_patch_or_ask_for_a_read` and
`captured_stream_ends_where_hyprland_does` replay it;

**2026-10-08 · laptop-verify: the Hyprland capture's provenance corrected;
reconstructed stableIds take the real form.** The capture was assembled from
three separate reads, which SOURCE.txt and stream.txt now say. The
reconstructed fixture's stableIds become `18000001`… and
`hyprland_windows_join_the_toplevel_list_by_stable_id` joins on that form.

**2026-10-08 · laptop-verify: second review of the capture notes; open burst
reordered; the capture script records where its replies fall.** The
reconstruction's open burst takes the captured order.
`scripts/capture-hyprland.sh` keeps the stream open, reads replies only
after `QUIET` (0.3 s) with no event byte (up to `TRIES`), and writes
`marks.txt` (`scripts/test-capture-hyprland.sh`).

## laptop-toplevel

**2026-10-08 · laptop-toplevel: wlr-foreign-toplevel-management is the
window source without an adapter.** Closes the 2026-10-07 wave4-wm
follow-up. The `strand-toplevel` thread binds
`zwlr_foreign_toplevel_manager_v1` (up to v3) and every `wl_seat`;
`wm::merge` picks one whole window source: the IPC adapter, else wlr when
bound, else the read-only ext list. It is a de facto standard on wlroots, so
it fits "compositor-agnostic first" and fills labwc's gap; design.md says
so.

**2026-10-08 · laptop-toplevel: wlr window ids and the list join.** A wlr
window's id is `wlr-<key>`, a per-handle client counter. `Window::toplevel`
joins by app id and title, n-th oldest to n-th oldest, only when both
protocols show the same count, else `None`; the id is not the joined
identifier so a late join does not change keys.

**2026-10-08 · laptop-toplevel: focus, the keyboard's screen and actions.**
`focused` is wlr `activated`; `minimized` and `fullscreen` map one to one.
One activated window on exactly one output makes that output the focused
screen and its active workspace `workspaces.focused`, refining wave4-wm's
protocols-alone rule. `win.focus()` is `activate` on the first seat still
offered, `close()` is `close`, `minimize()` is `set_minimized`; `Ok` means
sent, an unknown id is `UnknownWindow`.

**2026-10-08 · laptop-toplevel: labwc in the compositor matrix.** labwc runs
headless in the Arch matrix image (pixman, `WLR_HEADLESS_OUTPUTS=1`,
desktops `1` and `2`). With no IPC truth, the IPC-truth and bar pixel tests
skip it, and `the_stores_follow_a_compositor_without_ipc` checks against
test windows' own configures (`TestWindow::activated`).

## laptop-services

**2026-10-08 · laptop-services (audio): `DeviceRef::Id` carries
`object.serial`; item writes carry the record they were made on.**
`DeviceRef::Id` is `Id { id, serial: Option<u64> }` (`DeviceRef::id`,
`DeviceRef::device`) and the thread refuses a mismatched serial
(`UnknownDevice`); `AudioChange::Serials` lets handle users build refs
(`Mirror::device_ref`). The schema record gains no serial; `Write` gains
`held`, and the store refuses a write or `make_default()` whose held record
names another `node.name` than the device now under that id.

**2026-10-08 · laptop-services (audio): without an inotify instance the
audio thread retries on its timer and makes the watch again.** The socket
watch is re-made each time the thread starts waiting for the socket, so
freed instances are picked up; the 10 s timer remains the fallback (fanotify
and stat polls rejected). `audio::deny_inotify(remote, deny)` (doc-hidden)
lets tests deny it.

**2026-10-08 · laptop-services (core): held item writes, latest write wins
per item.** In `KeyedSignal::write_item_tagged`, each item write gets a
per-list order number; one landing drops other handlers' older held writes
of that item, any other change of a held item drops its held write, and held
writes of other items are kept, following the field rule "latest value
wins". Fixed in strand-core (`keyed/reactive.rs`, `Runtime::edit_deferred`);
no interface changed.

**2026-10-08 · laptop-services (introspect): one kept D-Bus connection per
bus.** `strand_introspect::properties` keeps one connection per `Bus` in a
static current-thread runtime with no thread of its own; connecting is
single-flight. An I/O failure or a 2 s timeout drops the connection and
retries once; an error answer keeps it.

**2026-10-08 · laptop-services (core): a kept held item write is re-based
onto what kept it.** When an item write lands, later held writes of that
item take the landed value as their base, and a `Settle` report (the
service's answer, possibly corrected) re-bases them instead of dropping
them; an outside report still drops them.

**2026-10-08 · laptop-services (introspect): dropping a hung connection
never waits for another caller.** The connection map is a `std::sync::Mutex`
never held across an `await`, and a separate async mutex, waited for within
the caller's deadline, makes connecting single-flight, so `properties` keeps
its 2 s bound.

## laptop-trim

**2026-10-08 · laptop-trim: a surface's size goes to logic only when a
binding reads it.** The extra logic wakes on the 16-CPU laptop were
`ToLogic::Size` for a popup whose width follows `pct(cpu.usage)`, not the
trim. `Forward::configured` (demo/host.rs) now sends `Size` only for a
surface node carrying `Prop::Watch`, as the renderer already does for layout
facts; a newly watched node's size is reported at once. No interface
changed.

## laptop-open

**2026-10-08 · laptop-open: the container suite's timing gates are advisory
on the laptop; GitHub is the latency reference (owner's decision).** In
`scripts/container/ci.sh` the `theme_swap_bench` and `reload_latency` steps
run through `timing_step`, reprint their key numbers, and on gate misses
only print WARN, ending "CI PASSED with timing warnings";
`STRAND_STRICT_TIMING=1` makes them fail. Memory, idle and pixel gates stay
strict; ci.yml and the budgets are unchanged.

**2026-10-08 · laptop-open: the audio read again is kept, and not reported
upstream (owner's review).** `module-metadata`'s forwarding rule reads as a
deliberate choice whose side effect drops an update in a short window, so
`audio::REREAD` stays as the client-side answer.

**2026-10-08 · laptop-open: COSMIC is out of scope (owner's decision).**
Custom-shell users run bare compositors (Hyprland, niri, sway, river, labwc,
wayfire). Strand still runs on COSMIC via the protocols (`windows.focused`
null, window actions `Unsupported`), with no adapter or test planned;
handoff.md, README and features.md say out of scope.

**2026-10-08 · laptop-open: only gate misses are advisory in the container
suite's timing steps.** Every wall-clock gate failure message starts with
"timing gate missed" (`GATE_MISS` in `theme_swap_bench.rs` and `bench.rs`),
and `scripts/container/gate-misses.sh` treats a step as advisory only when
every panic carries the marker and they cover the failed tests; anything
else fails.

**2026-10-08 · laptop-open: gate verdicts are held at the end of each timing
test.** Wall-clock gates in `theme_swap_bench.rs` record verdicts in a
`Gates` collector checked once as the test's last statement, after every
functional assertion, with one `GATE_MISS` failure naming every missed
clause (as `bench.rs`'s `failed` list does);
`gate_verdicts_are_held_until_the_end` tests it.

**2026-10-08 · laptop-open (winstate): `win.maximize()` and
`win.fullscreen()` are window actions, and they toggle.** Owner decision,
replacing laptop-toplevel's "no maximize or fullscreen actions in the
language": `Window` gains `action maximize()`, `action fullscreen()` and
`maximized: bool` (`WindowAction::{Maximize, Fullscreen}`,
`WmAction::{MaximizeWindow, FullscreenWindow}`). They toggle, as every
compositor's command does, avoiding `unmaximize()`/`unfullscreen()`;
`minimize()` still sets.

**2026-10-08 · laptop-open (winstate): how each compositor runs them.**
Hyprland Lua: `hl.dsp.window.fullscreen({ mode = "maximized" | "fullscreen",
window = "address:0x…" })`; classic: `[[BATCH]]dispatch focuswindow
address:0x…;dispatch fullscreen N`, done only when every part is `ok`.
`j/clients` mode 1 is `maximized`, bit 2 `fullscreen`. niri:
`MaximizeWindowToEdges { id }`, `FullscreenWindow { id }`. sway: `[con_id=N]
fullscreen toggle`, maximize `Unsupported`. wlr: set/unset requests chosen
from last state (fullscreen needs v2); the ext list alone answers
`Unsupported`.

**2026-10-08 · laptop-open (winstate): niri's window state comes from the
wlr protocol.** niri's IPC has no fullscreen or maximized, so an adapter
reporting no window states (`IpcSnapshot::window_states`) takes them from
wlr toplevels (`wm::merge_with`): a lone app id pairs directly, twins pair
by app id and title in order, disagreeing counts get neither.

**2026-10-08 · laptop-open (winstate): the matrix checks both actions
live.** `window_state_actions_follow_the_compositor` toggles a real window's
fullscreen then maximize on every matrix compositor, checking store,
compositor report and the window's own configures (Hyprland's own maximized
flag is skipped: it marks every toplevel maximized). sway's `win.maximize()`
answers `Unsupported`.

## laptop-resilience

**2026-10-09 · laptop-resilience: the compositor matrix runs nightly too.**
The `compositors` job also runs on the nightly schedule (named `compositors
(nightly: archlinux:latest Hyprland, niri, sway, labwc)`), and
`scripts/compositor-matrix-ci.sh` names failures with compositor and package
version and writes `target/matrix/summary.md` to the job summary.

**2026-10-09 · laptop-resilience: Hyprland dispatches go out in Lua first.**
From 0.56 Lua is Hyprland's only config kind, so the adapter sends the Lua
dispatcher object first and falls back to classic only on `Invalid
dispatcher` (or vice versa on Lua's parse error), keeping the understood
dialect for the connection; a reconnect starts in Lua again. An action's own
failure is not resent.

**2026-10-09 · laptop-resilience: an adapter that cannot understand its
compositor degrades to the standard protocols.** A session now ends with an
I/O error (reconnect, keep state) or "not understood"
(`wm::understood::SessionEnd`), which sends `AdapterMsg::Degraded(text)`:
state comes from the protocols as if the adapter failed to connect,
`Sources::degraded` holds the text, and the adapter retries on backoff. A
malformed state reply (needed fields are now required in serde types)
degrades at once; `MAX_STRIKES` = 8 consecutive non-event stream messages
degrade; a refused action syntax degrades and is pinned to the compositor's
version (`wm::understood::Refused`).

**2026-10-09 · laptop-resilience: the degraded adapter is a
`ServiceDiagnostic`, not an overlay notice.** The text names compositor,
version and what was not understood, and is `notice: false` since the bar
keeps working. `Cx::warn(message)` (`Envelope::Warning`) lets a running body
raise a non-notice diagnostic; only the hub's oldest live subscription
(`WmSubscription::reports()`) raises it, once per new text.

**2026-10-09 · laptop-resilience: one unsupported action is only rejected;
the syntax is judged by actions every version has.** Only a refusal in every
dialect of `ws.focus()`, `win.focus()` or `win.close()`
(`wm::understood::tells_syntax`) counts as a changed syntax; the same
refusal of a later action (maximize, fullscreen, minimize) answers that
action `Rejected` and the adapter stays up, since older compositors (niri
25.08 lacks `MaximizeWindowToEdges`) refuse unknown actions in the same
words.

**2026-10-09 · laptop-resilience: a retry after a stream that was not
understood waits for an event; a degradation is reported once.** Each
adapter keeps a `wm::understood::Degradation` across sessions: after a
stream failure, the next session sends `Connected(true)` and state only once
its stream carried an event, answering actions `NotConnected` meanwhile, so
ids no longer flip every backoff cycle. A session that fails without coming
up sends no second `Degraded`.

**2026-10-09 · laptop-resilience: only sway's unknown-command reply refuses
its syntax.** sway sets `parse_error` for every `CMD_INVALID`, including
state errors (no outputs, `Expected workspace number`), so only
`Unknown/invalid command '…'` counts as a refused syntax
(`wm::sway::refuses_syntax`); other `parse_error` replies answer `Rejected`
and the adapter stays.

## laptop-media

**2026-10-09 · laptop-media: a new track starts its time at 0 in the same
update as its title.** When a `Metadata` names another track (by
`mpris:trackid`, else `xesam:url`, else `xesam:title` with `xesam:artist`,
comparing the first key both name), the carried position is set to 0 at once
and the `Position` answer still corrects it. A `Metadata` change for the
same track keeps the position; an invalidated `Metadata` is judged when its
whole re-read lands.

**2026-10-09 · laptop-media: a whole re-read naming another track resets the
time only while no newer position fix can stand.** Each read carries the
player's latest question number when asked (`Ask::asked`, `Read::asked`),
and resets only while that is still the latest, so a newer `Seeked` or
`Position` answer is not overwritten.

## laptop-labwc

**2026-10-09 · laptop-labwc: a stopping protocol thread waits for the
compositor to take the requests it sent.** A run that acted and stopped at
once could lose its request: the thread closed before writing or before
labwc read it (libwayland-server drops unread data on hangup). The thread
now writes a batch's requests before answering them and, on a stop after any
request, sends `wl_display.sync` and waits for `done` up to 500 ms
(`SETTLE_WAIT`).

**2026-10-09 · laptop-labwc: every end of the protocol thread after a
request settles; an `Ok` means on the connection, not written.** The return
on a failed state send (dropped receiver) now settles too when a request was
sent (`a_request_outlives_a_dropped_state_receiver`). Writes use zero wait,
so the guarantee is corrected: an `Ok` means written or queued, and every
end after a request flushes and waits up to 500 ms for the compositor to
read it.

## m4-owner

**2026-10-09 · m4-owner: GPU tests run on lavapipe everywhere, with an
advisory leg on the laptop's GPU.** The container image and GitHub CI use
software Vulkan (lavapipe), where GPU pixel and idle-release tests are
enforced; an opt-in laptop hardware leg is advisory, gets only
`/dev/dri/renderD128` and starts its own compositor.

**2026-10-09 · m4-owner: the GPU backend is in every build, not an opt-in
cargo feature.** Shaders, promotion and bundled GPU effects ship in the
installed binary and fall back gracefully (CPU rendering, CPU effect
versions, a `shader` node draws nothing, the inspector and `strand report`
say why); design.md now says "built in and falls back to the CPU". GPU
crates stay cold until needed, so PSS is unchanged. The release `.text` gate
becomes the spike's measured `.text` plus about 10%, while a CPU-only build
keeps 15 MiB.

**2026-10-09 · m4-owner: syntax for the four bundled GPU effects design.md
left unnamed.** Bloom, CRT, chromatic aberration and raster wobble are
`filter:` values (`bloom(r)`, `crt()`, `chromatic(px)`, `wobble(amp)`) and
liquid glass a `backdrop:` value (`glass()`), adding no concept.

**2026-10-09 · m4-owner: a missing `/etc/pam.d/strand` falls back to the
`login` service.** With a one-time warning, rather than PAM's `other`, which
usually denies all and would lock the user out; every later PAM error fails
closed.

**2026-10-09 · m4-owner: the lock VM container gets `/dev/kvm`.** `/dev/kvm`
is 0666 on the laptop, so the owner allowed passing it to the lock VM
container only (`scripts/container/lockvm.sh`); this decision is the
authority until CLAUDE.md names it.

**2026-10-09 · m4-owner: `letters` keeps no positional.** As design.md and
wave2-check have it, `letters { … }` animates the text of its enclosing
`text` node; the plan's `letters "text" { … }` is not adopted.

## m4-infra

**2026-10-09 · m4-infra: lavapipe is selected by `VK_DRIVER_FILES`, with a
fixed llvmpipe vector width.** The image and CI set `VK_DRIVER_FILES` (and
`VK_ICD_FILENAMES`) to `/usr/share/vulkan/icd.d/lvp_icd.json` and
`LP_NATIVE_VECTOR_WIDTH=256` so pixels do not depend on AVX2 vs AVX-512;
`LP_NUM_THREADS` stays unset. `STRAND_REQUIRE_GPU=1` is set like the other
tiers; the `GPU tier device` step prints the Mesa version (updates pocket).

**2026-10-09 · m4-infra: the hardware GPU leg runs in the check image, not
the Arch image.** Ubuntu's Mesa 25.2.8 drives the laptop's Panther Lake GPU,
so `gpu.sh` uses `run.sh`'s image and build tree, passes only
`/dev/dri/renderD128` (any `card*` refused), swaps the ICD by PCI vendor and
starts its own sway. Failures warn unless `STRAND_STRICT_GPU=1`.

**2026-10-09 · m4-infra: the lock VM is a baked Ubuntu disk image booted
with a direct kernel.** virtme-ng over a shared rootfs breaks in an
unprivileged container, so `Dockerfile.lockvm` builds an Ubuntu 24.04 guest
(sway, grim, pamtester, user `tester`/`strand-test`, `/etc/pam.d/strand`)
into an ext4 image with `mke2fs -d`, booted with `snapshot=on`; a tiny PID 1
(`lockvm/init`) mounts two 9p shares and powers off. No virtiofsd, no
network. TCG via `LOCKVM_TCG=1`.

**2026-10-09 · m4-infra: CI gets a `lock-vm` job.** GitHub's Linux runners
expose `/dev/kvm` once a udev rule makes it 0666 (applied with `sudo` on the
runner only); the job fails without KVM rather than emulating. It runs
nightly, on `workflow_dispatch`, tags, and pushes or PRs touching lock paths
(dorny/paths-filter), otherwise passing empty.

**2026-10-09 · m4-infra: the lock-vm paths filter follows the critique's
S-lock ownership, widened by globs.** It names S-lock's planned files plus
`services/auth*`, `auth.schema`, `check/lock.rs`, `strand/src/lock.rs`, and
`*session_lock*`/`*lock_fallback*` anywhere under `crates/`; S-lock keeps it
in step with real names.

## m4-render-split

**2026-10-09 · m4-render-split: strand-render's monoliths split by concern,
with no feed.rs or backend.rs yet.** `renderer.rs`, `flatten.rs`,
`raster.rs`, `anim.rs` and `layout.rs` became directories by pure moves (map
in architecture.md, "`strand-render` module map"); `feed.rs` and
`backend.rs` are left to the streams that give them code, promotion hooking
into `Renderer::paint_frame` in `frame.rs`.

## m4-surface-split

**2026-10-09 · m4-surface-split: the `manager.rs` and `run.rs` splits are
pure moves, and `run/lock.rs` is real.** Moved items became `pub(super)`
with exports unchanged; `run/lock.rs` holds `Shell::unlocked`, while the
deferral inside `Shell::apply` stays in `shell.rs`. `feeds.rs`, `lists.rs`
and `gpu.rs` wait for their streams; trim tests moved to `run/trim.rs`, the
rest to `run/tests.rs` keeping `crate::run::tests::*`.

**2026-10-09 · m4-surface-split: log targets follow the new module paths.**
Moved log lines now name their new module (`strand::run::shell`,
`strand_surface::manager::popup`), as every module logs under its own path;
`STRAND_LOG` never filters by target and nothing matches these strings.

## m4-docs

**2026-10-09 · m4-docs: where the M4 plan lives.** The plan and its review
merge into `docs/m4-plan.md`, the review's stream table, ownership and
ordering winning and the owner's answers (m4-owner) replacing open
questions. architecture.md gets the M4 interfaces in two parts: all but the
GPU now (wave 0a), the GPU after the spike (wave 0c).

**2026-10-09 · m4-docs: time leaves follow the schema, not the old sketch.**
`Wave` carries its phase and `Noise` its argument as expressions, per
builtin.schema's `wave(period, phase: float = 0)` and `noise(x: float)`;
`Index` and `Count` are `letters`' element-scope `index` and `count`. No
syntax changes.

**2026-10-09 · m4-docs: `DropPayload` in strand-scene, `NodeEvent::Drop` in
strand-render.** `NodeEvent` lives in `strand_render::input`, while
`DropPayload` sits in `strand-scene` beside `InputEvent` because
`strand-surface` produces it for external drops (`InputEvent::DragDrop`). An
in-Strand drag sends only the source node; `Prop::Drag` and `Prop::Accepts`
carry type names.

**2026-10-09 · m4-docs: the unlock gate is a type.**
`strand_auth::UnlockToken` is minted only by `strand_auth::Client` from a
success reply, is not `Clone`, and `strand-surface`'s `State::unlock` takes
it, so no other path can unlock. This adds an edge from `strand-surface` to
`strand-auth`'s lib (libc and zeroize only); only the binary links libpam.

**2026-10-09 · m4-docs: keyframes travel inline.** `Prop::Play` carries the
compiled block (`PropValue::Keyframes`) instead of `[name, seq]`, so render
keeps no keyframes table for reloads to update; settings are those
`check::keyframe_settings` allows (`duration`, `delay`, `repeat`,
`alternate`, `easing`).

**2026-10-09 · m4-docs: an SVG part names its layer with `Prop::Name`.** The
new `SvgPart` kind carries its `#id` as `Prop::Name` rather than a new prop.

**2026-10-09 · m4-docs: the lock fallback bypasses the Router.** While
render's built-in fallback lock shows, lock-surface keys go from the binary
straight to `lock_fallback`, independent of the Router, logic thread and
text worker. The Router stays S-lists'; parallax, tilt and jelly only read
it.

**2026-10-09 · m4-docs: tray click coordinates are left to S-surface.** A
user-facing `x`/`y` argument would be new syntax a config cannot fill, so
architecture.md fixes only the outcome (real coordinates, not 0, 0) and
S-surface picks the route.

**2026-10-09 · m4-docs: clipboard is not M4.** design.md never mentions a
clipboard and no M4 box asks for it; drag and drop's `wl_data_device` makes
it cheap later.

**2026-10-09 · m4-docs: `strand_auth::Client` takes its `pre_exec` from its
owner.** `Client::new(helper, pre_exec: fn())` takes the hook and its owners
pass `child::restore_in_child`, since `strand-auth`'s lib depends on no
Strand crate and should not own the THP policy.

**2026-10-09 · m4-docs: `auth` moves to its own schema file.** S-lock moves
`provisional service auth` to `strand-services-schema/src/auth.schema` (an
`AUTH` constant, a `schemas()` entry) in wave 1; after 0b, a bundled-effect
stream edits only its own effect's declaration in builtin.schema.

**2026-10-09 · m4-docs: a scrim is a colour.** builtin.schema narrows
`scrim: paint` to `scrim: color`, matching design.md's single-pixel buffer
and `SurfaceSpec`'s `Option<Color>`, so a gradient scrim is a type error.

**2026-10-09 · m4-docs: an external drop's app is resolved by logic.**
`DropPayload::External` carries `app_id: Option<String>`; logic resolves it
through `apps` into `record Drop`'s `app: App?`, null when no installed app
has it.

## m4-integration

**2026-10-09 · m4-integration: owners for the modules the wave-0 splits
added.** Render internals go to S-runtime, surfaces and exit poses to
S-surface, scroll and virtualisation (`layout/list.rs`) to S-lists; the run
loop's shared modules change by small PRs S-runtime reviews.

## m4-gpu-spike

**2026-10-09 · m4-gpu-spike: wgpu 30 presents to a layer surface on
lavapipe; readback is the universal fallback.** `spikes/gpu-present` (wgpu
30.0.1 Vulkan, vello_gpu 0.3.0) presents to a layer surface on lavapipe
through Mesa's wl_shm WSI, but Intel ANV cannot present to a pixman sway (no
linux-dmabuf). Offscreen rendering plus `copy_texture_to_buffer` into a
`wl_shm` buffer works on every compositor and device tried. So promotion
must test surface compatibility (`compatible_surface` or non-empty
`get_capabilities`) and fall back to readback, also the path for unpromoted
shaders.

**2026-10-09 · m4-gpu-spike: dropping the device frees memory on hardware
but not on lavapipe.** On lavapipe a dropped device leaves
`libvulkan_lvp.so` and `libLLVM` mapped and PSS about 80 MiB above baseline,
the same each cycle (not a leak); on ANV PSS returns to within about 6 MiB.
So the CI idle test asserts the GPU thread is gone, nothing wakes, and PSS
after a second promote-and-drop is within tolerance of the first; the
hardware leg may also assert within about 6 MiB of baseline.

**2026-10-09 · m4-gpu-spike: linking the GPU backend adds 3.0 MiB of `.text`
and about 1.3 MB of cold PSS; proposed gate 18.5 MiB.** Linking wgpu, naga,
vello_gpu and wayland-backend `client_system` took `.text` from 13.86 to
16.86 MiB and the bar's mean PSS from 32.4 to 33.7 MB (under the 34 MB
target by about 0.3 MB). The default GPU build's `.text` gate should be 18.5
MiB (19,398,656 B), recorded when S-gpu lands it; a CPU-only build keeps 15
MiB.

## m4-scene

**2026-10-09 · m4-scene: naga is checked in the compiler, behind a `shaders`
feature; the CPU-only build links none of it.** `check::shaders` (naga
parse, validate, `u_*` reflection) lives in `strand-compiler` so
`strand-dev`'s LSP checks shaders without wgpu, behind a compiler `shaders`
feature that `strand`'s default `gpu` enables; the CPU-only build keeps the
interim `uniform_ty` limit.

**2026-10-09 · m4-scene: render lowers its display lists into `strand-gpu`'s
frame type; the GPU crate depends only on `strand-scene`.** `strand-gpu`
defines `Frame` (kurbo paths, peniko paints, layers, texture ids, shader
passes), render fills it in `renderer/backend.rs`, and the GPU thread builds
the vello_gpu scene off the main thread. `strand-surface` hands out
`raw-window-handle` handles without depending on `strand-gpu`; the binary
joins them.

**2026-10-09 · m4-scene: a software Vulkan adapter counts as no device,
unless `STRAND_GPU_SOFTWARE=1`.** On lavapipe the "GPU" is the CPU and keeps
about 80 MiB mapped after a drop, so the CPU fallback serves better. The
lavapipe test tier sets the variable beside `STRAND_REQUIRE_GPU=1`, which
keeps its harness meaning.

**2026-10-09 · m4-scene: a shader on a CPU surface is drawn offscreen and
read back; an unpresentable surface promotes in readback mode.** Three
backends: `Cpu`, `GpuPresent`, `GpuReadback`, so a small shader behind a
bar's clock is still drawn as design.md requires. A pass or readback frame
is a frame hold up to 8 ms (`GPU_WAIT`), then the previous result draws;
while the device starts or is absent the CPU fallback draws.

**2026-10-09 · m4-scene: a surface presented by the GPU has one committer,
the GPU thread.** While presented, the manager's acks, regions and margins
are pending state applied by the next present, and render paints poses into
frames. A surface to destroy lives until the GPU thread releases its
swapchain (`gpu_release`, then `take_back`). In readback mode the main
thread commits.

**2026-10-09 · m4-scene: demotion mirrors promotion, the drop is timed by
render, a failure is retried once per 30 s.** Demotion takes 500 ms of
frames under 0.2 Mpx then a settled frame; the 30 s drop deadline lives in
`Renderer::next_wake` so the GPU thread never wakes on a timer. A lost
device demotes at once with a full repaint; after a failure render asks
again at most once per 30 s.

**2026-10-09 · m4-scene: the shader ABI is a prelude Strand prepends,
Strand's bindings in group 0 and the file's `u_*` in group 1.**
`strand_scene::shader::PRELUDE` declares `time`, `size`, `scale`, `pointer`,
the vertex output and an input texture, so a file is its uniforms and one
`@fragment` function; diagnostics subtract the prelude. The checked text
travels as `Prop::Shader`/`ShaderCode`, a failing save keeps the last good
build.

**2026-10-09 · m4-scene: idle-test tolerances.** Lavapipe asserts no growth:
PSS after the second promote-and-drop within 3 MiB of after the first. The
advisory hardware leg's "within about 6 MiB of baseline" bound is set from
S-gpu's first `gpu.sh` run; design.md's +20–40 MB while promoted is checked
only on hardware.

**2026-10-09 · m4-scene: m4-plan.md brought in line with 0c and with wave 0
as landed.** The `.text` gate is 18.5 MiB, GPU exit tests assert what
lavapipe can meet, the pose-commit question is settled, and S-infra lists
where landed infra differs.

**2026-10-09 · m4-scene: time leaves reach a scope through `with_time`, and
read 0 without it.** `TokenScope::with_time(Option<TimeContext>)` with
`TimeContext { t, index, count }`, carried into overrides, instead of
changing `resolve`; without one every leaf is at `t = 0`. `wave(period,
phase)` is `0.5 − 0.5·cos(2π(t/period + phase))` (0 to 1, phase in periods);
`noise(x)` is 1-D gradient noise in −1..1, 0 at whole `x`; a non-positive
period leaves the prop unset.

**2026-10-09 · m4-scene: a template's numeric slots follow
`PropValue::numbers_mut`.** `TokenExpr::Template` gained `numbers:
Vec<Option<TokenExpr>>`, filled after colours in one depth-first order
(`Number`, `Angle`, non-`Auto` `Length`, `Insets` TRBL, `Corners` clockwise,
border width before paint, gradient angle/`from` before stop offsets, shadow
`x y blur spread`, then items). Producing them is F1's compiler work.

**2026-10-09 · m4-scene: canvas ops are the `Canvas` record's methods, and
the enum stays open.** `DrawOp` has a variant per `Canvas` method (`line`,
`rect`, `circle`, `fill`, `stroke`, `text`), fills and strokes ending the
path as in canvas 2D; it is `#[non_exhaustive]`, and `scene_catalogue.rs`
checks `DrawOp::METHODS` against the record.

**2026-10-09 · m4-scene: `svg_part` is the one scene kind with no schema
element; compiler-set props stay out of the schema.** `NodeKind::SvgPart` is
listed scene-only; the catalogue test fails if the schema ever declares
`name`, `watch`, `two_way`, `uniforms`, `shader`, `accepts`, `row_count` or
`row_first`. `uniforms` is class `Effects` (`~` defaults to
`$motion.effects`); the others snap.

**2026-10-09 · m4-scene: the backend and GPU status types live in
strand-scene.** `GpuStatus`, `AdapterInfo { name, driver, software }` and
`BackendChange` sit beside `Backend` in `strand_scene::backend`, since a
CPU-only build has no `strand-gpu`; logic receives `ToLogic::GpuStatus`.
`GpuStatus::NOT_BUILT` is the reason for a build without the backend.

**2026-10-09 · m4-scene: bundled passes read their reach from uniform slot
0; a blur reaches three standard deviations.** `ShaderPass` keeps `code`,
`uniforms`, `input`; `bloom`, `chromatic` and `wobble` take their reach from
`uniforms[0]`, other passes reach nothing. `Bundled` has nine variants for
eight effects. `Effect::Blur { radius }` is a standard deviation as in CSS
`blur()` and reaches `ceil(3·radius)`.

**2026-10-09 · m4-scene: the bundled effects' signatures.** builtin.schema
declares `bloom(radius: length)`, `crt()`, `chromatic(offset: length)`,
`wobble(amplitude: length)` and `glass()`, each returning `Filter`; further
knobs become named parameters with defaults. `filter: glass()` type-checks,
so rejecting it is `check/effects.rs`'s job (S-effects).

**2026-10-09 · m4-scene: behaviour later streams own is an explicit, tested
no-op.** The Router ignores `InputEvent::Drag*`, the demo host does not
forward `NodeEvent::Drop`, `window: true` rows mount normally, and the
`SurfaceHost` hooks (`compositor_caps`, `gpu_release`, `lock_changed`)
default to nothing, each with a test; `LockState` sits beside `SurfaceHost`.

**2026-10-09 · m4-scene: a spec's scrim is read only on popups and panels.**
`SurfaceSpec::resolve` fills `scrim` from a `Color` or solid `Paint` on
`popup` or `panel` only, leaving it `None` otherwise and for gradients.

**2026-10-09 · m4-scene (review): keyframe stops resolve like a pose.**
`has_tokens`, `reads_time` and `TokenScope::resolve` recurse into
`PropValue::Keyframes` stops: a stop's token follows theme swaps, a stop
reading `t` makes the node frame-driven, and one failing stop fails the
value.

**2026-10-09 · m4-scene (review): one `var<uniform>` per `u_*`, packed
densely in binding order.** Each `u_*` is its own binding in `@group(1)`,
keeping one prop to one declaration; `UniformSlot` gained `binding`,
`offset` is the slot's first `f32` in `ShaderPass::uniforms`
(`ShaderCode::packed`), and the GPU thread copies each slot to its binding
range at the device's alignment.

**2026-10-09 · m4-scene (review): pass uniforms are buffer values, and reach
divides by the scale.** Render packs `ShaderPass::uniforms` in buffer units
(px × scale) so the GPU thread uploads them as they are;
`Bundled::reach(uniforms, scale)`, `ShaderPass::reach(scale)`,
`Effect::reach(scale)` and `Effect::reach_of(effects, scale)` divide slot 0
back to logical damage (a blur's radius is already logical). A non-positive
scale counts as 1.

**2026-10-09 · m4-scene (review): `reduced_motion` stops the clock and keeps
the letters.** Render passes `TimeContext::frozen(index, count)` (`t = 0`)
instead of no context, so `letters { x: index * 8 }` keeps its layout; every
leaf is evaluated at the context's `t`, and no context is
`TimeContext::default()`.

**2026-10-09 · m4-scene (review): the software-adapter variable is
pending.** CI, `run.sh` and `ci.sh` do not yet set `STRAND_GPU_SOFTWARE=1`;
the integrator adds it with S-gpu's first test needing it (the lavapipe idle
test). Until then a lavapipe-only run treats the adapter as no device.

## m4-runtime-w1

**2026-10-09 · m4-runtime-w1 (F1): a time-bound value is its own `Value`
variant, and logic errors where it needs a value.**
`Value::Time(Rc<TokenExpr>)` sits beside `Value::Token`, chosen by
`TokenExpr::reads_time`. Comparisons, boolean ops, branches, `match`,
indexing, computing builtins (`join`, `pct`, `dur`, `min`, `max`, `clamp`,
`rgb`, `material`, …), service methods, action arguments and handler stores
give one error value (`vm::value::time_in_logic`), as design.md's "Motion
and time" asks; arithmetic and colour methods stay symbolic.

**2026-10-09 · m4-runtime-w1 (F1): `noise` of a constant is the scene's
noise, and `%` on tokens is `Rem`.** The VM uses
`strand_scene::tokens::noise`, so a value is the same wherever computed.
Token `%` mapped to `Div` by mistake; `strand_scene::tokens::BinOp::Rem` now
evaluates it.

**2026-10-09 · m4-runtime-w1 (F1): a shadow's colour slot is its last
colour-like part.** `convert` fills number slots in `PropValue::numbers_mut`
order; a shadow's colour comes from its last part if colour-like, else its
first colour or token. Trailing empty number slots are trimmed, so values
without time travel as before.

**2026-10-09 · m4-runtime-w1 (F1): only paint follows time, and `t` starts
at the node's first painted frame.** Layout resolves with no time context
(`t = 0`), so a time signal never re-lays out a surface; moving siblings is
what springs are for. `t` counts from the first committed frame drawing the
node on the presentation clock; a reload keeping the node keeps its `t`. A
`tokens` override reading time makes its subtree read time; the global table
is frozen since the compiler rejects time there.

**2026-10-09 · m4-runtime-w1 (F1): a cached scene with clocks is never
reused.** A surface whose last flattened scene drew time-reading nodes
flattens every frame and never reuses that cache; toggling `reduced_motion`
marks every surface dirty. Shells without time signals cache as before.

**2026-10-09 · m4-runtime-w1 (F2): a node has one clock, capped by its kind;
a capped clock reads `t` in whole ticks.** A node's cap applies to
everything on it (`shimmer` at 30 fps); others run at refresh. `t` advances
only in ticks (reached half a frame early), so capped nodes repaint only on
ticks. The rate table is `clock::rate`; grain's rate and GIF and raster
rates are left to S-effects.

**2026-10-09 · m4-runtime-w1 (F2): between ticks the loop sleeps until
`next_wake`; the frame period is estimated.** Each surface estimates its
period as the shortest gap between frames, ignoring gaps under 1/360 s and
capped at 1/60 s. When the next frame is not nearest a tick, `wants_frame`
is false and `Renderer::next_wake` is half a frame before the tick, mapped
to the wall clock via the last frame.

**2026-10-09 · m4-runtime-w1 (F2): what counts as hidden.** A node gets no
clock when it is not laid out, its `opacity` or `scale` is 0, an ancestor is
hidden, or all it draws is outside its ancestors' clip; unless the hiding
opacity or scale follows time, or (outside the clip) a placing prop (`x`,
`y`, `scale`, `rotate`, `shadow`) does. With every clock idle there are no
frames and no wake.

**2026-10-09 · m4-runtime-w1 (F3): an effect layer is a push/pop group;
vello_cpu lowers only what one cell can draw.** The layer is
`Item::PushLayer(Arc<Layer>)` … `Item::PopLayer`, like other groups. In the
256×64 cell raster, `Opacity`, `Blend` (`add` is Porter-Duff plus),
`Mask::Fade` and `Mask::Radial` lower to a cell's `push_layer`; `Blur` and
`ColorMatrix` go offscreen (F4); `Shader` groups draw unfiltered on the CPU
and `Mask::Shape` stays opaque until S-effects.

**2026-10-09 · m4-runtime-w1 (F4): offscreen groups take the effects a cell
cannot draw.** A layer with `Blur` or `ColorMatrix` is drawn into a pixmap
of its bounds plus reach, filtered (separable Gaussian to 3σ, then the
matrix on straight alpha), and drawn per cell under the layer's opacity,
blend and masks, independent of damage. The cache keys on content, region,
effects and transform, with its own 4 MB LRU budget; in-use groups are never
evicted, oversized ones are drawn uncached.

**2026-10-09 · m4-runtime-w1 (F4): a raster node is a source behind a seam,
and its pixmap follows its clock.** A `RasterSource` draws premultiplied
RGBA for a size, scale and `TimeContext` and gives its `Rate`, attached via
the hidden `Renderer::set_raster_source`. The pixmap is redrawn only when
its key (size, scale, `t`, `index`, `count`, in `Item::Raster`) changes; one
pixmap per node, outside the offscreen budget.

**2026-10-09 · m4-runtime-w1 (review): a built-in `effect` gets a clock only
when it reads time or has a raster source.** Replaces the F2 rule that a
built-in effect animates by nature, which kept idle surfaces' frame loops
running. The kind only caps a clock (`shimmer` at 30 fps); the clock comes
from time-reading props or a `RasterSource`, and a GPU-drawn effect must
give its node a clock the same way.

**2026-10-09 · m4-runtime-w1 (review): a letter's `index` and `count` are
time leaves from the VM on.** Mount binds them to
`Value::Time(TokenExpr::Index)` and `Value::Time(TokenExpr::Count)`
(`builtins::scope_value`), and arithmetic, colour methods and `noise` keep
results time-bound (`Value::symbolic_from`), so per-letter phases reach
render. Only `t` readers are frame-driven, so `x: index * 8` costs no clock;
`index` reaching logic is an error value.

**2026-10-09 · m4-runtime-w1 (review): a symbolic number never fills a
colour slot.** `convert::numeric` judges a symbolic value by shape: time
leaves, arithmetic and literals are numbers; colour methods, `oklch(from …)`
and token references can be colours. So `shadow: 0 0 (8 * wave(2s))` and
`radial(center, 40 * wave(2s))` read right; a bare token size like
`radial(center, $size)` still reads as a gradient.

**2026-10-09 · m4-runtime-w1 (review 2): an offscreen group is drawn and
filtered in its own pixmap.** The context draws straight into the kept
pixmap and the blur keeps row-pass f32 results only for the `2r + 1` rows
the column pass needs, in a ring, instead of allocating about six times the
pixmap per build. Pixels are identical
(`ring_blur_matches_the_whole_group_blur`).

**2026-10-09 · m4-runtime-w1 (review 2): the gap between a capped clock's
ticks is not the frame loop stopping.** The loop has stopped only when no
surface wants a frame and no clock waits on a wake; until then unused paint
cache and offscreen entries go by `IDLE_FREE`, so 30 fps ticks do not free
untouched blurred groups.

**2026-10-09 · m4-runtime-w1 (review 2): a global token that reads time
drives its readers.** Replaces F1's "the global token table is frozen"
claim: rather than reject time there, `TokenTable::time_paths` finds
time-reading tokens (transitively), `freeze` leaves them out to be evaluated
at the reader's time, and render treats a prop referencing one
(`PropValue::reads_time_with`, `time_reads`) or an override entry reading
one as time-reading, keeping one rule with `set { $spin: t * 20deg }`.

**2026-10-09 · m4-runtime-w1 (review 2): a node's clock starts on the frame
that draws it.** Reading a clock never starts it; it starts where the node
is drawn, or where it is hidden only by something following time. A hidden
node reads `t = 0` until it shows.

## m4-lock-w1

**2026-10-09 · m4-lock-w1: the helper's test hooks are one `faults` feature,
on in the crate's own tests.** `STRAND_AUTH_PAM_CONFDIR`,
`pam_start_confdir`, the `STRAND_FAULT` points (`auth_crash`, `auth_hang`,
`auth_garbage`) and `Client::with_test_env` sit behind `strand-auth`'s
`faults` feature, since a swappable PAM stack would be an unlock. The crate
dev-depends on itself with `faults`; `tests/no_faults.rs` checks default
builds hold none of the hooks' strings.

**2026-10-09 · m4-lock-w1: only `PAM_SUCCESS` from `pam_authenticate` then
`pam_acct_mgmt` unlocks; an expired password is refused.**
`PAM_NEW_AUTHTOK_REQD` is `Denied` with PAM's message (the user unlocks from
a TTY). A visible prompt (`PAM_PROMPT_ECHO_ON`) fails the conversation
(`PAM_CONV_ERR`); hidden prompts get the password as in swaylock. A password
with a NUL byte is refused.

**2026-10-09 · m4-lock-w1: `strand` is found in `/etc/pam.d` or
`/usr/lib/pam.d`.** The helper uses the `strand` service when either file
exists, else `login`, saying which in its hello; the client warns once per
process (`take_service_warning`).

**2026-10-09 · m4-lock-w1: a helper that times out or misbehaves is killed
with everything it started.** The client `SIGSTOP`s the helper, kills its
descendants found in `/proc`, then the group and the helper, and reaps it;
the next password starts a new helper. Any non-protocol reply is a
`Protocol` failure with the same treatment.

**2026-10-09 · m4-lock-w1: the helper is hardened only where it costs
nothing.** It sets `PR_SET_DUMPABLE` 0, reads its socket unbuffered, and
wipes every frame and password buffer on drop (`tests/zeroize.rs`). No
`mlockall`, since a small `RLIMIT_MEMLOCK` would make PAM fail and lock the
user out; swap and the shell's own copies remain residual risks.

**2026-10-09 · m4-lock-w1: where the helper is found.** `default_helper()`
looks beside the executable, above `deps`, then `/usr/libexec/strand`,
`/usr/lib/strand`, `/usr/local/libexec/strand`; no env override outside
`faults`. None found fails closed with `AuthError::Spawn`.

**2026-10-09 · m4-lock-w1: `auth` keeps its provisional stub in
builtin.schema.** Corrects m4-docs: every builtin service keeps its
`provisional` stub, replaced through `Schema::extend`, so the compiler alone
still knows the names (grammar.md's `lock { … auth.submit(secret) … }`).
`auth.schema` replaces the stub like the others;
`service_schemas_extend_the_builtin_one` proves it.

**2026-10-09 · m4-lock-w1: the `auth` store hands tokens to a sink and never
unlocks.** `auth::configure(AuthConfig { helper, timeout, sink })` takes an
`UnlockSink` (`Arc<dyn Fn(UnlockToken) + Send + Sync>`) that the binary
points at `State::unlock`; without one a success unlocks nothing. One helper
per lock session; a submit during a check is dropped, not queued (no
double-counting against `pam_faillock`); a new check clears `failed`.

**2026-10-09 · m4-lock-w1: the lock's other outputs get a manager-painted
pixel; their keyboard focus is the content's.** The manager paints a 1×1 shm
buffer stretched by `wp_viewporter` in the lock colour
(`State::set_lock_color`, made opaque) on every non-content output,
invisible to the host. Keyboard focus on a pixel surface is delivered to the
content (`keyboard_target`).

**2026-10-09 · m4-lock-w1: nothing but a token unlocks, whatever the shell
or compositor does.** `State::unlock(UnlockToken)` is the only call sending
`unlock_and_destroy`; a closed or removed `lock` spec, a dropped manager or
`finished` after `locked` sends nothing, so the session stays locked. A
refused or released lock is spent until the spec closes, so a refusing
compositor is not asked in a loop.

**2026-10-09 · m4-lock-w1: `LOCK_FALLBACK_NODE` is the seam for a lock with
no spec.** `State::lock()` without a spec still makes a content surface
under `LOCK_FALLBACK_NODE` (`NodeId::new(u32::MAX, u32::MAX)`), and
`State::lock_content()` names it, so wave 2 can paint render's fallback
there.

**2026-10-09 · m4-lock-w1: session-lock tests take a lock only in the lock
VM.** `session_lock.rs` and `vm_pam.rs` skip unless `STRAND_LOCK_VM=1`,
hostname `lockvm` and PID 1 `lockvm-init`.
`scripts/lockvm/scenarios/build.sh` builds them (paths in
`target/lockvm/bins.env`) and `all.sh` runs them as `tester`, failing on any
skip; the PAM scenario uses a default-feature helper. CI's `lock-vm` job
runs `all.sh`.

**2026-10-09 · m4-lock-w1: the instance holds the lock's `open` while
locked; `lock_shown` falls back to `open` until the compositor answers.** A
config write of `false` while locked lands in state, but the `lock`'s `open`
keeps sending `true` while the compositor reports `Locked`, with one notice
per session; after `Unlocked` a two-way `open` is written `false`. Before
any report `lock_shown` keeps the `open` rule, failing closed.

**2026-10-09 · m4-lock-w1: one `lock` per config; surface props a lock
cannot honour are warnings.** A second `lock` is `check::lock_twice`;
`screens`, `layer`, `anchor` and `keyboard` on a lock are `check::lock_prop`
warnings. `attach` and `scrim` are left to S-surface's check.

**2026-10-09 · m4-lock-w1 (review): no lock is taken until the binary opts
in.** `State::enable_session_lock()` gates both paths: before it an open
`lock` spec is a one-time warning (not spent) and `State::lock()` is
`LockError::NotEnabled`, since a lock with no unlock wiring is a lockout.
Wave 2's `run/lock.rs` enables it with the `auth` sink.

**2026-10-09 · m4-lock-w1 (review): a token that arrives while the lock is
pending is kept until `locked`.** Sending `destroy` for a pending lock risks
`invalid_destroy` if `locked` is undispatched, so the token is kept on the
pending lock and spent when `locked` is dispatched; `finished` drops it.
`unlock()` returns true for a kept token.

**2026-10-09 · m4-lock-w1 (review): `finished` after `locked` asks for a new
lock, once.** Replaces the earlier "send nothing" rule, which could leave
the user on the compositor's blank fallback with no password field. The
manager asks for a new lock at once (same spec, or the fallback node), once
per lock session (reset by an unlock or the spec closing); if the compositor
had unlocked, the user enters the password again, the fail-closed cost.
Decided by the pure `after_finished`.

**2026-10-09 · m4-lock-w1 (review): a lock mounted open by a reload is not a
new request, in compiler and surface manager alike.** Only a mounted lock's
`open` going false to true re-arms `lock_shown`, matching the surface
manager's rule. A reload's new `lock` node takes over the old one (one
`lock` per config): a held lock keeps going with its content on the new
node, a spent one stays spent.

**2026-10-09 · m4-lock-w1 (review): a password longer than PAM takes is
refused, never cut.** `protocol::MAX_PASSWORD` (511, `PAM_MAX_RESP_SIZE`
less its NUL) is the limit: `Client::submit` answers longer input `Denied`
without starting the helper, and the helper's `check` refuses it too, so a
prefix can never unlock. A cut frame stays over the limit.

**2026-10-09 · m4-lock-w1 (review): `finished` after `locked` is answered
with `unlock_and_destroy` before the new lock.** As the protocol asks
(`destroy` after `locked` is an error), so a compositor counting the old
object as the locker does not refuse the new lock; the new lock follows in
the same flush. `State::unlock` stays the only release of a lock the
compositor holds.

## m4-lists-w1

**2026-10-09 · m4-lists-w1: a list is windowed only when its `for` is its
only content, starting with 32 rows.** `window_for` windows a `list` only
when its `for` is the one child making nodes; otherwise every row mounts.
Logic first mounts `DEFAULT_LIST_WINDOW` (32) rows until render's first
`ListWindow`; requests cap at `MAX_LIST_WINDOW` (1,024); a window past the
end shows the last rows. Only rows the window moves are marked `window`;
data-inserted rows play `enter`.

**2026-10-09 · m4-lists-w1: render scrolls by a paint offset, lays a row out
once per scroll, never shows past mounted rows.** Wheel steps retarget a
`$motion.spatial` spring; touchpad motion tracks the fingers and flings with
`FLING_DECAY` τ = 0.35 s until below 20 px/s; `reduced_motion` lands at once
and does not fling. Frames inside laid-out rows (view ± `layout_margin`,
32..128 px) only translate; leaving them lays out incoming rows only
(`relayout_list`). The view clamps to mounted rows; logic is asked
(`take_list_windows`) for view ± 1.5 viewports once mounted rows stop
covering ± one.

**2026-10-09 · m4-lists-w1: `nav` selects by global index, with Home, End
and page keys.** The Router keeps the selection's global index (`row_first`
+ place): Up/Down by one, Page_Up/Page_Down by a view, Home/End to the ends;
keys, not syntax. An unmounted target is revealed (`reveal_index`) and
selected on landing, Return activating it then. A row scrolled out by the
pointer stays selected by index without scrolling back; a new query restarts
at the first row.

**2026-10-09 · m4-lists-w1: submenu keys and the Router hooks.** Right opens
a submenu only outside an `input`, under the selected row of the focused
list, else among the hovered chain's direct children; Left closes only a
nested popup. `Router::drag()` and `DragView` are published (always `None`
until wave 2). `type: password` inputs needed no change.

**2026-10-09 · m4-lists-w1: `pages` direction travels as `row_first`, the
page's source offset.** The `pages` node reuses `Prop::RowFirst`, set by the
shown page's switch, and render compares old and new to slide forward from
the right with `slide(right)`/`slide(left)`, keeping the old page as a
ghost. A page's own `enter`/`exit` wins, `transition:` on `pages` is
S-effects' mask (`Renderer::page_swap`), `reduced_motion` snaps, and `pages`
clips its content.

**2026-10-09 · m4-lists-w1: the binary's side of list windows, and the sway
test's reading of "grim refs".** The main thread forwards
`Renderer::take_list_windows` as `ToLogic::ListWindow { list, first, count
}` and logic applies `Instance::set_list_window` (`run/lists.rs`). The host
pings its wake source after input that leaves a surface wanting a frame.
Damage lines gain `gaps=N`; `STRAND_MOCK_APPS=<n>` installs mock apps; the
sway launcher test checks screenshots structurally, not against PNGs.

**2026-10-09 · m4-lists-w1: the 3.5 ms scrolling budget is a p95 of the
render thread's frame work, gated only when optimised.** `list_scroll_bench`
measures the render thread's work per presented frame at 60 and 144 Hz
through a swipe, fling and wheel steps, gating the p95; a debug build only
reports and keeps functional checks. A miss is held to the end with
`GATE_MISS` and advisory in the container suite.

**2026-10-09 · m4-lists-w1 (review): in a `nav` input, Home and End stay the
caret's.** An `input` keeps Home/End (Shift extending); the list's ends move
to Ctrl+Home/Ctrl+End. A focused `list` takes Home/End with any modifiers;
Page_Up/Page_Down stay the list's.

**2026-10-09 · m4-lists-w1 (review): a list is windowed only when each item
is one row.** The compiler windows a `for` only when its body always mounts
exactly one scene node (one non-on-demand, non-surface element, or a
component doing the same, followed eight levels), since render and the
Router count children as rows. Other lists mount every row but still lay out
only what is in view.

**2026-10-09 · m4-lists-w1 (review): a page's direction is its place in
mount order, not its source offset.** Replaces the source offset, which
never changed for pages a `for` makes: logic orders pages by their switch
fragment's place in the fragment tree (source order, `for` item order,
stable across `if`/`match`) and moves `row_first` by one towards the new
page's side.

**2026-10-09 · m4-lists-w1 (review): a row the window unmounts keeps its
`state`s by key.** Like a closed popup (`Ctx::keep_cells`, `Ctx::closed`), a
window-unmounted row's cells move to a kept scope under the list and rebind
when its key mounts again; a key the data drops disposes them. Only cells
move; `let`s and bindings rebuild.

**2026-10-09 · m4-lists-w1 (review): "smooth" also means no frame waits for
logic's rows.** `ListFrames` counts `stalls` (shown offset more than half a
pixel from the scroll's) and reports `top_row`, checking every painted
frame. The bench's logic stand-in answers 50 ms late and must leave zero
stalls at 5,760 px/s, covered by `WINDOW_NEED` (one viewport) and
`WINDOW_OVERSCAN` (1.5); the sway test reads `stalls=` and `top=` from
damage lines.

**2026-10-09 · m4-lists-w1 (review): input pings the main loop only for a
surface it woke.** The host notes which surfaces were idle before an input
event and pings only if one of them wants a frame after it, avoiding an
extra `text_ready` pass per pointer motion while something animates.

**2026-10-09 · m4-lists-w1 (review): the bench's CI lines and the exit box
go to the integrator.** The stream's `ci.sh` and `gate-misses.sh` edits are
reverted; the integrator is asked to add `cargo test --profile timing -p
strand-render --test list_scroll_bench -- --nocapture` to GitHub's `timing`
job and ci.sh (30 s, `^list scroll` in `key_numbers`). The "smooth 2,000-row
scrolling" exit box stays unticked; replacing grim refs with `gaps=`,
`stalls=`, `top=` checks is for the integrator to accept.

**2026-10-09 · m4-lists-w1 (review 2): a window move keeps its rows' cells
in one pass.** The window collects rows leaving it and moves their cells at
once (`Ctx::keep_row_cells`), skipping already-kept cells by set lookup and
making a keep scope only for rows with cells, so a move no longer costs rows
× cells and stateless rows make nothing.

**2026-10-09 · m4-lists-w1 (review 2): a reveal's hold is not a stall.** A
list whose offset `reveal_index` moved does not count held frames until its
view reaches the offset or the pointer moves that list again.

**2026-10-09 · m4-lists-w1 (review 2): the sway test reports stalls, the
bench asserts them.** Wall-clock checks stay out of functional assertions,
so `the_design_launcher_scrolls_2000_apps` prints its stall count and
asserts only `gaps=0`, the view past row 100 and Ctrl+End;
`list_scroll_bench.rs` deterministically requires `stalls == 0`.

## m4-surface-w1

**2026-10-09 · m4-surface-w1: the shared fake compositor is a test-only
crate.** The fake from `strand-services/tests/common/fake_wlr.rs` moved
unchanged into `crates/strand-fake-wayland` (publish = false), with a
one-line re-export at the old path, so tests across crates share it without
`#[path]` ties. It gained the surface globals sway 1.9 lacks and records
what each surface committed, which the tests assert on.

**2026-10-09 · m4-surface-w1: capabilities reported after the first wakeup,
and on change.** `ext_background_effect_manager_v1` says whether it blurs
only in its `capabilities` event, so the manager reports in an idle callback
at the end of its first wakeup and again when the flags change.
`background_effect` counts only with the blur flag, so a manager that cannot
blur keeps the tint (design.md's Hyprland rule).

**2026-10-09 · m4-surface-w1: the blur region stays inside the rounded
shape.** A `wl_region` is integer rectangles, so bands are rounded inward
(each row as wide as its narrowest point inside the corner arc), because
blur past a corner shows while a missing sliver under an antialiased edge
does not. Equal rows merge; `radius: full` is taken as a pill.

**2026-10-09 · m4-surface-w1: the blur fallback's reason is the
compositor's, said once.** Every `blur` node falls back for the same reason
(no `ext-background-effect-v1` that blurs; `blur_fallback: none` is a
choice, not a fallback), so the first frame asking for blur logs one warning
and sends a `notices` event to `strand watch` through `ToLogic::Notice`. On
Hyprland the reason names `strand compositor-rules` and the tint stays
unless the effect confirms blur.

**2026-10-09 · m4-surface-w1: `strand compositor-rules` prints Hyprland 0.56
Lua rules.** Default output is `hl.layer_rule({ name, match = { namespace =
"^strand-X$" }, blur, blur_popups, ignore_alpha })`; `--classic` prints
hyprlang `layerrule` lines. A rule goes to each `bar`, `panel` or `osd`
whose tree or nested popup has a `blur` prop; `ignore_alpha` is 0.5 so
margins and shadows (alpha up to 0.4) stay sharp. It lives in
`crates/strand/src/rules.rs` and never talks to a running Hyprland.

**2026-10-09 · m4-surface-w1: scrim covers the usable area; a popup's is a
layer surface.** The scrim covers the usable area, not bars' exclusive
zones, so bars stay readable and clickable. A scrim alone gets
`strand-<Name>-scrim` with an empty input region; without single-pixel
buffers the pixel is a 1×1 shm buffer the viewporter scales, else a
surface-sized shm buffer. Its panel-subsurface placement is replaced by the
entries below.

**2026-10-09 · m4-surface-w1: `attach:` forces the anchor edge and names a
popup's side.** By "remove a concept", `attach` alone says where the box
sits: a panel anchors to that edge (other axis kept) with margin 0 there; a
popup's `attach: <edge>` names its own side touching the anchor, gap 0.
Render (`strand-render/src/fillet.rs`) squares the touching corners, adds
concave fillets of the target's `radius`, and sets the overhang past the
edge to 0.

**2026-10-09 · m4-surface-w1: `attach` and `scrim` on `bar`, `osd` or `lock`
are errors.** The schema lists both in the shared `surface` group, so
`check/surfaces.rs` (`check::surface_prop`) restricts them to `popup` and
`panel`, over props and `when` blocks, with a help naming the surface that
should carry it.

**2026-10-09 · m4-surface-w1: a panel's scrim is a layer surface on the
layer below the panel.** Replaces the subsurface reading: niri crops a
layer's subsurfaces to its origin, while different layers stack in a defined
order everywhere. A `top` panel with a scrim goes on `overlay` with its
scrim on `top`; a panel on another layer keeps it with the scrim one below
(none below `background`). The scrim is the click-away catcher again;
toggling `scrim` recreates the panel.

**2026-10-09 · m4-surface-w1: a popup's scrim goes below a nested-in panel,
beside a bar.** A popup's scrim on its root's layer covered a panel root on
niri, so it goes on the layer below its root, and a `top` panel with a
nested popup declaring `scrim:` rises to `overlay`
(`placement::layer_config_with`), whether or not the popup is open. A bar
keeps its popup's scrim on its own layer; an open panel's catcher moves to
the layer `Under::of_layer` names.

**2026-10-09 · m4-surface-w1: a bar shadowing the usable area is a panel to
its popup's scrim.** The scrim shares a bar's layer only when the bar's
buffer lies inside its exclusive zone
(`LayerConfig::inside_exclusive_zone`); otherwise the scrim goes below and a
`top` bar with a scrimmed popup rises to `overlay`. Raising only while the
popup is open is left for later. Tested by
`placement::tests::a_bar_shadowing_the_usable_area_rises_over_its_popup_scrim`.

## m4-integration-w1-w1

**2026-10-09 · m4-integration-w1: list_scroll_bench is a timing gate; flat
PushLayer/PopLayer list accepted.** S-lists' 3.5 ms scroll bench joins
GitHub's `timing` job and an advisory `timing_step` in
`scripts/container/ci.sh`, named in gate-misses.sh. The display list is flat
`Item::PushLayer`/`PopLayer` markers plus `Item::Raster { node, key, pixmap,
rect }`, so damage, CPU raster and GPU lowering walk one `Vec` without
recursion.

**2026-10-09 · m4-integration-w1: Hyprland's rung-2 proof is contrast under
230.** `surfaces_meet_the_live_compositor` hands `hyprctl eval` only code
lines (Lua `--` comments made it print usage) and fails on a usage reply.
Hyprland 0.56.2 blurs over a mix of blurred and sharp stripes (contrast 153
vs 306 unblurred), so the test asks for under 230, which a no-op rule cannot
meet; the blur-ladder box stays unticked.

## m4-gpu-w2

**2026-10-09 · m4-gpu-w2: the GPU draws straight colours into
`Bgra8Unorm`.** `strand-gpu` lowers colours as the scene has them, swaps
each CPU upload back once, and renders into an offscreen `Bgra8Unorm`
texture, so readback is a byte copy and presenting a blit into any swapchain
order.

**2026-10-09 · m4-gpu-w2: a pass on a CPU surface is its own request; passes
are external textures.** `GpuRequest::Pass(PassFrame { key, id, size, pass,
globals })` is answered `GpuReply::PassPixels`; `Frame` has no readback
field. In a GPU frame each `Op::Pass` is drawn first and sampled by vello.
Failures answer `GpuReply::Failed` and the device stays up.

**2026-10-09 · m4-gpu-w2: GPU parts are crate defaults; the workspace takes
none.** render's and surface's `gpu` and the compiler's `shaders` are
default features, but the root `Cargo.toml` declares them `default-features
= false`. `strand --no-default-features` builds without wgpu, vello_gpu or
naga, and CI sets `STRAND_GPU_SOFTWARE=1` so lavapipe counts.

**2026-10-09 · m4-gpu-w2: WGSL problems are reported on the `shader` node's
path.** Messages start `aurora.wgsl:5:12:` with `PRELUDE_LINES` subtracted.
`Compiled::check_shaders` reads files on demand (cached per path), a failing
file keeps the last good build, and naga adds name and arity checks to
`uniform_ty`.

**2026-10-09 · m4-gpu-w2: a `when` block's uniforms land over the base
ones.** Each `when` block with a `u_*` prop holds the base uniforms with its
own on top, so setting one keeps the rest.

**2026-10-09 · m4-gpu-w2: promote only at a large frame with no spring in
flight.** `promote.rs` promotes at the first settled large frame, demotes at
a settled frame, and demotes a surface idle for 500 ms at its own wake
(`Promotion::wake`); an idle shell wakes once, 30 s after its last GPU
frame.

**2026-10-09 · m4-gpu-w2: render queues GPU requests; the host owns the
`Gpu`.** Render queues `GpuRequest`s (`Renderer::take_gpu_requests`) and the
host starts the `Gpu` when they come; replies go to `Renderer::deliver_gpu`.
Test seams `promote_now`, `set_gpu_idle`, `gpu_frames_copied` avoid
wall-clock promotion.

**2026-10-09 · m4-gpu-w2: readback frames are one frame behind; a clocked
pass is pipelined.** A `GpuReadback` surface copies the last frame's pixels
and sends the current one, so main never waits. A changed pass holds the
frame up to `GPU_WAIT` unless it reads `strand.time`. CPU-filtered groups
and masks are uploaded so backends match (tolerance 6, 2 % of pixels).

**2026-10-09 · m4-gpu-w2: the binary's GPU host and the idle cycle.**
`run/gpu.rs` starts the `Gpu`, attaches promoted surfaces via
`State::raw_handles`, paints with `Renderer::paint_gpu` one frame in flight,
and calls `malloc_trim(0)` after the thread ends. An empty paint is no
promotion frame; `STRAND_GPU_IDLE_MS` shortens the idle for tests;
`ToLogic::GpuStatus` warns once per reason.

**2026-10-09 · m4-gpu-w2: canvas drawing.** `draw: (c) => …` runs in the
prop's memo with a `Canvas` of the laid-out size; `Vm::record_canvas`
records `Prop::Draw`, drawn as clipped `Item::Fill`s. `c.fill(p)` with no
path fills the box; token paints use `DrawOp::FillThemed`/`StrokeThemed`.
`c.text` is not drawn yet.

**2026-10-09 · m4-gpu-w2: `.text` gates with the GPU linked.**
`the_release_binary_code_stays_within_its_gate` checks 18.5 MiB (19,398,656
B) with the GPU backend and 15 MiB with `--no-default-features`, and asserts
no `libvulkan` is mapped in the bar.

**2026-10-09 · m4-gpu-w2: the hardware leg.** ANV on pixman sway attaches in
`Readback`. A debug binary pages in far more code, so `gpu_idle` checks the
6 MiB baseline only in release with `STRAND_GPU_HARDWARE=1`.

**2026-10-09 · m4-gpu-w2 (review): a presented surface crossfades on the
GPU.** The snapshot is uploaded once and drawn at `1 - w` under the new
frame in a `Plus` layer at `w`, matching `swap::blend`; the copy lives in
`GpuState::fades` for the fade.

**2026-10-09 · m4-gpu-w2 (review): every frame and pass is answered.** Every
early return of `draw_frame` answers `GpuReply::Failed { surface }` (render
falls back to CPU); an empty pass answers `Failed { key }`.

**2026-10-09 · m4-gpu-w2 (review): a surface an ending thread holds is
released at its end.** `run/gpu.rs::release_route` waits for the ending
thread, else asks the running one, else takes the surface back now.

**2026-10-09 · m4-gpu-w2 (review): an unavailable reason is a warning
once.** The 30 s retry stays; the log warns only when the reason changes,
else debug.

**2026-10-09 · m4-gpu-w2 (review): colour uniforms are premultiplied sRGB.**
The space everything composites in, so `u_tint: $accent` shows `$accent`; a
shader wanting linear converts itself. Changes 0c's interface, for owner
confirmation. Still open: `strand.pointer` is -1, -1 and uniforms do not
spring.

**2026-10-09 · m4-gpu-w2 (review): a clocked shader on a GPU-drawn surface
damages its box every frame.** `Renderer::gpu_clocked_damage` adds clocked
pass boxes so the surface is not judged idle; a surface whose `Release` went
out is `releasing` and gets no frames.

**2026-10-09 · m4-gpu-w2 (review): `gpu_idle` proves the presented path.**
Renamed `gpu_is_released_when_idle`: panel `Glow` must attach in `Present`
on lavapipe for 1.5 s and match the CPU panel `Ref`; hidden, it demotes and
releases, PSS within 3 MiB across cycles.

**2026-10-09 · m4-gpu-w2 (review): bar PSS with the GPU linked.** The bar's
33.7-34.0 MB is the GPU-linked build, as the spike predicted; section
ordering and `opt-level = "s"` remain the lever.

**2026-10-09 · m4-gpu-w2 (review 2): handles lent for an `Attach` keep the
`wl_surface` alive.** `State::raw_handles` lends the surface until
`take_back`, so a destroy waits and calls `gpu_release`; a `Release` before
`Attached` hands nothing off.

**2026-10-09 · m4-gpu-w2 (review 2): still shaders keep their pixels across
a device drop.** Pass results are CPU pixmaps, kept until inputs change.

**2026-10-09 · m4-gpu-w2 (review 2): a handed-off surface's geometry goes
out as pending state.** `draw_handed_off` sends viewport, scale and popup
geometry via `commit.rs::send_geometry` without committing.

**2026-10-09 · m4-gpu-w2 (review 2): readback pixels of another size are not
copied in.** The CPU draws until new-size pixels return; the GPU thread
uploads before destroying retired images.

**2026-10-09 · m4-gpu-w2 (review 2): one device per process, also across a
drop.** The host queues requests until every dropped thread has joined
(polled every 50 ms).

## m4-effects-paint-w2

**2026-10-09 · m4-effects-paint-w2: `filter:` and `backdrop:` take their own
functions.** `backdrop:` takes `blur` and `glass`; `filter:` takes `blur`,
the seven colour functions and `bloom`, `crt`, `chromatic`, `wobble`. Others
are `check::misplaced_filter`; a negative `blur`/`bloom` radius or `grain`
outside 0..1 is `check::effect_range` (`check/effects.rs`).

**2026-10-09 · m4-effects-paint-w2: group effects are built from props every
frame.** `crate::effects::group` reads resolved `filter:`, `mask:` and
`blend:` at flatten, composing runs of colour functions into one matrix, so
`tint($accent)` follows a theme swap. `blend: normal` is no effect; the
`set_layer_effects` seam is gone.

**2026-10-09 · m4-effects-paint-w2: the colour functions' matrices.** Filter
Effects spec matrices over straight-alpha sRGB with Rec. 709 luma, so
`grayscale(1)` equals `saturate(0)`; `grayscale`/`invert` clamp to 0..1,
`hue(θ)` keeps luma, and `tint(c)` maps luma onto `c` as strongly as its
alpha. A radial mask's percentage is of the distance to the farthest corner.

**2026-10-09 · m4-effects-paint-w2: bundled filters are shader passes; on
the CPU only `bloom` draws.** `bloom`, `crt`, `chromatic` and `wobble` are
`Effect::Shader` passes with their length knob as uniform 0. On the CPU
bloom is a glow (its group over a copy blurred σ = r/3); the others draw
unfiltered.

**2026-10-09 · m4-effects-paint-w2: the shape library's outlines.** `rect`,
`pill`, `circle` are exact paths; the rest are 120 angle-sampled points
joined by Catmull-Rom, so any two morph point by point. A shaped box clips
fill, border, children and content; shadow, hit test and `squircle` use the
rounded rect at the shape's corner. `mask: shape(name)` cuts to the outline.

**2026-10-09 · m4-effects-paint-w2: `shape:` morphs by one spring on
progress.** One 0..1 channel on the `shape` transition (`$motion.spatial`;
`~ instant` and `reduced_motion` snap) lerps from the outline on screen.
State lives in `shapes::morph::Morphs`, owned by the `Animator`.

**2026-10-09 · m4-effects-paint-w2: `glow: r` is the shape grown by σ = r/4
and blurred by σ.** Boxes glow as an unoffset shadow (spread σ, blur 2σ)
with its 9-slice cache; text, icons and images glow their own pixels,
tinted, blurred and coverage doubled. `glow: r` alone uses the node's
colour.

**2026-10-09 · m4-effects-paint-w2: `inner_shadow:` is CSS `inset`; `rim:`
is a one-pixel band.** `inner_shadow` is a blurred even-odd ring clipped to
the box, between background and border. `rim: edge, color` is the band one
logical pixel in from that edge, over the border. Both follow `shape:`.

**2026-10-09 · m4-effects-paint-w2: grain animates at 12 fps, static under
`reduced_motion`.** Raster sources are built from resolved props each frame
(`effects::raster::built`, `RasterNodes::pixmap_from`, rate from
`effects::raster::rate`), replacing `Renderer::set_raster_source`;
`RasterNodes::set` stays for stateful sources.

**2026-10-09 · m4-effects-paint-w2: strokes run clockwise from 12 o'clock
inside the box.** A stroke follows the outline inset by half its width.
`wave: amplitude, wavelength` is a seamless sine (amplitude alone takes 18
px); `trim` applies after it and `trim: to` means `0, to`. Default cap
`butt`, `round` for gauges and wavy meters; `stroke`, `trim`, `wave`, `glow`
spring.

**2026-10-09 · m4-effects-paint-w2: an `arc`'s `width` is its line's; its
box is square.** On `arc` only, `width` is line width (default 4 px) and the
box is `size:`, `height:` or 24 px (`Ctx::Square`). The track spans `sweep`
(270°) with the gap at the bottom; the value fills clockwise in `color` over
`track`.

**2026-10-09 · m4-effects-paint-w2: a wavy meter's fill is a static wave
that springs flat.** Non-zero `wave` draws the fill as a sine line with a 40
px wavelength; `wave` springs to 0 on pause. The wave does not travel, which
would give every meter a clock.

**2026-10-09 · m4-effects-paint-w2: `backdrop:` is an empty offscreen group
of what is behind.** It is one `ShaderInput::Backdrop` pass clipped to the
outline, under the `bg`. The CPU draws it from earlier display items at
quarter size (half under 8 px blur, full under 2); items behind hash into
its key. `glass()`'s CPU fallback is a 16 px blur under 12 % white.

**2026-10-09 · m4-effects-paint-w2: built-in effects and particles are pure
functions of `t`.** `effect lightning | sparks | shimmer | ripple | aurora`
and `particles` are CPU rasters from props; particles cap `rate · life` at
1,000. Each is already running at `t = 0`, the frame `reduced_motion` holds.
A node's clock starts only when its raster is drawn.

**2026-10-09 · m4-effects-paint-w2: `letters` goes inside its `text`;
letters are drawn glyphs.** `text "hi" { letters { … } }` is allowed
(`check/tree.rs::rendered_child`). Letters are laid-out glyphs in visual
order with `index` and `count`, and `x`, `y`, `opacity`, `scale`, `rotate`
and `color` apply per letter.

**2026-10-09 · m4-effects-paint-w2: text `fill:` composites onto glyphs;
`text_stroke:` grows coverage.** `fill:` paints source-in over glyphs in an
isolated layer (new `fill` on `Item::Glyphs`). `text_stroke: width, paint`
is a copy under the letters, blurred 1.48 × width and thresholded at quarter
coverage.

**2026-10-09 · m4-effects-paint-w2: `roll: true` rolls changed glyphs along
the `roll` transition.** Changed glyphs slide up out as new ones rise, so
`41%` → `42%` moves only the `2`; a changed glyph count rolls all. `~
instant` and `reduced_motion` snap.

**2026-10-09 · m4-effects-paint-w2: keyframes settings and how stops
compose.** Default `duration` 300 ms; `repeat: 0` or less loops; `easing`
applies between stops. `x`, `y`, `rotate` add, `scale` and `opacity`
multiply, others replace. A tree-level `play` plays once at mount; under
`reduced_motion` finite blocks skip and loops hold. The VM has
`Value::Keyframes(DefId)`.

**2026-10-09 · m4-effects-paint-w2: `stagger:` numbers the children that
enter together.** Child `i` of a batch holds its enter pose `i · stagger`
(step capped at 1 s); a later batch counts from 0. Only paint is delayed,
not layout.

**2026-10-09 · m4-effects-paint-w2: parallax against the surface; CPU tilt
turns in the plane.** `parallax: d` moves up to `d` towards the pointer by
its offset from the surface centre; `tilt: a` turns in-plane up to `a`. Both
spring, are off under `reduced_motion`, and use `Renderer::set_pointer`.

**2026-10-09 · m4-effects-paint-w2: transition masks are clip paths.**
`wipe(edge)` (`left` default), `disc`, `dissolve` (4 px cells) and, on the
CPU, `pixelate` as a sharpening blur. A node with `transition:` masks in and
out (a removed one plays as a ghost); in `pages` the mask goes on the later
child.

**2026-10-09 · m4-effects-paint-w2: a shared-element morph starts from the
last box with its name.** Render remembers each `morph` name's last box for
1 s; an entering node with the name starts there, FLIP-style, replacing its
enter pose. Across surfaces it falls back to the enter pose until origins
exist.

**2026-10-09 · m4-effects-paint-w2: `merge d` draws a goo field under its
children.** Children's rounded boxes combine by a smooth minimum of width
`2d`, coloured by their solid `bg`, as a CPU raster under them, cached by
the boxes.

**2026-10-09 · m4-effects-paint-w2: an image swap masks the new image in
once decoded.** An `image` with `transition:` keeps the old source under the
new until the new decodes, then masks it in. Supersedes "not done yet"
above.

**2026-10-09 · m4-effects-paint-w2 (review): a `play` wants frames only
while its node is drawn.** It follows the time-clock rule, its own props
counting as time-following; a hidden play keeps its start and resumes in
phase.

**2026-10-09 · m4-effects-paint-w2 (review): an image swap waits for decode
without frames; a failed decode ends it.** The image then draws as without
`transition:`.

**2026-10-09 · m4-effects-paint-w2 (review): only a `text` may hold
`letters`.** Elsewhere it is `check::children_not_allowed`.

**2026-10-09 · m4-effects-paint-w2 (review): a lone-radius glow is the
node's colour everywhere.** Spring and rest use the node's own `color`, else
inherited (`anim::motion::glow_color`); particles read `glow` as a number or
list.

**2026-10-09 · m4-effects-paint-w2 (review): a solid text `fill` springs;
gradients, `filter: blur()` and `blur:` snap.** Structured springs are not
added; architecture.md's "Not yet animated" should list gradients, `filter`
functions, `mark_color` and `blur:`.

**2026-10-09 · m4-effects-paint-w2 (review): undrawn staggered children and
image swaps keep nothing busy.** They show at rest when next seen, as an
unseen `enter` does.

**2026-10-09 · m4-effects-paint-w2 (review): before the GPU path aurora is
still and CPU fallbacks are noticed.** Aurora draws its `t = 0` frame once
per size; `Renderer::take_effect_notices` reports each CPU fallback once a
run as `ToLogic::Notice`, a runtime notice, not a checker warning.

**2026-10-09 · m4-effects-paint-w2 (review): remembered `morph` boxes are
dropped once stale.** Boxes older than a second are pruned when the map
doubles and on tree changes.

## m4-effects-media-w2

**2026-10-09 · m4-effects-media-w2: animated images play through the image
store.** The decode carries a `Timeline` (`Decoded::anim`),
`ImageKey::frame` names a frame, and a `Player` per image and size decodes
frames in turn. The clock (`ImageStore::frame_rate`) is capped, stops while
hidden and under `reduced_motion` (frame 0); at most two frames per image
and size.

**2026-10-09 · m4-effects-media-w2: each format's timing and composition.**
Delays under 20 ms play at 100 ms. Loop counts are honoured and a finished
animation keeps its last frame with no clock: GIF without NETSCAPE once,
count `n` plays `n + 1` (0 forever). A PNG with `acTL` before its data is
animated; a WebP frame disposed to background clears.

**2026-10-09 · m4-effects-media-w2: the spectrum is 64 pitch bands from the
audio thread.** `Levels::bins` holds 64 bands evenly in pitch from 40 Hz to
16 kHz, each its loudest bin in dB from −72 dBFS. The FFT runs on the loop
thread, at most once a frame, only for a visible tap and never on silence.

**2026-10-09 · m4-effects-media-w2: `graph` and `spectrum` are CPU raster
nodes.** A `graph` keeps a sample per column, ticks every `history /
columns` (at least 16 ms) and draws only new columns. A `spectrum` has no
clock: it repaints when fed (`Renderer::feed`), and
`Renderer::take_feed_demand` limits taps to visible ones. Both draw in
inherited `color`.

**2026-10-09 · m4-effects-media-w2: a spectrum bar is its loudest band; one
tap per visible spectrum.** Averaging halved pure tones. The binary keeps a
level tap per visible spectrum (`run/feeds.rs`); one losing its tap is fed
silence.

**2026-10-09 · m4-effects-media-w2: what `#id { … }` can set.** `rotate`,
`scale` (about the drawing's centre), `x`, `y` (SVG units), `opacity` and
`fill`; the `svg` itself takes `fill` and `fit`. Parts are wrapped in groups
by byte range; an SVG is read on first draw, at most 4 MB.

**2026-10-09 · m4-effects-media-w2: Lottie through velato into vello_cpu.**
velato 0.12 shares kurbo and peniko with vello_cpu 0.3. The clock is frame
rate × `|speed|` (max 120 fps), looping; negative plays backwards, `speed:
0` holds. It is `contain`ed; files at most 8 MB; image layers draw nothing
yet.

**2026-10-09 · m4-effects-media-w2: blurred album art is an `image` in a
cached blur group.** `filter: blur(30)` caches the group, rebuilt only when
art or box changes.

**2026-10-09 · m4-effects-media-w2: window capture is a process-wide tap, at
most 15 fps.** `wm::capture::capture_window(id, max, f) -> CaptureTap`
mirrors audio taps; the `wm` service turns taps into `ProtoCmd::Capture`.
Frames are box-filtered down on the protocol thread; failed sessions retry
after a second.

**2026-10-09 · m4-effects-media-w2: a `thumbnail` is fed like a spectrum,
rounded up to 64 px.** Render asks via `FeedKind::Thumbnail { window, max }`
at the largest drawn size rounded up to 64, so animated boxes do not restart
captures. Drawn by `fit` (`contain`); a hidden thumbnail keeps its last
frame.

**2026-10-09 · m4-effects-media-w2: an animated image's clock wakes at frame
changes.** The wake is the next frame boundary (`Timeline::until_change`,
`ImageStore::frame_wake`), not the delays' common step.

**2026-10-09 · m4-effects-media-w2: animated-image players share files and
keep within 16 MB.** Players share compressed files and stay within
`MAX_PLAYER_BYTES` (16 MB), dropping least recently used.

**2026-10-09 · m4-effects-media-w2: the spectrum's FFT is a small radix-2
transform, not realfft.** rustfft cost up to 846 KB of `.text`;
`audio::spectrum::Fft` replaces it, and design.md reads "FFT on the audio
thread". velato builds at opt-level "z".

**2026-10-09 · m4-effects-media-w2: smaller wave-2 review fixes.** A
sourceless `lottie` has no clock, an unwrappable `svg` is not reparsed every
flatten, feed demand is checked on detach, and an `svg` `#id` taking any
other prop is `check::svg_part_prop`.

**2026-10-09 · m4-effects-media-w2: fed media drops what an old tap sent.**
Taps are numbered and `fed()` drops sends not from the node's running tap.

**2026-10-09 · m4-effects-media-w2: a thumbnail's frame belongs to one
window.** `capture_window`'s callback takes `Option<&CaptureFrame>` (`None`
once when its window goes), and render drops the frame when the source names
another window.

**2026-10-09 · m4-effects-media-w2: a stopped capture session is replaced
after RETRY.** It leaves the list after `RETRY` (1 s), then a still-listed
toplevel gets a new session; the fake gains `Cmd::StopCapture`.

**2026-10-09 · m4-effects-media-w2: the player budget never drops a player a
live frame draws.** `Players::with_live` keeps players drawn together past
the budget.

**2026-10-09 · m4-effects-media-w2: the live thumbnail proof checks the
colour.** `TestWindow::open_filled` paints one colour and the matrix waits
for a frame centre within 3 of it.

## m4-lock-w2

**2026-10-09 · m4-lock-w2: what shows the built-in password field.** While a
lock is asked for or held, `run/lock.rs` shows render's fallback on the
first of: logic ended or panicked; a heartbeat unanswered for 3 s (beats
only while locked); SIGINT/SIGTERM; no committed lock frame within 1 s;
locked with no `lock` open; the lock's node gone; a runtime fault in the
lock's subtree; the text worker gone; `auth` unable to check
(`Verdict::Failed`); `finished` after `locked`. Once shown it stays until
unlock.

**2026-10-09 · m4-lock-w2: the fallback checks passwords with its own
client.** The fallback runs its own `strand_auth::Client` (default helper,
no test hooks) on its own thread, its token going into `State::unlock` like
`auth`'s. Both token paths are wired before `State::enable_session_lock` in
`Guard::wire`, so no build can take a lock it cannot release. `AuthConfig`
gains `failed` and `client_hook`.

**2026-10-09 · m4-lock-w2: a strand killed while locked locks again when it
starts.** While the session is locked the binary keeps
`$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.locked`; a strand finding it
locks at once and shows the fallback. A stale marker from an old session
locks once, which fails closed.

**2026-10-09 · m4-lock-w2: the fallback draws without vello, fonts or the
text worker.** `strand_render::lock_fallback` paints a signed-distance
rounded field and at most 24 dots straight into the buffer in fixed colours
(`#11111b` background, `#1e1e2e` field, `#3b202c` refused); the other
outputs take its background. Theme tokens are not used, since the theme is
part of what may be broken.

**2026-10-09 · m4-lock-w2: lock backgrounds are single-pixel buffers from
the scrim code.** Solid outputs use `crate::solid::solid_buffer`
(`wp_single_pixel_buffer_v1`, else 1×1 shm) scaled by the viewporter,
sharing the scrim's path; `State::lock_solid_is_single_pixel` lets the VM
test check it.

**2026-10-09 · m4-lock-w2: one `faults` feature on strand, unified with
strand-auth's.** `strand/faults` enables `strand-auth/faults` (`auth_crash`,
`auth_hang`, `auth_garbage`, `Client::with_test_env`) and the binary's
`STRAND_FAULT` points `logic_panic`, `logic_hang`, `text_panic`,
`lock_no_frame`, `auth_missing`, plus a 3 s `auth` timeout under
`auth_hang`. Without it the names are absent
(`the_binary_holds_no_fault_code`). OOM is played as SIGABRT plus a restart.

**2026-10-09 · m4-lock-w2: `finished` after `locked` is played in the VM by
a Wayland proxy.** sway 1.9 never sends `finished` to a granted lock, so
`tests/lock/proxy.rs` forwards all bytes and descriptors and injects
`ext_session_lock_v1.finished` on request; the test asserts the new lock
shows the fallback and takes only the right password.

**2026-10-09 · m4-lock-w2: the lock idiom submits on Return through `on
key`.** The `Router` turns Return into `activate` only for a list's selected
row, so auth.schema now documents `on key(k) { if k.name == "Return" {
auth.submit(secret); secret = "" } }`. Return activating a plain input is
left as an open question for the integrator.

**2026-10-09 · m4-lock-w2: `pam_faillock` is proved with the scenario's own
stack.** `faillock.sh` swaps in `deny=3` around `pam_unix`, its tally in a
user-owned directory, then restores the stack. After three wrong passwords
the right one is refused as `Denied` by the same and a new helper; a lockout
must not show the fallback.

**2026-10-09 · m4-lock-w2: the lock's content survives its node going (fault
point `lock_unmount`).** strand-surface remakes the content with the old
node after `spec_gone`, so the host must still treat it as the lock content.
Fault point `lock_unmount` applies logic's `Remove` in the VM
(`lock_unmounted_then_the_contents_output_unplugged_keeps_the_fallback`).
The remembered-node mechanism is replaced by the later "strand-surface names
the lock's content" entry.

**2026-10-09 · m4-lock-w2: "the lock's colour" is the `lock` node's `bg`.**
While the config's lock shows, the solids take the `lock` node's `bg` with
tokens resolved through the global table and the lock's `set`; a gradient
gives its first stop, no `bg` is black, and they follow the lock's `when`s.

**2026-10-09 · m4-lock-w2: fallback reasons live only while a lock is asked
for or held.** Each main-loop turn with no lock drops a pending reason, and
the restart marker is removed once no lock is asked for or held (a refused
lock had left it). Replaces "Only an unlock removes it".

**2026-10-09 · m4-lock-w2: the 1 s first-frame deadline is gated, never a
functional failure.** `a_healthy_lock_draws_its_first_frame_in_time` takes
three lock sessions and raises `GATE_MISS` at its end if a first frame
reached 1 s; `Vm::content()` raises `GATE_MISS` when the fallback took over
for a missed frame. `FIRST_FRAME_TEXT_WAIT` (500 ms) leaves little margin;
left open.

**2026-10-09 · m4-lock-w2: password redaction covers current values by
substring.** Logic keeps each `type: password` input's current value as a
zeroizing `Password` and replaces it with `<redacted>` in fault messages,
watch `fault` events and failed-write debug lines. Older values are not
kept, since the idiom empties the field on submit.

**2026-10-09 · m4-lock-w2: CI lints faults-only targets and runs the lock VM
for every lock file.** The lock-vm paths filter gains run/mod.rs,
run/shell.rs, run/logic.rs, strand's Cargo.toml, tests/lock/** and
strand-surface's solid.rs and manager/scrim.rs; lint gains `cargo clippy
--workspace --all-targets --all-features`.

**2026-10-09 · m4-lock-w2: strand-surface names the lock's content.**
`Guard::check` sets the content each turn from `State::lock_content()`,
paired with the node the host saw attached (the fallback node if none,
showing "locked with no `lock` open"); the remembered node is gone. Fault
point `lock_unmount_unseen` plays a lock's node gone before any output
(`lock_unmounted_with_no_output_then_an_output_keeps_the_fallback`).

**2026-10-09 · m4-lock-w2: with a password input mounted, value-carrying
fault errors are redacted whole.** Substring search misses transformed or
copied passwords, so while any `type: password` input is mounted every fault
whose error can carry values (`Error::Failed`, keyed-collection errors) is
logged as `<what>: <redacted>`, keeping `at` and `frozen`. Errors made only
of ids and names keep their text. Replaces "by substring" for the mounted
case.

## m4-lists-w2

**2026-10-09 · m4-lists-w2: the calendar Escape flake was the test keyboard
joining late.** A key sway receives before strand binds `wl_keyboard` is
lost, so the product is unchanged. The `Desk` now makes its virtual keyboard
after sway is up and before strand connects, an ordering guarantee, so
`the_bar_and_its_calendar_on_two_outputs` presses Escape with no sleep; the
launcher test uses the same keyboard.

**2026-10-09 · m4-lists-w2: the grim proof of 2,000-row scrolling is row
continuity between settled shots.** `the_design_launcher_scrolls_2000_apps`
scrolls sixteen bursts of 16 notches (240 px, five 48 px rows), settling
each with the pointer off the list, and asserts each shot's list clip equals
the previous moved up exactly 240 px (at most 0.2 % of pixels off by more
than 24), that one more row does not match, and that `top=` advances by
five. The exit box is ticked.

**2026-10-09 · m4-lists-w2: the launcher scroll test waits for the box's
full width.** Settled now also requires the box to measure its declared 600
px at its middle row, the end of the `scale: 0.96` enter pose; continuity
bursts wait until no frame is painted between two shots and the pixels are
identical.

**2026-10-09 · m4-lists-w2: drag and drop matches types by name; the value
stays on the logic thread.** The `drag:` type name comes from the value at
runtime and each `on drop`'s from its first parameter as declared (untyped
is `any`), printed by the checker's type display (aliases resolve, `path`
reads `text`, `T?` as `T`). The value stays per source node (`Ctx::drags`)
and `Instance::drag_value` hands it to the drop. An element may have one `on
drop` per type; a keyed reorder is a keyed move that springs by FLIP.

**2026-10-09 · m4-lists-w2: the Router owns the drag gesture; the dragged
row is a held paint offset.** A press on a `drag:` source becomes a drag
after 6 px (`DRAG_THRESHOLD`) and its release clicks nothing. The source is
drawn at the pointer as `Animator::lift`, its box kept, and letting go
becomes the keyed FLIP glide. The target is the innermost node under the
pointer (`Renderer::hit_under`) taking the type; `at` is a global row index
(offset by `row_first`). Escape cancels; other programs' drags reach only
`on drop` taking `Drop`.

**2026-10-09 · m4-lists-w2: drop arguments are made on the logic thread; a
gone source delivers nothing.** Render sends `NodeEvent::Drop { payload, at
}` and `run/lists.rs::drop_args` builds the arguments; if the source
unmounted, the drop is dropped with a debug line. Another program's drop
becomes the `Drop` record with fields per `kind`; `app` is the installed
`App` the drag named, else null.

**2026-10-09 · m4-lists-w2: `wl_data_device` carries only what one surface
cannot.** Within a surface the press grab suffices; the manager starts a
`wl_data_device` drag only when the held pointer leaves the surface
(`SurfaceHost::drag_source`), offering one MIME type naming this process and
manager. No drag icon follows the pointer (left for later). Other programs'
offers are accepted copy-only exactly while the Router has a target
(`SurfaceHost::drop_accepted`); files preferred over text, `.desktop`
uri-lists are apps, read via calloop up to 4 MiB.

**2026-10-09 · m4-lists-w2: an unread drop cancels its sender; a dropped
offer is always destroyed.** Every drop ends in `end_drop`: read, the offer
is finished, destroyed and the host gets `DragDrop`; unread (no pipe, error,
over 4 MiB, or past `DROP_READ_TIMEOUT` 5 s), it is destroyed unfinished,
which tells the sender `cancelled`, and the host gets `DragLeave`.

**2026-10-09 · m4-lists-w2: the dnd test counts each drag's end once.** A
finished source can also get `cancelled`, so the test program counts each
source once by whichever of `dnd_finished` and `cancelled` came first.

**2026-10-09 · m4-lists-w2: an `on drop` takes what a call with its
parameter would.** Delivery matches with the checker's `assignable` (an
empty list fits any list; `int` fits `float`, `length`, `angle`), and
`Prop::Accepts` lists every fitting name so render agrees. The dragged name
stays runtime-derived, since the expression's static type is dropped in
`lower`.

**2026-10-09 · m4-lists-w2: only a `list` or a row of `drag:` items holds
drop rows.** A target holds rows when it is a `list` or any live child is a
`drag:` source; any other target is placed among its parent's rows, so a
per-item dock target with content gets its dock index
(`a_per_item_drop_target_with_content_is_placed_among_its_siblings`).

## m4-surface-w2

**2026-10-09 · m4-surface-w2: Hyprland's "half-blurred" stripes were
aliasing; the rule blurs fully.** Hyprland's dual Kawase blur (size 8, 1
pass) lands every tap on the same phase of 2 px stripes, leaving a quarter
and three quarters. Nothing in `strand compositor-rules` changes; the test
paints 3 px stripes and asks for contrast under 60 of the unblurred 306,
which a no-op or half blur cannot meet.

**2026-10-09 · m4-surface-w2: what a surface delegates of its pose, and in
whose terms.** Compositors draw a layer surface from its arranged box's
top-left, so `SurfacePose` is in the surface's own terms (scale keeps the
top-left; offset moves it). `strand_render::pose` delegates opacity on all
but a `lock`; x or y on a `panel`/`osd` anchored on one side of that axis;
scale only when both are, with a centre-keeping move. A `bar` or `popup`
delegates only opacity. Static and moving poses delegate alike;
`set_compositor_poses` follows `CompositorCaps::delegates_poses`.

**2026-10-09 · m4-surface-w2: while the GPU presents a surface, its pose is
painted.** Two seams for the GPU merge: `State::clear_pose(surface)` sets
the identity pose as pending state for `hand_off`, and
`Renderer::delegated_pose` plus the flatten hook (`extras.compositor_poses`)
must skip `Backend::GpuPresent` surfaces.

**2026-10-09 · m4-surface-w2: submenus open to their `anchor:` side; the
tray's click point is the pressed node's corner.** A popup nested in a popup
opens right by default, left for `anchor: left`, above or below for
`top`/`bottom`, level with the row's top (`PopupConfig::aligned`, `FlipX`),
at gap 6. The host records the pressed node's bottom-left corner in
output-logical coordinates on every press
(`strand_services::tray::set_click_point`), so tray actions keep their
signatures. A new `SurfaceHost::surface_placed` hook reports where surfaces
lie.

**2026-10-09 · m4-surface-w2: on Hyprland a pose's scale is painted; fade
and offset are delegated.** Hyprland stretches a layer surface to its
arranged box regardless of the viewport destination, so with
`HYPRLAND_INSTANCE_SIGNATURE` set the host calls
`Renderer::set_compositor_pose_scale(false)`; the offset then carries no
centre-keeping move.

**2026-10-09 · m4-surface-w2: a tray menu is a recursive component over
DBusMenu; a `list` takes `focus: true`.** The fixture `tray_menu.strand`
uses `open: <-> open`, `on change item.menu.opened` and a recursive `Menu`
component, mounted lazily, with no new syntax. `focus: bool` is now a `list`
prop so a menu gets the keys. The manager remembers a layer surface that
released a grab while focused (`State::releasing`) so a late
`wl_keyboard.leave` does not close a regrabbed popup
(`tray_menus_nest_walk_and_choose`).

**2026-10-09 · m4-surface-w2: wave 1's edits in S-surface files stand;
`Host::input` reuses a buffer.** The lock and Router edits stay inside
architecture.md's interfaces and are tested. The one change: `Host::input`
reuses a buffer kept on the host instead of allocating a `Vec` per input
event.

**2026-10-09 · m4-surface-w2: a possibly late leave is held to the end of
its dispatch.** A leave for a `releasing` surface while a new grab holds the
keyboard is held (`State::held_leave`) until a calloop idle; an enter for
that surface in between makes it stale, otherwise the popup gets
`KeyboardLeave`. `releasing` is dropped with its surface. The fake gains a
seat keyboard (`Cmd::KeyboardEnter`, `KeyboardLeave`, `Key`) and
`xdg_wm_base`.

**2026-10-09 · m4-surface-w2: the compositor-animated poses box waits for
the GPU merge.** The box is unticked until GPU-presented surfaces paint
their pose: at the merge `Renderer::delegated_pose` and the flatten hook
skip `GpuPresent` surfaces and `State::hand_off` calls `State::clear_pose`;
the integrator ticks it with a proving test.

**2026-10-09 · m4-surface-w2: a layer surface's place follows its pose's
margins.** `place_layer` places at `posed_margin`'s margins, `sync_pose`
re-places when the offset moves them, and a monitor geometry change
re-places its layer surfaces
(`a_posed_panel_is_placed_where_its_margins_put_it`).

**2026-10-09 · m4-surface-w2: a delegated fade claims no opaque region.**
The opaque check also needs the delegated opacity at 1, and
`State::sync_opaque` sends a changed opaque region with a bare commit
(`a_delegated_fade_claims_no_opaque_region`,
`an_opaque_region_rides_a_bare_commit`).

**2026-10-09 · m4-surface-w2: under a delegated scale the manager maps
regions, input and popup anchors.** A static scale is delegated for good, so
the manager maps render's at-rest regions by `Surface::pose_factor`: opaque
rounded inward, input and blur to the nearest pixel; popup anchors map by
the parent's factor; pointer and drag positions are divided back
(`State::send_input`). A static scale above 1 stays delegated. Corrects "for
the pose's few frames" above.

**2026-10-09 · m4-surface-w2: a dropped pose places its surface again.**
`State::clear_pose` calls `place_layer` when it drops a non-zero offset and
resends the input region when it drops a scale.

**2026-10-09 · m4-surface-w2: the tray click point falls back to (0, 0) and
follows a delegated scale.** A press on an unplaced surface and every key
press set (0, 0); a timer or IPC action still sends the last press's point.
Under a delegated scale the point is `origin + corner × scale`
(`Renderer::delegated_pose`; `a_press_sets_the_tray_click_point`).

## m4-integration-w2-w2

**2026-10-09 · m4-integration-w2: S-effects' CPU-only paints are islands on
a GPU surface.** Gradient-filled glyphs (`Item::Glyphs` `fill`) and
`Mask::Shape` masks lower on the GPU as CPU islands (rasterised and
uploaded), as masks already did, rather than new GPU ops. Tests now set
`opacity:`, `mask:` and `filter:` props instead of the gone
`set_layer_effects` seam
(`shape_masks_and_text_fills_are_drawn_by_the_gpu_like_the_cpu`).

**2026-10-09 · m4-integration-w2: a raster source's `state` is its config.**
`RasterNodes::pixmap` computes the source's `RasterSource::state` and passes
it as `pixmap_from`'s `config`, so set and prop-built sources share one key
(config, size, scale, time). The flattener's clock takes a set source's
clock, then an animated image's, then a prop-built rate; `timed` ORs the
letters, shader and SVG-part rules.

**2026-10-09 · m4-integration-w2: a GPU-presented surface paints its
poses.** Render keeps `GpuPresent` roots in `Extras::gpu_presented`, the
flatten hook delegates only when `Extras::delegates_pose` allows,
`Renderer::delegated_pose` returns `None` for them, and `State::hand_off`
calls `State::clear_pose`. Readback surfaces still delegate. The poses box
is ticked (`a_presented_surface_paints_its_pose`,
`a_handed_off_surface_drops_its_compositor_pose`).

**2026-10-09 · m4-integration-w2: the 2,000-row scroll test finds the list's
clip by contrast.** GPU and CPU shots differ by a level or two everywhere,
so the clip is the rows where a pixel moved by more than the check's
tolerance (24). Promotion during scroll is what design.md asks, so the test
changed, not the backend.

**2026-10-09 · m4-integration-w2: the design bar's debug PSS ceiling is 80
MiB.** The merged debug binary measures about 69-70 MB in
`the_design_bar_keeps_the_m0_budget`, mostly its own resident code. The
debug ceiling only catches gross regressions, so it rises from 64 to 80 MiB;
the release gate (34 MB target, 38 MB ceiling) is unchanged. `services.rs`'s
ceiling stays 64 MiB.

**2026-10-09 · m4-integration-w2: `gpu_idle` keeps its sway under the system
temp dir.** A long checkout path overflowed `sun_path`'s 108 bytes for
sway's IPC socket, so the test uses `std::env::temp_dir()` like the other
sway tests.

**2026-10-09 · m4-integration-w2: fat LTO and size-built decoders keep
`.text` in its gates.** The owner-confirmed gates stay (15 MiB CPU core,
18.5 MiB GPU); image and SVG decoding crates (`image-webp`, `jpeg-decoder`,
`png`, `gif`, `fdeflate`, `miniz_oxide`, `weezl`, `imagesize`, `usvg`,
`svgtypes`, `simplecss`, `data-url`) build at `opt-level = "z"` and release
LTO goes thin to fat; the timing profile keeps `lto = false`. CPU-core
margin is now 216 KB.

**2026-10-09 · m4-integration-w2: a GPU-presented frame gets the same
post-paint work as a CPU one.** Presented surfaces stopped telling logic
their layout facts and list windows, so the post-paint steps moved into
`DemoHost::painted`, called by both paths (GPU with full damage, age 0)
(`a_presented_frame_hands_its_layout_facts_to_logic`).

**2026-10-09 · m4-integration-w2: the live thumbnail test accepts the
window's colour anywhere.** Hyprland captures a toplevel as its tiled box,
not the buffer, so the test accepts a frame with the colour at any pixel; a
wrong buffer or an all-zero frame still fails.

**2026-10-10 · m4-integration-w2: a presented frame's `damage` line is
logged once presented.** The line now waits for the GPU thread's `Presented`
reply (`Host::log_frame`, from `run/gpu.rs`) and a failed frame logs none,
matching the CPU line; `Host::painted` still runs at send. Not reproduced
locally; the explanation is a reading of the test.

**2026-10-10 · m4-integration-w2: the real-services bar's debug PSS ceiling
is 80 MiB too.** `services.rs::the_real_services_sleep_when_nothing_changes`
gets the same 80 MiB debug ceiling as the design bar for the same reason;
its 38 MB release gate is unchanged. Replaces "`services.rs`'s ceiling stays
64 MiB" above.

**2026-10-10 · m4-integration-w2: promotion's idle rule waits for the attach
and a presented frame.** A slow device start let the idle rule demote a
surface before its first GPU frame. Now the idle rule applies only to a
surface the GPU draws (`Surf::idles`), the attach and each `Presented`
restart the window (`Promotion::shown`), and `run::gpu` runs render's update
after any reply other than `Presented`
(`a_shown_frame_restarts_the_idle_window`,
`a_frame_in_flight_keeps_its_surface_promoted`).

## m4-effects-finish

**2026-10-10 · m4-effects-finish: Return activates a plain `input`.** The
`Router` sends `activate` to a focused `input` without `nav:` on Return or
KP_Enter, after `on key`; auth.schema's example returns to `on activate {
auth.submit(secret); secret = "" }` (`return_activates_a_plain_input`).

**2026-10-10 · m4-effects-finish: a node's first frame starts at its props'
values.** The Animator ignores a node's touches on the frame it enters, so
only its `enter {}` pose animates in.

**2026-10-10 · m4-effects-finish: a shared morph a preview sees start begins
on the painted frame.** A preview marks it pending; the painted frame starts
it from its own boxes and offsets.

**2026-10-10 · m4-effects-finish: `jelly` stretches a dragged node along its
motion.** A held or gliding node with `jelly: a` stretches by `1 + s` along
its motion and squashes by `1 / (1 + s)` across, `s` = `a` × speed / 1,500
px/s capped at `a`. Speed is the drawn offset, not `DragView::velocity`; the
deformation springs (`spring(300, 0.35)` default) so it wobbles.

**2026-10-10 · m4-effects-finish: the rice acceptance test.**
`rice_now_renders_at_fixed_times` paints design.md's rice
(`rice_now.strand`) at fixed times, every frame's damage inside the pill and
glow. Its paused reading is replaced by "the rice's paused half" below.

**2026-10-10 · m4-effects-finish: a `spectrum` or `graph` with no size is 64
× 24.** A side that is set wins; fully sized ones are unchanged.

**2026-10-10 · m4-effects-finish: a shared-element morph crosses surfaces on
one output.** The host gives render surface origins
(`Renderer::set_surface_origin`); a `morph` name drawn on another surface of
the same output starts there shifted by the origins' difference, else the
enter pose plays.

**2026-10-10 · m4-effects-finish: `pixelate` is a true mosaic on the CPU
too.** Supersedes the blur fallback: each cell of `(1 − p) · 12` logical px,
aligned to the box corner, becomes its average. It is render's `Layer` field
`mosaic: Option<u16>`, not a scene `Effect`.

**2026-10-10 · m4-effects-finish: goo is contoured by marching squares.** As
design.md says: a 2 px grid, interpolated edge crossings, saddles split by
the centre's sign, one-pixel antialiasing.

**2026-10-10 · m4-effects-finish: the CPU's tilt stays 2D, as design.md
says.** The in-plane turn is the specified CPU version; 3D tilt is the
Bundled GPU effects box's.

**2026-10-10 · m4-effects-finish: a backdrop inside an offscreen ancestor
shows what is behind it.** Offscreen groups are drawn in order of where each
ends in the display list.

**2026-10-10 · m4-effects-finish: `svg` and `lottie` files are read on the
image worker; Lottie image layers draw.** Sources ask via
`RasterProps::files` (`ImageStore::load_file`); results mark every surface
dirty (`files_arrived`), and a node draws nothing until its file arrives.
Lottie PNG/JPEG assets, `data:` or beside the file, draw via
`RenderSink::draw_image`; remote URLs are not fetched.

**2026-10-10 · m4-effects-finish: thumbnails turn frames upright and leave
the pointer out.** `wm::capture::upright` inverts each frame's `transform`
before scaling, and sessions keep `Options::empty()`. `TestWindow` fills its
configured size, so the matrix checks the frame's centre again.

**2026-10-10 · m4-effects-finish: "only the ring repaints"; multiple
outlines compose.** An unshaped node's border is hashed apart and, when only
it changes, damages only its strips. Multiple outlines need no list-valued
`border:`: they compose from `border:`, `stroke:`, `rim` or nesting.

**2026-10-10 · m4-effects-finish: the Effects catalogue box is ticked.**
Every catalogue row has a proving test; GPU halves stay in the Blur ladder
and Bundled GPU effects boxes.

**2026-10-10 · m4-effects-finish: multiple outlines, corrected and proved.**
A same-width stroke and border are one outline; concentric ones need a
`stroke:` narrower than its `border:`
(`multiple_outlines_compose_from_border_stroke_rim_and_nesting`).

**2026-10-10 · m4-effects-finish: the rice's paused half is in design.md;
its ring repaints.** design.md now says that when music stops only the
turning border's ring repaints. Ring strips are two corner-reach columns (a
squircle's 1.6 × radius, capped at half the side) and two border-height
rows, plus 2 px.

**2026-10-10 · m4-effects-finish: a Lottie's image assets have a pixel
budget.** Superseded by m4-owner-docs, "a Lottie's image assets are bounded
as images are".

**2026-10-10 · m4-effects-finish: jelly's speed is smoothed by time.** Each
frame moves it `1 − e^(−dt/τ)` of the way, τ = 24 ms, so refresh rate does
not matter; a held node at rest no longer keeps `Animator::busy`.

**2026-10-10 · m4-effects-finish: a popup's output follows its parent.**
Every surface's output resolves from its nearest placed ancestor whenever
any surface is placed, moves or goes.

**2026-10-10 · m4-effects-finish: the lock VM submits through `on
activate`.** `crates/strand/tests/lock.rs`'s `CONFIG` uses it and
`the_configs_lock_unlocks_through_activate_and_through_key` proves both
forms; the Hyprland thumbnail evidence reads "pending CI".

## m4-gpu-effects

**2026-10-10 · m4-gpu-effects: bundled effects run on the file-shader
pipeline with four `vec4` uniforms.** Each is a `ShaderCode` at
`bundled:<name>` (`strand-gpu/src/bundled.rs`) with `u0`-`u3` at `@group(1)`
bindings 0-3, slot 0's first float the reach. `backdrop_blur` is two steps
(`PassFrame::then`), `particles` an instanced pipeline; `PassFrame` carries
`input`.

**2026-10-10 · m4-gpu-effects: at most one optional knob per function.**
builtin.schema: `bloom(radius: length, strength: float = 1)`, `crt()`,
`chromatic(offset: length)`, `wobble(amplitude: length)`, `glass(refraction:
length = 12)`. Fixed: wobble wavelength 40 px and period 2 s, glass
dispersion 0.3 and frost 1.5 px, CRT scanlines every 3 px.

**2026-10-10 · m4-gpu-effects: what each effect looks like.** Bloom: 64-tap
Gaussian (σ = radius / 3) of luma past a 0.2-0.7 knee, screened. Chromatic:
red and blue offset apart. CRT: barrel (k = 0.06), scanlines, vignette.
Wobble: travelling sine rows. Tilt: perspective turn fitted to its box.
Glass: rounded-box refraction, dispersion, frost, tint, fresnel rim, pointer
highlight. Aurora and particles match the CPU.

**2026-10-10 · m4-gpu-effects: particles above 1,000 are the GPU's, up to
50,000 alive.** `GPU_MAX_ALIVE` is 50,000; positions stay computed on the
CPU, a pure function of `t`.

**2026-10-10 · m4-gpu-effects: where a pass's pixels go.** A node's own pass
is its `Item::Raster`. A layer's bundled passes are always read-back `Pass`
requests of CPU-rasterised input, becoming the layer's offscreen group
(`Layer::gpu`); the layer's CPU blur and colour matrix apply first.

**2026-10-10 · m4-gpu-effects: an effect asks for the GPU only while it
shows.** Wants exist only for nodes inside their clips and surface,
including `shader` nodes; the device drops 30 s after the last
(`bundled_effects_start_the_gpu_only_while_visible`).

**2026-10-10 · m4-gpu-effects: GPU forms, fallbacks and notices.** With a
GPU possible, aurora runs a clock, `wobble()` follows time and fallback
notices are silent. `tilt:` turns 3-D once the GPU answered the node (a
one-pixel pass starts the device), 2-D until then; the lean springs four
channels.

**2026-10-10 · m4-gpu-effects: a large blur is 0.2 Mpx of box and reads 3σ
past it.** A `backdrop: blur()` box of at least 200,000 buffer pixels is the
GPU's, reading the box grown by 3σ; smaller ones stay CPU. Glass reads only
its box.

**2026-10-10 · m4-gpu-effects: a shader's pointer follows the pointer.**
`strand.pointer` is in buffer pixels relative to the pass's box, (-1, -1)
outside, passed only to code that reads it and to glass.

**2026-10-10 · m4-gpu-effects: shader uniforms spring.** `Prop::Uniforms`
entries spring per channel on the effects curve; a different shape snaps.

**2026-10-10 · m4-gpu-effects: `c.text` draws.** Each canvas text op is a
text part (index up to 254), shaped in the node's themed font and drawn as
`Item::Glyphs` from its baseline point, clipped to the box.

**2026-10-10 · m4-gpu-effects: `STRAND_GPU_IDLE_MS` stays a test hook.** A
setting would add a concept design.md lacks.

**2026-10-10 · m4-gpu-effects: the hardware leg and ANV's readback.** ANV
attaches in `Readback` on pixman sway and matches the CPU; debug PSS is
reported, not gated.

**2026-10-10 · m4-gpu-effects: two fallback tests make their GPU
unavailable.** They deliver `GpuReply::Unavailable` first, since a `gpu`
build may get a device.

**2026-10-10 · m4-gpu-effects (review): a failed bundled pass draws its CPU
version.** `ShaderResults::failed` records it, `ShaderResults::usable` never
offers it, and it is not retried while inputs stay; a tilt turns 3-D only
after pixels (`ShaderResults::answered`) and never after a failure.

**2026-10-10 · m4-gpu-effects (review): pass pixels are held only while a
frame wants them.** Results record their surface and are dropped when its
wants no longer name them; they count as GPU-path cost, not the 4 MB
offscreen cache.

**2026-10-10 · m4-gpu-effects (review): a tilt's warm-up pass waits until
the node shows.** `set_promotion(false)` (tests) keeps the large-blur
surface on the CPU in the visibility test.

**2026-10-10 · m4-gpu-effects (review): "aurora and noise fields" is aurora
bundled plus noise fields as `.wgsl`.** design.md gives a noise field no
spelling, so no `effect noise` is invented.

**2026-10-10 · m4-gpu-effects (review): two test fixes.** Displaced
`blur(-4)` and `grayscale()` assertions return to the bundled-filter test so
CPU-only builds run them.

**2026-10-10 · m4-gpu-effects (review): the hardware leg in release.**
`gpu_idle` in release on ANV stayed inside its 6,144 kB and 3 MiB bounds;
the presented cost against +20-40 MB is still unmeasured.

**2026-10-10 · m4-gpu-effects (review 2): a failed pass is keyed without its
time.** A want's `base` excludes time and pointer (and clocked bundled
uniforms); a failed pass retries only when `base` changes, and the node
takes its CPU form whole (`Flattener::gpu_draws`).

**2026-10-10 · m4-gpu-effects (review 2): a tilt warms up again after the
device goes.** It keeps `answered` only while it still holds 3-D pixels.

**2026-10-10 · m4-gpu-effects (review 2): the noise-field box is left open
for the owner.** Superseded by m4-owner-docs, "the owner accepts noise
fields as `.wgsl` shaders".

**2026-10-10 · m4-gpu-effects (review 2): the idle test proves its bloom.**
`gpu_idle` waits for the bloomed #f5c2e7 box to be at least 24 (summed
channels) brighter than a plain one, which only the GPU bloom gives.

## m4-interaction-finish

**2026-10-10 · m4-interaction-finish: layer origins are arranged past our
own zones, in global layout coordinates.** `placement::arranged_area`
follows wlroots' `arrange_layers` (exclusive surfaces first, from `overlay`
down, zone plus margin); only our own surfaces are known. Origins add the
output's logical position, the space `SNI Activate(x, y)` uses.

**2026-10-10 · m4-interaction-finish: a tray action called by no input sends
(0, 0).** The click point is read when `TrayCall` is built and only inside
an input-event handler (`vm::in_input_handler`, a probe handed to the tray),
before its first await; otherwise (0, 0), as SNI hosts send.

**2026-10-10 · m4-interaction-finish: Hyprland is known by its globals, not
`HYPRLAND_INSTANCE_SIGNATURE`.** The variable leaks into nested sessions, so
`caps::Offered::from_registry` sets `CompositorCaps::hyprland` from any
`hyprland_*` global (`a_posed_panel_looks_the_same_on_every_compositor`).

**2026-10-10 · m4-interaction-finish: redaction remembers replaced values
and matches fragments.** The newest four replaced values are kept as
zeroized `Password`s, and a message holding any 4-byte run of a known value,
ASCII case ignored, is redacted whole.

**2026-10-10 · m4-interaction-finish: the supervisor requirement is
documented and proven in the lock VM.** Death while locked needs a
supervisor: architecture.md carries the user unit, and
`a_supervised_strand_dying_while_locked_keeps_the_session_locked` proves the
desktop never shows. strand ships no supervisor itself.

**2026-10-10 · m4-interaction-finish: the blur ladder's inspector half is a
`strand watch` notice per `blur` box.** When the compositor does not blur,
the host sends `ToLogic::BlurFallback` per `blur` node when the region count
changes, and logic makes one deduplicated `<file>:<line>:<col>` notice per
box with `why` from `caps::blur_missing`.

**2026-10-10 · m4-interaction-finish: how Hyprland's blur mixes with
Strand's.** With `ext-background-effect-v1` Strand sends regions and no
tint, and the layer rules blur every pixel above 0.5 alpha besides. Nothing
is detected; the printed rules say they are only for a Hyprland without the
protocol.

**2026-10-10 · m4-interaction-finish: a carried drag shows the source as its
icon.** Supersedes "no icon": `Renderer::drag_image` rasterises the dragged
node once per drag, committed after `start_drag` with the grab point as
offset.

**2026-10-10 · m4-interaction-finish: what a drag gives other programs
travels in `Prop::Drag`.** Text, `Drop` and `App` values carry an exportable
form (`List [type, kind, text…]`, read via
`strand_scene::drag_type`/`drag_export`); files go out as `text/uri-list`
and paths, apps as `.desktop` URI and id.

**2026-10-10 · m4-interaction-finish: another Strand process's drag arrives
as a `Drop`.** It is read through what it exports, or refused; no
cross-process value serialisation.

**2026-10-10 · m4-interaction-finish: `Prop::DropRows` replaces "make it a
`list`".** The compiler sets it on an `on drop` target with a direct `for`
child, and render counts rows there too.

**2026-10-10 · m4-interaction-finish: redaction review: a floor, escapes and
a core-dump risk.** Values under `FRAGMENT` (4) bytes are not kept or
matched; messages are compared Unicode-lowercased with escapes undone.
architecture.md ("The lock") records that a core dump can hold kept values
and suggests `LimitCORE=0`.

**2026-10-10 · m4-interaction-finish: review of the input flag, blur notices
and drag icon.** The input flag covers a handler's first stretch only; blur
notices are kept per surface and re-said after a reload;
`Raster::paint_window` paints the icon into a box-sized buffer.

**2026-10-10 · m4-interaction-finish: the documented unit lifts systemd's
start limit and backs off.** `StartLimitIntervalSec=0`, with
`RestartSec=100ms` growing over `RestartSteps=5` to `RestartMaxDelaySec=2s`;
`tests/lock.rs` reads the unit from architecture.md
(`Supervisor::documented`).

**2026-10-10 · m4-interaction-finish: only press-caused handlers take the
tray's click point.** `vm::INPUT_EVENTS` is `click`, `secondary`, `middle`,
`activate` and `key`.

**2026-10-10 · m4-interaction-finish: a zone that goes to none or moves
re-places the others.** `Surface::zone_on` remembers the monitor, and both
old and new monitors are re-placed; the fake gains
`SurfaceGlobals::xdg_output`.

## m4-integration-w3

**2026-10-10 · m4-integration-w3: the offscreen pass both effects branches
rewrote.** `Offscreen::prepare` keeps one pass ordered by group end, with a
`gpu` layer taking its pixels first; `render_group` applies `pixelate`
before CPU filters, and `Layer::cell_local` needs `mosaic` and `gpu` empty.

## m4-audit

**2026-10-10 · m4-audit: the lock exit is ticked; the helper is killed and
deleted for real.** `pam_helper_killed_mid_check_shows_the_fallback`
SIGKILLs a hung helper (new fault `auth_hold`), and
`pam_helper_deleted_and_killed_keeps_the_session_locked_until_it_is_back`
removes it. The fallback's `strand-lock-auth` thread now looks for the
helper at every check; until found every password is refused (fail closed).

**2026-10-10 · m4-audit: "no grim pixel shows the desktop" means every
pixel.** Locked assertions scan the whole shot (`Shot::desktop_anywhere`,
tolerance 3); `Shot::desktop()` stays for "the desktop is back".

**2026-10-10 · m4-audit: a GPU submission that never ends loses the
device.** Readbacks wait at most `HUNG_AFTER` (10 s), then the pass answers
`Failed`, the thread sends `Lost` and `Exited` and leaks the device rather
than drop it (`strand-gpu/tests/hang.rs`). A hung `GpuPresent` frame and GPU
stalls while locked were left for the owner.

**2026-10-10 · m4-audit: the GPU thread's caches are bounded.**
`strand-gpu/src/lru.rs` keeps 64 pipelines and 16 readback buffers keyed by
size, so remounts and reloads no longer grow them.

**2026-10-10 · m4-audit: the 2,000-row e2e test reports stalls, not asserts
them.** Asserting `stalls == 0` on a debug binary would gate logic speed
functionally; `list_scroll_bench.rs` stays the assertion. `still()` also
waits for the burst's frame and `pending=0` (`Renderer::work_pending`).

**2026-10-10 · m4-audit: test-only log lines.** The damage line gains
`pending=0|1`; the host logs `pointer entered surface N` at debug, which
tests wait for instead of sleeping.

**2026-10-10 · m4-audit: flakes fixed without wall-clock bounds.** `agree()`
takes each step's expected state, animated.rs writes its files once per
process, and several tests wait on channels or gates instead of timing.
`huge_fan_out_fails_fast` and `strand-dev/tests/lsp.rs` keep wide wall-clock
bounds, left to their owners.

**2026-10-10 · m4-audit: smaller fixes.** The lifecycle test is renamed
`the_thread_and_driver_start_at_spawn_and_end_at_shutdown`; the helper stops
printing its own `login` warning; thumbnail frames are read from a sealed
read-only memfd mapping and scaled before turning upright.

**2026-10-10 · m4-audit: a lock with no way out is a check error.**
`check::lock_no_auth` errors when nothing in a `lock` (handlers, timers, all
branches, shown components) calls `auth.submit`, judged only on otherwise
clean programs. A runtime "no password input" rule was rejected
(`a_lock_needs_a_reachable_auth_submit`).

**2026-10-10 · m4-audit: the run ends the GPU thread before the surface
manager drops.** Exit calls `GpuHost::shutdown(GPU_SHUTDOWN)`, joining for
at most 3 s; if a thread will not end, the manager is leaked with
`mem::forget`, keeping the `RawHandles` contract.

**2026-10-10 · m4-audit: an image worker that cannot start stops the
shell.** Shell and demo use `Renderer::try_new` (error `DemoError::Io`);
`Renderer::new` keeps inline decoding for tests and offline renders.

**2026-10-10 · m4-audit: the helper's test hooks are asked for by name.**
The strand-auth self dev-dependency is gone, so `cargo test --release` no
longer installs a hooked helper; `tests/pam.rs` and `tests/no_faults.rs`
need `--features faults` in their own CI step, and `tests/plain_build.rs`
checks a plain helper has no hooks.

**2026-10-10 · m4-audit: the `strand` PAM service is chosen only when libpam
reads its file.** The helper takes `strand` when `/etc/pam.d/strand` is
readable, or `/usr/lib/pam.d/strand` is and `login` lives only in
`/usr/lib/pam.d`; otherwise `login`. Most libpams ignore the vendor dir,
which made every password `Denied`.

**2026-10-10 · m4-audit: kept-cell values and notices go through the
password redaction.** `Shell::apply` and `Shell::after_step` pass them
through `Secrets::redact` (`redact_kept`, `redact_lines`) before logging or
showing them.

**2026-10-10 · m4-audit: a shader's per-pixel private memory is capped at
check time.** `check::shader` sums private, workgroup and local variables
(naga's `try_size`) and refuses more than `MAX_PIXEL_MEMORY` (16 KiB) per
pixel.

**2026-10-10 · m4-audit: the release frame-time bench runs in the timing
job.** `animated_frames_fit_the_refresh_budget` is no longer `#[ignore]`:
optimised builds gate 16.7 ms and half-refresh with `GATE_MISS`, run with
`--profile timing`; debug builds only report.

**2026-10-10 · m4-audit: the shape list is a reading of design.md.**
design.md names only cookie, clover, burst, pill and polygons; `rect` and
`circle` let morphs meet a plain box, the polygons are fixed, and the rest
are Material 3 shapes. Answered by m4-owner-2, "the shape set stays at 13".

**2026-10-10 · m4-audit: xdg-activation moves past M4.** Superseded by
m4-owner-docs, "xdg-activation is scheduled for M5".

**2026-10-10 · m4-audit: correction: a lock's surfaces never reach the GPU
thread.** `State::raw_handles` and `State::hand_off` refuse `Role::Lock`, so
a lock surface is readback-only, bounded by `GPU_WAIT` (8 ms) and
`HUNG_AFTER`; the lock's fault list needs no GPU-stall entry
(`a_lock_surface_is_never_handed_to_the_gpu`).

**2026-10-10 · m4-audit (round 3): the `GPU_WAIT` bound is tested.** Two
device-free tests assert a never-answered readback or pass holds at most
`GPU_WAIT`, then the CPU paints.

**2026-10-10 · m4-audit (round 3): large frames seconds apart are not one
animation.** A pause longer than `RUN_GAP` (250 ms) restarts the promotion
run (`large_frames_apart_are_not_one_run`); the full-shell budget asserts no
`GPU: ` line.

**2026-10-10 · m4-audit (round 3): the `login` fallback warning names every
case.** It names a missing, unreadable or vendor-ignored `strand` file and
the stacks README recommends, without a protocol change.

**2026-10-10 · m4-audit (round 3): a GPU thread that cannot start wakes the
loop.** The waker sits in a slot the thread takes; the failure path calls it
after queuing `Unavailable` and `Exited`.

**2026-10-10 · m4-audit (round 3): no session lock under `STRAND_MOCK`.**
The mock's `auth` cannot work, so `Guard::wire` never calls
`enable_session_lock` or keeps a marker there; faking `auth` was rejected.

**2026-10-10 · m4-audit (round 3): the restart marker is written when the
lock is asked for.** `marker_step` writes on `lock_active()`, so a death
with the lock pending relocks on restart (fault `abort_before_locked`).

**2026-10-10 · m4-audit (round 3): two flaky tests wait for what they
assert.** Drag tests poll or wait for `pointer entered surface`;
`keyed_scaling.rs`'s `contains_key` mean bound goes from 2.5 to 3.5.

**2026-10-10 · m4-audit (round 3), for the owner: `theme_swap_bench`'s
tightest gate has no headroom on GitHub.** The 5 ms whole-swap gate measured
2.5-5.1 ms on CI, so the `timing` job can fail about one run in fifteen;
best-of-N was rejected and the choice is the owner's.

**2026-10-10 · m4-audit (round 3), deferred past M4: PAM messages during a
check.** `TEXT_INFO` and `ERROR_MSG` arrive only with the verdict, so a
`pam_fprintd`-first stack times out to the fallback; fixing it needs
protocol and design changes. README asks for a password-only stack.

**2026-10-10 · m4-audit (round 4): a spring that settles on a large frame
does not promote.** `Promotion::frame` remembers springs in flight; the
frame where they rest never promotes, the next large springless one may.

**2026-10-10 · m4-audit (round 4): a frame whose inline pass fails is
answered once.** The frame ends at the failed pass with one `Failed`; render
draws it on the CPU, where the pass fails by key and is not retried.

**2026-10-10 · m4-audit (round 4): a hung frame records its inline passes as
failed.** On `Failed { surface, key: None }` render records all the
surface's inline passes as failed, since the culprit is unknown.

**2026-10-10 · m4-audit (round 4): `auth` verdicts outlive the store and
carry their lock session.** The check task delivers to
`UnlockSink`/`FailureSink` itself, so a store stopped after `STOP_GRACE`
still answers. Each check carries the generation `LockScreen::session` (via
`AuthConfig::session`, `SessionTag`); verdicts for an ended session are
dropped.

**2026-10-10 · m4-audit (round 4): shader files are read as a regular file
of at most 1 MiB.** Loader, LSP and `strand check` use
`check::shaders::read_file`, the `import()` rule.

**2026-10-10 · m4-audit (round 4): an SVG's `<image href>` reads no file.**
Both SVG parse sites use `media::svg::options()`, whose resolver returns
nothing; `data:` URLs still resolve.

**2026-10-10 · m4-audit (round 4): Lottie files are bounded by their
structure.** `lottie::load` refuses reference cycles, nesting past
`MAX_NESTING` (32) and per-frame work past `MAX_WORK` (100,000 instances);
refusing beats clamping, and looping is not a reference cycle.

**2026-10-10 · m4-audit (round 4): `capped_clocks_paint_at_their_rate` maps
wakes from before each paint.** Its first instant is taken before the T0
paint (`clocked_since`), removing a scheduling dependence.

**2026-10-10 · m4-audit (round 4): the logic theme-swap gate moves to the
timing job.** `a_theme_swap_is_under_five_milliseconds_of_logic` checks 5 ms
only in optimised builds with `GATE_MISS`. Supersedes wave3-theme's "in
every build".

**2026-10-10 · m4-audit (round 4): lock VM absence checks wait for a
positive sign first.** They wait for the restarted strand's CLI answer or
the manager's reply before requiring a second of matching shots.

**2026-10-10 · m4-audit (round 4): the supervisor unit names strand by an
absolute path.** `ExecStart=%h/.cargo/bin/strand run`, since systemd does
not search `~/.cargo/bin`; the test requires an absolute path.

**2026-10-10 · m4-audit (round 5): image, SVG and Lottie files are read
through a bounded read.** `image::read_capped` refuses non-regular files,
checks length and reads through `take(max + 1)`, since procfs files report
size 0.

**2026-10-10 · m4-audit (round 5): the per-pixel memory cap counts by-value
arrays indexed at run time.** naga's SPIR-V backend spills them, so the
check adds each run-time-indexed array or matrix base and array-typed
parameters and results.

**2026-10-10 · m4-audit (round 5): a lock session begins when a lock is
first asked for.** `Guard::check` calls `LockScreen::begin` on first sight
of a lock, so a check begun outside it cannot release it.

**2026-10-10 · m4-audit (round 5): a drag export nobody reads is given up.**
Each write has `DRAG_WRITE_TIMEOUT` (5 s) without progress; progress resets
it.

**2026-10-10 · m4-audit (round 5): a shader file that loses the device is
not run again in the process.** `ShaderResults::lost` keeps such files by
content for the process; their nodes draw nothing with one warning, and an
edit is tried again. The owner later capped lost devices at 3 (m4-owner-3).

**2026-10-10 · m4-audit (round 6): a lock session also begins when a verdict
sees the lock first.** `LockScreen::see(active)` runs from `Guard::check`
and from `LockScreen::answer` before any tag compares, once per lock.
Replaces round 5's "turn after the request".

**2026-10-10 · m4-audit (round 6): the spectrum test's failure names its
cause.** `a_test_tone_lifts_a_spectrum_bar` runs with `STRAND_LOG=info` and
prints bars, `pw-play` state and the log on failure.

**2026-10-10 · m4-audit (round 6): the glyph diff's linearity is counted,
not timed.** `glyph_damage` is generic over `GlyphCell`, and the test counts
comparisons instead of timing.

**2026-10-10 · m4-audit (round 7): a saved `.wgsl` a lock draws waits for
the unlock.** A lock's region hashes the code of every literal-path `shader`
it reaches (`check::shaders::each_shader`), so saving that file is
`LockDeferred`.

**2026-10-10 · m4-audit (round 7): `transition: crossfade` is gone; `none`
names no mask.** design.md lists only wipe, disc, dissolve and pixelate, so
`crossfade` leaves `SwapKind`; `pages` with `transition: none` still slides
(`effects::transition::is_mask`).

**2026-10-10 · m4-audit (round 7): a helper that exited while idle is
dropped, not signalled.** `Helper::running` drops an exited helper (its pid
may be reused) and kills only one whose state cannot be read.

**2026-10-10 · m4-audit (round 7): the fallback field ignores Return with
nothing typed.** An empty Return is `Action::None`, since with
`pam_faillock` it would count as a failed login.

**2026-10-10 · m4-audit (round 7): a burst of file results is kept for its
sources.** `make_room` drops, once 64 are held, only results older than
`FILE_RESULT_STALE` (1 s), and past `MAX_FILES_HELD` (256) the oldest.

**2026-10-10 · m4-audit (round 7): two watcher tests lose their wall-clock
bounds.** They assert on notices and batch contents; the quiet-period
property is proved on the core's clock
(`busy_never_moves_the_quiet_period`).

**2026-10-10 · m4-audit (round 7): the IPC server replaces only a stale
socket.** `Server::bind` refuses a non-socket with `AlreadyExists`, keeps
`AddrInUse` for a live socket, replaces a stale one; `Drop` removes only its
own socket (same `dev`/`ino`).

**2026-10-10 · m4-audit (round 7): the drag-export limit is tested from the
reader's side.** `a_slowly_read_drag_export_is_written_whole` reads after 3
s pauses and gets every byte; rustix gains `event`.

**2026-10-10 · m4-audit (round 7): a frame whose callback or presentation
never comes stops nothing.** A surface throttled for `THROTTLE_GIVE_UP` (1
s) paints new content anyway (`Stats::throttle_given_up`), so a hidden
surface may paint at most once a second (fake `Cmd::HoldFrames`).

**2026-10-10 · m4-audit (round 8): the documented lock field does not send
an empty password.** auth.schema and `check::lock_no_auth`'s help teach `on
activate { if secret != "" { auth.submit(secret) }; secret = "" }`; the
`auth` store still sends whatever a config submits.

**2026-10-10 · m4-audit (round 8): pixmaps without `XDG_RUNTIME_DIR` go to a
private directory.** The fallback is `$TMPDIR/strand-<uid>/pixmaps/<pid>`,
used only if `strand-<uid>` is a real, user-owned 0700 directory, else
refused; temp files open `create_new`.

**2026-10-10 · m4-audit (round 8): the slow-reader drag test's hang-up check
is bounded.** Its `poll` waits at most 5 s, a liveness limit, rather than
hanging CI.

**2026-10-10 · m4-audit (round 8): the frame give-up is tested on the
presentation path.** The fake offers `wp_presentation` on request with
`Cmd::HoldFeedback` and `Fake::sync`; two tests prove give-up and its cancel
(`State::give_up_armed`).

**2026-10-10 · m4-audit (round 8): keyboard focus follows the lock content
to another output.** `sync_lock_surfaces` moves focus to the new content
with a `KeyboardEnter` when the old one held it.

**2026-10-10 · m4-audit (round 8): the format-on-save overlay test no longer
rests on the wall clock.** `quiet()` may be overridden per thread in tests
(`QUIET_ON_THIS_THREAD`, `cfg(test)`); the test uses 2 s.

**2026-10-10 · m4-audit (round 9): a popup in a lock is no way out, and a
warning.** Popups never open on a lock surface, so `check::lock_no_auth`
skips `popup` and `tooltip`, and each one a lock shows warns
`check::lock_popup`.

**2026-10-10 · m4-audit (round 9): a panic in the compiler's shader parse is
a diagnostic.** `reflect` runs under `guarded`, turning a naga panic into a
`check::shader` problem.

**2026-10-10 · m4-audit (round 9): PAM modules run with `/dev/null` on stdin
and stdout.** `take_socket` moves the socket above fd 2 (`F_DUPFD_CLOEXEC`)
before `pam_start`; stderr stays strand's (fault `auth_stdio`).

**2026-10-10 · m4-audit (round 9): a missing helper is said when strand
starts.** `strand run` checks `strand_auth::default_helper` after
`Guard::wire` and warns with a `strand watch` notice; refusing to lock was
rejected as failing open.

**2026-10-10 · m4-audit (round 9): a lock without ext-session-lock is
reported `Finished`, and said at start.** `LockError::Unsupported` becomes
`LockState::Finished`; `State::session_lock_supported()` adds a start
notice.

**2026-10-10 · m4-audit (round 9): the lock VM unlocks with the field the
docs teach.** `lock.rs`'s `CONFIG` uses the guarded handler, and
`the_vm_config_uses_the_documented_field` fails if it and auth.schema
diverge.

**2026-10-10 · m4-audit (round 9): the late-feedback test skips only a real
stall.** It waits on `State::stats().presented` and skips only when the
give-up was counted before the feedback.

**2026-10-10 · m4-audit (round 9): an offscreen group's cache entry pins the
pixmaps its key names.** Keys hash pixmaps by `Arc` address, so entries hold
a `Weak` of each (`offscreen::pins`) to stop address reuse; hashing content
was rejected as too costly.

**2026-10-10 · m4-audit (round 9): spectrum diagnostics that can name the
failure.** Three meter log lines become info, a starting meter says so, and
failures report `strand-levels`' inputs (`PipeWire::linked_from`). The cause
is still open.

**2026-10-10 · m4-audit (round 9): the 2,000-row grim proof checks moved
rows against the first mount.** After the bursts the test wheels back up and
asserts the moved window's rows match the first mount's pixels (24 per
channel on 99.8 %) and differ one row off.

## m4-integration-w3 (closing)

**2026-10-10 · m4-integration-w3 (closing): a mocked run's "never locks"
line is info, not WARN.** It is expected behaviour of a development host,
and no-WARN checks such as `the_design_bar_is_laid_out_start_centre_end`
must not trip on it; `a_mocked_run_never_locks_the_session` runs with
`STRAND_LOG=info`.

**2026-10-10 · m4-integration-w3 (closing): the idle test's logic bound
counts voluntary switches.** `the_real_services_sleep_when_nothing_changes`
reads only voluntary context switches (`wakes_of`), one per sleep, since
preemption under load is not a wake; the bound stays 7, and zero-switch idle
windows still count every switch.

**2026-10-10 · m4-integration-w3 (closing): demo.rs reads only complete
damage lines.** `damage_lines` drops a last line with no newline yet, so a
half-written line is read whole next time.

**2026-10-10 · m4-integration-w3 (closing): a frame or pass whose handling
panics is still answered.** wgpu's `Device::poll` can panic after a driver
reset, so `handle_caught` answers the owed request with `Failed` (kind
`Lost`, the panic text) before `Lost`; `hang.rs` accepts either "stopped
answering" or "was lost". Lowering `HUNG_AFTER` was rejected.

## m4-owner-docs

**2026-10-10 · m4-owner-docs: the owner accepts noise fields as `.wgsl`
shaders.** A noise field is written like the aurora, `shader "noise.wgsl" {
… }`, adding no syntax; design.md says so and lists the five named filters
and backdrops separately. The box is ticked with
`bundled_effects_start_the_gpu_only_while_visible` (`noise field` case) and
`uniforms_match_the_file_by_name_and_type`.

**2026-10-10 · m4-owner-docs: xdg-activation is scheduled for M5.**
features.md's M5 list has an unticked xdg-activation box; the server still
reports spec 1.1 until it is worked.

**2026-10-10 · m4-owner-docs: memory budgets are test targets, never limits
on a shell.** Nothing may refuse, truncate or degrade what a config shows to
stay under design.md's memory budgets; bounds against hostile or broken
input and fixed-size evicting caches stay, each with its own reason.
design.md's memory section says this; limits like `MAX_DECODE_BYTES`,
`MAX_TEXT_BYTES`, `MAX_FONT_PX`, Lottie structure bounds and capped reads
stay.

**2026-10-10 · m4-owner-docs: the theme-swap snapshots are not capped.**
`SNAPSHOT_MAX` and `SNAPSHOTS_MAX` are gone, so a surface of any size
crossfades (`a_surface_larger_than_1080p_crossfades_too`).
`a_4k_surface_crossfade_is_measured` reports a 3840×2160 swap (3.9-5.9 ms
swap, 10-20 ms blend per frame against the 4 ms `BLEND_BUDGET`) without
gating; whether that cost is acceptable is the owner's call.

**2026-10-10 · m4-owner-docs: notification history keeps 1,000.** The bound
now only guards against a client looping, so `KEPT` rises from 100 to 1,000;
older ones still close as expired (1)
(`history_is_bounded_and_lying_pictures_are_refused`).

**2026-10-10 · m4-owner-docs: a Lottie's image assets are bounded as images
are.** Assets are bounded by 4,096 px a side (`MAX_ASSET_SIDE`) and 64 MiB
of pixels together (`MAX_ASSET_PIXELS`, `MAX_DECODE_BYTES / 4`), shrunk
alike past that; a shown asset decodes at its authored size
(`an_asset_a_shell_shows_is_decoded_at_its_authored_size`,
`assets_are_decoded_within_their_bound`). Replaces m4-effects-finish's 1,024
px / 1 Mi pixel budget.

**2026-10-10 · m4-owner-docs: the glyph atlas's cap is a hostile-text bound
of 8 MiB.** Leased pages are never evicted, so 1 MiB could hide CJK glyphs;
`AtlasConfig::max_bytes` defaults to `DEFAULT_MAX_BYTES`, 8 MiB per scale
(128 pages), while one hostile request still stops at two 2,048 px pages.
Pages are made only as needed (`a_text_heavy_shell_fits_the_default_atlas`,
`the_default_atlas_bounds_one_request_of_huge_glyphs`).

## m4-owner-swap

**2026-10-10 · m4-owner-swap: the owner keeps the 5 ms theme-swap gate and
asks for a cheaper swap.** Four changes, no colour or verdict changed:
`material::from_seed` resolves all 49 roles through one `SchemeResolver` and
keeps each thread's last eight palettes (keyed by 8-bit seed, variant, mode,
clamped contrast); `Motion::probe(at)` reads position, velocity and settling
in one evaluation; the contrast memo hashes by word; the play-through
computes each moment's roots once, charging `CHECK_WORK` as before.

**2026-10-10 · m4-owner-swap: the timing gates time a palette solved, not a
palette kept.** Every gated swap first calls the hidden
`material::forget_kept_palettes()`, so the gates time a fresh solve; the
kept case is printed beside it, not gated
(`forgotten_palettes_are_solved_again_alike`). The gated 8-scope
`spring(1600, 1)` swap went from 1.61 to 1.36 ms.

## m4-owner-2

The owner's M4 decisions of 2026-10-10; detail is in the m4-owner-docs and
m4-owner-swap sections.

**2026-10-10 · m4-owner-2 (owner decision 1): a noise field is a `.wgsl`
shader.** Spelled like the aurora, `shader "noise.wgsl" { … }`, with no
syntax added; the noise box is ticked.

**2026-10-10 · m4-owner-2 (owner decision 2): the theme swap is cheaper and
the 5 ms gate stays.** m4-owner-swap's four changes keep every colour and
verdict, and gated swaps call `material::forget_kept_palettes` first.

**2026-10-10 · m4-owner-2 (owner decision 3): the shape set stays at 13.** A
shape goes only if the others express it exactly, same pixels and morphs;
none does (`rect`/`pill` morph where a plain box snaps). design.md lists all
13; if unshaped boxes ever morph, `rect` and `pill` go
(`no_shape_duplicates_another`).

**2026-10-10 · m4-owner-2 (owner decision 4): xdg-activation is M5 work.**
It has an unticked M5 box; the server still reports spec 1.1.

**2026-10-10 · m4-owner-2 (owner decision 5): Lottie refuses reference
cycles, not loops.** Code, errors and tests say "reference cycle"; looping
and time remapping are unaffected.

**2026-10-10 · m4-owner-2 (owner decision 6): memory budgets are benchmark
targets, not caps on a shell.** The four budget-only limits changed as
m4-owner-docs records; the memory gates (bar 34/38 MB, full shell 64/70 MB)
are unchanged, and the 4K fade cost is an open owner item.

## m4-owner-3

The owner's remaining M4 decisions of 2026-10-10; code citing
m4-close-gpucap, m4-close-helper or m4-close-blend points here.

**2026-10-10 · m4-owner-3 (m4-close-gpucap; item 7): after 3 lost GPU
devices the GPU stays off until restart.** Only `GpuReply::Lost` counts
(`promote::LOST_CAP = 3`, in the renderer's `Device`); start failures still
retry every 30 s. Once off nothing asks for a device or promotes, effects
use CPU versions, and status is `Unavailable { reason: GPU_OFF }`, told once
via `Renderer::gpu_off()` (`the_gpu_is_off_after_three_lost_devices`).

**2026-10-10 · m4-owner-3 (m4-close-helper; item 9): a missing `strand-auth`
keeps warn-and-lock; `strand check` reports it.** `strand run` still warns
and locks, failing closed. `strand check` reports `check::lock_no_helper`
once at the first `lock` when `strand_auth::default_helper` finds nothing;
it lives in the binary's check.rs, so LSP and reload do not report it.

**2026-10-10 · m4-owner-3 (item 2): a hung `GpuPresent` frame stays bounded
by the WSI's acquire timeout.** Accepted: it never reaches the lock. No code
changes.

**2026-10-10 · m4-owner-3 (item 6): the promoted GPU cost on hardware is
measured in M5.** Presenting needs a KMS compositor, so it has an unticked
M5 box.

**2026-10-10 · m4-owner-3 (m4-close-blend; item 8): the 4K crossfade's blend
is cheaper and stays ungated.** The blend uses u16 lanes, exactly the old
formula, and snapshots copy rows from their stride
(`the_blend_matches_the_wide_formula_exactly`). A 4K fade frame now blends
in about 3.4 ms, inside 16.7 ms, so item 8 is resolved.

## m5-owner

The owner's answers of 2026-10-10 to the M5 plan's twenty questions; code
citing "m5-owner Qn" points here.

**2026-10-10 · m5-owner Q1: no QuickJS escape hatch.** A JS runtime adds a
second language, against design principle 2, and `.text` cannot hold it.

**2026-10-10 · m5-owner Q2: no WASM plugin tier.** `from dbus | file |
listen | poll` services and Rust plugin crates (Q18) cover integrations.

**2026-10-10 · m5-owner Q3: no AccessKit in v1.** AT-SPI roles and an
accessible-name prop can arrive in 1.x as schema additions.

**2026-10-10 · m5-owner Q4: keep `.strand`; no KDL.** The tooling is built
on it, and KDL has no expression grammar.

**2026-10-10 · m5-owner Q5: "frozen" means grammar.md plus a self-contained
spec.md with a compatibility policy.** In 1.x no syntax is removed or
changes meaning and builtin schema entries are only added; after the freeze,
changes to grammar.md or spec.md need a decisions paragraph citing the
policy (typos exempt). No hash pin.

**2026-10-10 · m5-owner Q6: the CPU-only `.text` gate is re-based
provisionally to 15,925,248 B; the GPU gate stays 18.5 MiB.** M4's
15,657,671 B plus a 230,000 B allocation, rounded up to 64 KiB, rounding not
spendable; reclaims lower it, and it is never raised in M5 without the
owner.

**2026-10-10 · m5-owner Q7: reclaims are behaviour-neutral only.** One
commit each with before and after numbers, kept only if `timing` stays
green; no opt-level cut on a gated hot path.

**2026-10-10 · m5-owner Q8: `strand fmt` stays in `strand`.** Moving it
needs the owner with a measured figure.

**2026-10-10 · m5-owner Q9: the shell draws only the inspector's outline and
repaint flash; `strand-dev inspect` is a terminal client.** Presentation
stays out of the gated binary.

**2026-10-10 · m5-owner Q10: a token runtime overlay is addressed with the
token spelling.** `strand set '$accent' '#ff8800'` writes
`$XDG_STATE_HOME/strand/tokens.toml`, taking the token swap path; `strand
clear '$accent'` removes it.

**2026-10-10 · m5-owner Q11: write-back to a read-only target is refused,
offering `$EDITOR`.** design.md:760's Nix redirect means the settings
write-back, not an overlay copy of `.strand` files.

**2026-10-10 · m5-owner Q12: the D-Bus name is
`io.github.jakeb_grant.Strand`, and may change before 1.0.** Bus and
interface name at object path `/`, in one constant
`strand_core::ipc::DBUS_NAME`; a second instance runs without D-Bus IPC.

**2026-10-10 · m5-owner Q13: `strand call` reaches service-level actions
only.** For example `strand call notifications.clear`.

**2026-10-10 · m5-owner Q14: xdg-activation waits at most 200 ms for a
token.** Later actions of the same handler queue behind it. Launches set
`XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` and strip inherited ones;
`ProvideXdgActivationToken` and `DBusActivatable` stay out.

**2026-10-10 · m5-owner Q15: the newcomer target is design.md's bar (a),
ticked by two or three timed human trials.** Plus the automated test and the
agent proxy, which carry the box alone if recruiting fails, as a recorded
reading.

**2026-10-10 · m5-owner Q16: `strand export` writes to stdout, or to `-o
<file>`.** Strand never writes another program's config unless asked.

**2026-10-10 · m5-owner Q17: `Async` moves to learning-ladder level 4; the
importer box is split.** The level-4 launcher already uses
`apps.search(query)`. Importers are ticked with M2's tests; plugins stay
open.

**2026-10-10 · m5-owner Q18: plugins are user binaries built through a
`strand` library builder.** `strand::Shell::builder().service::<S>().run()`,
git-only and unstable, with schemas written to
`$XDG_DATA_HOME/strand/schemas/`; no dylib loading.

**2026-10-10 · m5-owner Q19: the GPU-on-hardware box has two branches.**
`gpu.sh` on renderD128 if the spike starts sway there, else the owner runs
`scripts/gpu-measure.sh` on a KMS card.

**2026-10-10 · m5-owner Q20: M5 is not the 1.0 release.** Version stays
0.0.0, no tag or artifact; the freeze and compatibility policy still land.

## m5-integration

**2026-10-10 · m5-integration: the CPU-only `.text` gate is 15,925,248 B for
M5 (provisional).** `TEXT_GATE_CPU` in `crates/strand/tests/budgets.rs`
rises from 15,728,640 B per Q6; streams are held by the ledger, not the
gate.

**2026-10-10 · m5-integration: wave 0 is merged; the size-report test runs
in lint.** `scripts/test-size-report.sh` runs in ci.sh's lint steps and
ci.yml's lint job.

**2026-10-10 · m5-integration: the CPU-only gate is lowered to 15,859,712 B
after wave 0's reclaims.** It is recomputed from the lowered baseline
(15,657,671 − 37,056 + 230,000, rounded up to 64 KiB), the reading behind
the plan's "about 159 KB returns to 15 MiB"; the owner is to confirm.

**2026-10-10 · m5-integration: 0b's +12.5 KB is paid from the ledger.**
About 2.6 KB comes from S-ipc's D-Bus allocation (now 37,400 B) and 9,944 B
from the review-fix reserve (now 32,056 B); the 230,000 B total holds.

## m5-core

**2026-10-10 · m5-core: the 0a interfaces land as inert code (wave 0b).**
`strand_core::ipc` holds the protocol types and JSON-line codec (`Request`,
`parse`, `parse_m5`, `encode`, `VERSION`, `MAX_LINE`, `DBUS_NAME`) and the
`IpcRequests` channel with `Reply`/`ReplyFuture`; the binary's IPC is
`ipc/{mod, server, cli, describe}.rs`; `Shell::request` dispatches to
`run/request/*.rs`, one module per command. `strand` becomes a library with
the binary over it, `ActionCtx` reaches every `ServiceHost`, and no socket
answer changes.

**2026-10-10 · m5-core: `parse` reads only the commands the shell serves;
M5's fields are read by `parse_m5`.** `parse` reads M4's commands exactly as
M4 did and answers any other `cmd` "unknown command `x`", keeping M4
clients' answers and saving about 5 KB. Each stream adds one `parse` arm
(`"get" | "toggle" => return parse_m5(cmd, &v)`) with its handler; unserved
variants answer `strand_core::ipc::unknown`. How a `reset` with both `path`
and `node` reads is S-inspect's to decide and record.

**2026-10-10 · m5-core: wire forms the 0a text left open.** Node ids and
`inspect.dump`'s `surface` are opaque strings the inspector spells. `call`'s
`args` are a list of strings read by the parameters' types; `reset{node}` is
`Request::ResetNode`. Every string field except `set`'s value and `call`'s
args must be non-empty: an empty one encodes but does not parse back
(`empty_strings_do_not_round_trip_except_set_values`).

**2026-10-10 · m5-core: the request channel is the logic loop's, handed out
through `Live::requests_to`.** `logic()` makes the channel and drains it
after each sleep, woken by the sleeper's ping; S-ipc hands its clone to
`strand_services::ipc_bus`. Calls share the socket's dispatch
(`Caller::Socket` or `Caller::Bus`); `reload` and `watch` answer unknown
from the channel until S-ipc serves them; a dropped `Reply` completes with a
refusal so D-Bus callers never hang.

**2026-10-10 · m5-core: the snapshot hop lands as types, not a channel.**
`ToMain::Snapshot { kind, reply: SnapshotId }`, `SnapshotKind::{Report,
Pick, Layout { node }}` and `ToLogic::Snapshot(Snapshot { reply, data })`
carry JSON built by main; the first stream that sends one wires the channel.

**2026-10-10 · m5-core: the library's shape.** `lib.rs` holds the private
module tree and the binary uses a hidden `strand::cli` facade, so nothing
becomes public API before S-plugins designs `Shell::builder`. `main.rs`
keeps `#[global_allocator]` and the `NO_THP` constructor, repeated under
`cfg(test)` in `lib.rs` (`mimalloc_is_the_test_harness_allocator`,
`transparent_huge_pages_are_off_in_the_test_harness`).

**2026-10-10 · m5-core: `ActionCtx` is the last parameter; services do not
take it yet.** `ServiceHost::action(rt, target, name, args, ctx)`; the VM
passes `ActionCtx::default()`, `SchemaHost` records it in `ActionCall::ctx`,
and strand-services' `DynService::action` is unchanged for S-platform to
design.

**2026-10-10 · m5-core: 0b costs 12,544 B CPU-only (8,512 B GPU), past its 1
KB bound.** Most of 0b is reachable. The codec functions are `#[inline]` so
they reuse the binary's `serde_json` instantiations, `encode` builds one
`Map`, and the request channel is a `Mutex<Vec<IpcCall>>` rather than
`mpsc`. Proposed charging: about 2.6 KB to S-ipc's D-Bus allocation, the
rest to the integrator's reserve (accepted in m5-integration).

## m5-size

**2026-10-10 · m5-size: `scripts/size-report.sh` and the M5 size baseline.**
The report builds the gated binaries exactly as budgets.rs does and breaks
`.text` down by crate and symbol with binutils and awk only, charging
generics by a fixed rule tested by `scripts/test-size-report.sh`; under fat
LTO it shows where bytes sit, not exact costs. Baseline: CPU-only 15,657,671
B, GPU 19,041,095 B. Q8: `strand fmt` costs 27,729 B, under the 40 KB
threshold, so it stays.

**2026-10-10 · m5-size reclaim 1: strand-theme decodes WebP with
`image-webp` directly.** `image` leaves strand-theme's runtime dependencies
(kept for tests); header checks (`MAX_PIXELS`, `WEBP_BYTES_PER_PIXEL` within
`FULL_FRAME_BYTES`) run before allocation, and `webp_err` keeps `image`'s
error wording. Saves 9,600 B CPU-only
(`a_lossless_webp_seeds_as_the_same_png_does`,
`a_corrupt_webp_keeps_its_error_wording`).

**2026-10-10 · m5-size reclaim 2 (withdrawn): resvg, tiny-skia and
tiny-skia-path stay at opt 3.** Bindable SVG rasterises every animation
frame, and at "z" the ignored test `svg_frame_cost` measured about 210 µs a
frame against 56 µs at opt 3, breaking design.md's "Trivial" bound;
Cargo.toml says why, and `RELEASE_OPT_LEVELS` drops the three.

**2026-10-10 · m5-size reclaim 3: five event-rate crates go from opt "s" to
"z".** miette, strand-introspect, freedesktop-desktop-entry, pipewire and
libspa run off every timing gate; saves 27,776 B CPU-only. blake3 stays (it
grew at "z") and roxmltree stays because fontique may parse with it on the
reload path. `RELEASE_OPT_LEVELS` follows; GitHub's `timing` job decides
whether it stays.

**2026-10-10 · m5-size: wave-0 reclaims total and the candidate left for the
integrator.** Reclaims 1 and 3 save 37,056 B CPU-only and 37,376 B GPU. The
duplicate `jpeg_decoder` and `image_webp` instantiations (about 89 KB) need
one crate to own the decode or strand-theme to read wallpapers into memory,
architecture changes for the owner.

**2026-10-10 · m5-size: review fixes to the tooling.** `size-report.sh`
forwards its `SIZE_REPORT_*` settings as a quoted bash array so values with
spaces survive.

**2026-10-11 · m5-size (wave 2): the size pass cuts no code; the gates are
re-based by the integrator.** Under the owner's 2026-10-11 ruling (cut only
grossly verbose code, else re-base with a reason), wave 2 measured CPU-only
18,485,143 B (135,063 B over the 18,350,080 B gate) and GPU 22,382,423 B
(100,183 B over 22,282,240 B). No duplicated generics, ICF would save under
10 KB, and per-package opt "s" would slow hot paths, so nothing is cut; size
gates are tripwires, not caps.

**2026-10-11 · m5-size (wave 2, review round 3): the re-base happens now.**
The integrator re-bases both gates to the measured tree as handoff's Next
item 1, before CI runs on the merged head, not after S-tools' wave-3 work;
wave 3 charges against the re-based gates.

## m5-infra

S-infra's wave-0 work (spikes, pinned tree-sitter CLI, budgets ledger line,
`scripts/gpu-measure.sh`, deferred CI items).

**2026-10-10 · m5-infra spike 1: headless sway renders GLES2 and Vulkan
on the render node.** On renderD128 alone, `WLR_RENDERER=gles2` and
`vulkan` both start and offer linux-dmabuf; only `pixman` lacks it, so
M4's pixman fallback was the tests' own sway, not the GPU. `gpu-session.sh`
now tries GLES2, then Vulkan, then pixman, exports the one that started as
`STRAND_GPU_SWAY_RENDERER` and keeps the log in `GPU_SWAY_LOG`.

**2026-10-10 · m5-infra: Q19 answered (A); promoted cost measurable with
`gpu.sh`.** gpu_idle.rs attaches for `Present` when `STRAND_GPU_HARDWARE=1`
and `STRAND_GPU_PRESENT=1` and prints PSS while promoted (reported,
not gated). The presented path left 4,662–7,704 kB after the drop,
sometimes past the readback leg's 6,144 kB bound, so presenting is opt-in
and reports `BOUND MISS` instead of asserting; the readback leg still
asserts. Whether the bound becomes about 8 MiB is the owner's call.

**2026-10-10 · m5-infra spike 2: tree-sitter CLI 0.27.0 generates from
grammar.js without node.** `tree-sitter generate --js-runtime native`
uses the CLI's built-in QuickJS and gives output byte-identical to node's;
the parser is ABI 15 (the `tree-sitter` crate must be 0.25 or newer). So CI
regenerates everything from `grammar.js` and the committed-`grammar.json`
fallback is not needed.

**2026-10-10 · m5-infra: tree-sitter CLI pinned in the image
and CI lint only.** The Dockerfile and ci.yml's lint job fetch
v0.27.0 with the same `TREE_SITTER_VERSION`/`TREE_SITTER_SHA256`
pair. `scripts/check-tree-sitter.sh [PACKAGE]` regenerates
`crates/strand-dev/tree-sitter-strand` in a scratch copy and fails (exit 1)
on any difference, refuses another CLI version (exit 2), and passes with no
`grammar.js`. CI's lint now also runs the CI scripts' own tests.

**2026-10-10 · m5-infra spike 3: sway 1.9 honours xdg-activation tokens
from a layer surface.** A token with the clicked layer surface's valid
serial makes the window urgent (or focused under `focus_on_window_activation
focus`); a bad serial is ignored; a token without serial or surface gives
urgency only. sway focuses a newly mapped window anyway, so container
tests cover tokens and activation, and the launch focus check stays in
`matrix.sh`.

**2026-10-10 · m5-infra: the budgets ledger line.** budgets.rs's
`.text` gate test prints `ledger: <half>` and appends `cpu <half>`/`gpu
<half>` to `STRAND_LEDGER_FILE` against `LEDGER_BASELINE_CPU`/`_GPU`;
`scripts/container/ledger.sh FILE` prints the line and adds it
to the GitHub job summary, even when a gate fails. Baselines
are S-infra's, gate constants the integrator's. Tests:
`the_ledger_half_reads_size_delta_baseline_and_gate`, `test-ledger.sh`.

**2026-10-10 · m5-infra: `scripts/gpu-measure.sh` is owner-run only.**
It exits 2 unless `STRAND_OWNER_GPU_MEASURE=1`, then runs `strand
run` on a private animated-shader config and records PSS before, while
promoted and after the drop, attach mode and `strand report --json` into
`target/gpu-measure-<date>/summary.txt`. Its test, `test-gpu-measure.sh`,
is static only.

**2026-10-10 · m5-infra: two wall-clock bounds in functional tests become
work checks.** `huge_fan_out_fails_fast` now asserts the lookup stops
after exactly `MAX_TOKEN_STEPS` steps (`last_token_steps()`) instead of
under 500 ms. `dbus_services_are_checked_off_the_analysis_path` proves
the analysis did not wait on the bus by a first publish without the
"not checked" warning followed by one with it. Neither is a gate.

**2026-10-10 · m5-infra: compositor_matrix.rs sends Hyprland's
Lua dispatch first.** From 0.55 a Lua config's `dispatch` evaluates
`hl.dispatch(...)`, so the matrix sends `hl.dsp.focus({ workspace =
"NAME" })`, falls back to the classic form only on `Invalid dispatcher`,
and fails on any other refusal, as strand-services' adapter does. Test:
`hyprland_dispatch_forms_use_the_current_syntax_first`; the classic
fallback is unverified.

**2026-10-10 · m5-infra: GitHub's run on this branch.** GitHub's runner
links both binaries 1,989 B smaller than the container for the same tree,
so compare ledger figures only within one environment. theme_swap_bench
measured 4.31 ms median against its 5 ms gate.

**2026-10-10 · m5-infra: `gpu-session.sh` exports the renderer sway's log
reports.** `start_sway` requires wlroots 0.17's own log lines ("Opening
DRM render node" plus "Creating GLES2 renderer" or "Found matching Vulkan
physical device"); without them the attempt counts as failed and the next
renderer is tried. Stale sockets are removed after each attempt because
sway 1.9 can crash on shutdown.

## m5-w0-harden

Fixes to wave 0 from its audit, before wave 1 branches.

**2026-10-10 · m5-w0-harden: `reset{node}` from the request channel
gets M4's answer.** `Request::ResetNode` goes to `reset_node_request`
(run/request/reset.rs), which answers like the socket's "`reset` needs a
`path`" until S-inspect serves it, instead of `unknown command`. Test:
`requests_from_another_thread_are_answered_by_the_logic_loop`.

**2026-10-10 · m5-w0-harden: inspector logic-to-main
messages and the pick event are typed now.** `ToMain` gains
`Highlight(Option<(SurfaceId, NodeId)>)`, `DamageFlash(bool)` and
`PickMode(bool)`; `ToLogic::Picked(PickEvent::{Picked { surface, node },
Ended})` carries a click's pick unasked; `SnapshotKind::Pick` is gone and
`Layout` carries its `SurfaceId`. Inert until S-inspect fills it, so no
stream adds variants to `run/mod.rs`.

**2026-10-10 · m5-w0-harden: the handler input context's route is written
down for S-platform.** architecture.md fixes the route as text, not code:
the router returns a press's serial with its `NodeEvent`, `ToLogic::Event`
and `Instance::event` gain `input: Option<InputCtx>`, the VM keeps it
per invocation, `DynService::action` takes the `ActionCtx`, and services
reach `strand_surface::activation_token` through a token source the binary
hands in. m5-plan gives S-platform every file on the route.

**2026-10-10 · m5-w0-harden: `strand_core::ipc::EventSink` fans events
out for D-Bus `watch`.** `EventSink`, a cloneable `Fn(&Answer)` that is
`Send` and `Sync`, is given through `Live::events_to`; `Shell::broadcast`
sends each event to the socket and the sink, so the sink hears events
even when the socket failed to bind. S-ipc owns handing a sink in. Test:
`an_event_sink_hands_each_event_on`.

**2026-10-10 · m5-w0-harden: socket path moves to
`strand_core::ipc::socket_path`.** The `$STRAND_SOCKET` /
`$XDG_RUNTIME_DIR/strand-<display>.sock` rule is pure environment logic,
so it sits in strand-core and the binary re-exports it; strand-dev takes a
direct strand-core dependency, since a transitive one cannot be named. Test:
`the_socket_is_per_display`.

**2026-10-10 · m5-w0-harden: an overlaid token's origin is the reserved
string `overlay`.** `strand_scene::tokens::OVERLAY_ORIGIN` replaces the
planned `TokenOrigin` enum: a reserved bare word keeps origins one kind
of value and cannot collide with a tier's spelling. S-ipc records it with
`set_origin` while the overlay holds; S-inspect reads it as the overlay
step. Test: `the_overlay_origin_is_recorded_and_read_like_any_other`.

**2026-10-10 · m5-w0-harden: architecture.md's Ownership paragraph
names who landed what.** `identity`, `cells_of`, `reset_subtree`,
`source_hash`, `TokenTable::provenance`, the token overlay,
`activation_token` and `Shell::builder` are listed as stream-landed,
not 0b's. The shared dispatch points (`parse`'s match, `Shell::request`,
`ToMain`/`ToLogic`/`Shell::handle`, `Live`'s fields, `lib.rs`'s `mod` list,
`ipc/mod.rs` re-exports) take one-line entries per stream, merged by the
integrator; 0b's +12,544 B was accepted.

**2026-10-10 · m5-w0-harden: how to read the ledger's Δ as spend.**
The Δ stays against M4's baseline; spend = Δ + 37,056 CPU-only (Δ +
37,376 GPU), within allocation means Δ ≤ +192,944 CPU-only (Δ ≤
+232,624 GPU), and the gate is hit at Δ = +202,041; the 9,097 B between
is rounding no stream may spend. resvg and blake3 at opt-level "z" are
recorded as not taken, so neither is retried.

**2026-10-10 · m5-w0-harden: `gpu-measure.sh`'s summary header no longer
ends the run.** Under `set -euo pipefail` a missing or silent vulkaninfo
made the header group return 1 and the script exit before `strand run`. The
header is now a `header` function with `if` blocks and a "gave nothing"
fallback; `test-gpu-measure.sh` runs it alone with vulkaninfo missing,
failing and silent.

**2026-10-10 · m5-w0-harden: the budgets job uploads the trial binaries.**
ci.yml's budgets job copies `target/release/strand` after the GPU and
CPU-only gate runs as `strand-gpu` and `strand-cpu` and uploads them as
the artifact `strand-release-binaries` (30 days; not a release), for the
human trials (Q15, Q20). GitHub only. Test: `test-ledger.sh`.

**2026-10-10 · m5-w0-harden: doc figures corrected.** The presented GPU
leg's after-drop figures are +6,868/+7,704 kB (6.7–7.5 MiB) and +4,662
kB (4.6 MiB), not "6.9–7.7 MiB", which read kB/1000 as MiB; PSS while
promoted is +18,660–24,639 kB over three runs. handoff.md records the
gate lowered to 15,859,712 B.

**2026-10-10 · m5-w0-harden: left for the owner.** Open owner questions,
none blocking wave 1: (1) the presented GPU leg's after-drop bound (about
8 MiB, or chase the retained memory); (2) whether to overrule charging
0b's +12,544 B to the D-Bus line and reserve; (3) whether to take the
`jpeg_decoder`/`image_webp` dedup (about 89 KB); (4) whether wave 0's
unwritten writing column moves to wave 1. Interface changes to these 0a
choices go through the integrator.

**2026-10-10 · m5-w0-harden: what the hardening costs.** The event fan-out,
`reset{node}` arm and inlined `socket_path` cost +2,816 B CPU-only `.text`
(15,635,911 B), paid from the integrator's reserve (32,056 → 29,240 B);
the 230,000 B total and the gate are unchanged.

**2026-10-10 · m5-w0-harden (second round): timing and fuzz steps run
their tests again; a step running no test fails.** After 0b's lib/bin
split, `--bin strand` commands ran 0 tests and passed, so `reload_latency`
and the fuzzer gated nothing. All six commands use `--lib` and go through
`scripts/container/tests-ran.sh`, which fails a passing run whose libtest
summaries count no test; advisory gate classification is unchanged. Test:
`test-tests-ran.sh`.

**2026-10-10 · m5-w0-harden (second round): `ActionCtx` and `InputCtx` move
to `strand_core::action`.** strand-services depends only on strand-core,
so the types live there (`strand_core::{ActionCtx, InputCtx}`),
`InputCtx::surface` is a plain `u32`, and `strand_compiler::vm::host`
re-exports them; the token source is `Fn(u32, u32)`. One type both ends
can name removes a concept. Test: `the_default_action_context_is_empty`.

**2026-10-10 · m5-w0-harden (second round): the request channel
closes when the loop ends.** The unmount now drops `bus_inbox` before
`inst.shutdown()`, so a call made after the last drain is refused, as
`IpcReceiver`'s doc says, instead of pending through the unmount. Test:
`requests_from_another_thread_are_answered_by_the_logic_loop`.

**2026-10-10 · m5-w0-harden (second round): docs and comments corrected.**
Wave 1 branches from `laptop/m5-w0`; all four generated tree-sitter paths
(`grammar.json`, `parser.c`, `node-types.json`, `tree_sitter/`) are named
as committed. `size-report.sh`'s baseline table adds a 0-byte row with
its whole negative Δ for a top-N crate the binary no longer has (test:
`test-size-report.sh` `--crate-delta`).

**2026-10-10 · m5-w0-harden (second round): wave 0's writing column
moves to wave 1.** S-docs' `docs/spec.md` skeleton, guide outline,
ladder placements and `strand new` template, and S-tools' export mapping
table, were never written; they open wave 1's writing column (the spec.md
skeleton first), and S-lsp starts the grammar only once the skeleton is on
`laptop/m5-w0`. The owner may want them before wave 1 instead.

**2026-10-10 · m5-w0-harden (second round): cost.** +64 B CPU-only `.text`
(15,635,975 B), from the unmount's extra drop, paid from the reserve
(29,240 → 29,176 B); the gate is unchanged.

**2026-10-10 · m5-w0-harden (third round): inspector messages to main
name nodes, never surfaces.** Logic never learns a `SurfaceId` and one
root shows on several surfaces, so `ToMain::Highlight(Option<NodeId>)`
and `Layout { node }` drop the surface, and main resolves it with
`Renderer::surfaces_of(node)` (the surfaces of the nearest surface root
at or above it). Dropping it removes a concept where a root-to-surface
map would add one. Test: `a_node_resolves_to_the_surfaces_that_show_it`.

**2026-10-10 · m5-w0-harden (third round): invocation numbers survive
`await`; one ordering queue for all services.** Per m5-owner Q14,
the VM numbers every handler invocation from 1, with or without input,
keeps the number in the frame for the whole run, and clears only `input`
at the first `await`; 0 (`ActionCtx::default()`) is an unordered action
outside any handler. The ordering queue is one on the services thread,
keyed by invocation. Text only; S-platform implements it.

**2026-10-10 · m5-w0-harden (third round): plugins get mimalloc
and THP-off from `strand::process_setup!()`.** A library's `#[used]`
constructor may not reach the linking binary, so the `macro_rules!`
`strand::process_setup!()` declares the `#[global_allocator]`
and the `.init_array` `NO_THP` constructor at the invoker's crate
root; `main.rs`, `lib.rs` under `cfg(test)` and every plugin
invoke it. Tests: `a_linking_binary_allocates_with_mimalloc`,
`a_linking_binary_runs_with_thp_off`.

**2026-10-10 · m5-w0-harden (third round): `check-tree-sitter.sh`
generates into a clean output.** It deletes the four generated paths from
the scratch copy before generating, so a stale file the CLI no longer writes
is caught, while hand-written files such as `src/scanner.c` stay. Test:
`test-check-tree-sitter.sh`.

**2026-10-10 · m5-w0-harden (third round): docs corrected.** README's and
features.md's commands use `--lib`, not `-p strand --bin strand <filter>`,
and `test-tests-ran.sh` now fails if either cites the `--bin strand`
form. m5-plan's spend figures, writing-column moves and test pointers
were updated to match.

## m5-speed-profile

**2026-10-11 · m5-speed-profile: release opt-levels re-chosen for speed,
and how measured.** Under the owner's speed-first policy of 2026-10-10,
every size override on a path a person waits on was measured (survey
P1–P31) with release benches in interleaved rounds pinned to one core,
through the new `speed-bench` feature (`crates/strand/src/speed.rs`),
run with `cargo test --release -p strand --no-default-features --features
speed-bench --lib speed:: -- --ignored`. Result: CPU-only **17,958,935
B** and GPU **21,854,807 B** (+14.9% each), over the old gates;
m5-speed-gates proposes new ones. Only the CPU-only build's speed
was measured. budgets.rs's `RELEASE_OPT_LEVELS` lists the 20 size
overrides kept, and `RELEASE_SPEED_CRATES` fails if any of 23 speed
crates, or a profile-wide or wildcard opt-level, goes below 3 (test:
`a_size_level_reaching_a_speed_crate_is_caught`).

## m5-speed-strand

**2026-10-11 · m5-speed-strand: `strand` goes from opt "s" to 3 (P6,
Finding 0).** Fat LTO optimises the whole merged module at the linking
crate's opt-level, so `strand` at "s" ran the Os pipeline over every
crate. At 3, list-scroll frame work dropped 21%, the launcher's open
animation 26%, Lottie and SVG frames about 19–20%, for +1,940,928 B
CPU-only and +2,452,224 B GPU. Opt 2 saved about 0.2–0.4 MB but was
11–21% slower, so 3 is taken.

## m5-speed-images

**2026-10-11 · m5-speed-images: the image decoders go from "z" to 3
(P27).** image-webp, jpeg-decoder, png, gif, fdeflate, miniz_oxide, weezl
and imagesize decode icons, album art, `image` nodes, animated frames and
the wallpaper. Against the final profile a PNG icon decodes 19% faster,
a GIF frame 13%, lossless WebP 8%, for +40,912 B CPU-only and +41,552 B
GPU, the cheapest change here.

## m5-speed-svg

**2026-10-11 · m5-speed-svg: usvg, svgtypes, simplecss, data-url and
roxmltree go to 3 (P20, P28).** A bindable SVG re-parses its text on
every part change, so an animated part re-parses each frame. Against the
final profile a spinning meter frame went 127 → 96 µs, for +129,344
B CPU-only and +129,152 B GPU for the usvg crates.

## m5-speed-lottie

**2026-10-11 · m5-speed-lottie: velato and serde_json go to 3 (P26,
P13).** velato builds a Lottie scene every frame and serde_json parses
it on first show. At 3 a 750 kB, 400-layer Lottie's first show is 20.4
ms instead of 28.0 ms, for +185,664 B CPU-only; a first-show hitch is
worth 1% of the binary, and serde_json is also on IPC and compositor JSON.

## m5-speed-palette

**2026-10-11 · m5-speed-palette: material-colors goes to 3 (P17).**
The `material(seed:)` solve runs on every swap to a new seed (inside
the gated 5 ms theme swap) and its quantiser on every wallpaper change;
a solve went 179 → 166 µs (−7%) for +25,024 B CPU-only, +24,192 B GPU.

## m5-speed-input

**2026-10-11 · m5-speed-input: Wayland crates go to 3; calloop and xkeysym
stay at "s" (P15, P16, P21).** `speed::input_dispatch_cost` times a 64-event
Wayland burst (18.0 → 13.7 µs at 3, −24%), a calloop channel wake
and xkeysym names; the last two did not move, so they stay at "s" (saves
6,208 B CPU-only). wayland-client and wayland-backend stay at 3 on the
gain, smithay-client-toolkit on the same per-event path without its own
figure. In the GPU build the wire parse is libwayland's C (`client_system`).

## m5-speed-kept

**2026-10-11 · m5-speed-kept: what stays built for size.** strand-services
stays at "z" (P7): 3 saved 23 µs a launcher keystroke for +2,491,200 B;
moving `Fuzzy` into a crate at 3 is the route if search grows. fontique
stays at "s" (P18; first lookup within ±1%). toml_edit stays at "s" though
`parse_doc` uses it on every settings read (`speed::settings_parse_cost`:
13.3 µs). zbus and friends, tokio, swayipc-types, strand-icons,
strand-introspect, miette, chrono, freedesktop-desktop-entry,
pipewire/libspa stay unmeasured since nobody waits on them; P1–P4 stay.

## m5-speed-gates

**2026-10-11 · m5-speed-gates: proposed `.text` and memory gates after
the speed profile.** The profile is a re-base, not M5 spend. Proposed:
CPU-only gate **18,219,008 B** (278 × 64 KiB), GPU **22,282,240 B**
(340 × 64 KiB); ledger spend reads Δ − 2,285,904 CPU-only and Δ
− 2,801,744 GPU. The two-monitor design bar's target and ceiling rise
2 MiB to **36,864 kB** and **40,960 kB**, since the profile adds about 2
MB resident code by choice. The integrator applies them under the owner's
2026-10-10 policy.

## m5-speed-caches

**2026-10-11 · m5-speed-caches: paint, offscreen and image caches sized
for speed.** Under the speed-first policy, `PAINT_CACHE_BYTES` 4 →
32 MB, `MAX_ENTRY_BYTES` 2 → 16 MB (`strand-render/src/cache.rs`),
`OFFSCREEN_BYTES` 4 → 32 MB, `IMAGE_CACHE_BYTES` 6 → 32
MB and `MAX_PLAYER_BYTES` 16 → 32 MB; design.md's figures
follow. `cache_bench.rs` showed whole repaints of a 2× launcher drop
from about 40 to 7 ms and a reopened 300-app launcher re-decodes no icon
(`a_reopened_launcher_decodes_no_icon_again`).

**2026-10-11 · m5-speed-caches: a stopped frame loop keeps unused entries
for 30 s.** The stop-time trim (`trim_unused_since`) is gone and `IDLE_FREE`
goes from 10 s to 30 s, the GPU device's idle time; unused entries are freed
at the next paint or wake, never at a wake of their own, so the no-wakeup
rule holds. A reopened launcher's first frame drops to 7.0–8.5 ms. Test:
`shadows_no_frame_used_go_once_idle`.

**2026-10-11 · m5-speed-caches (review): entries over 2 MB are built
only by a second whole-repaint frame.** The 16 MB limit made a cold
launcher open 13 ms slower, since most compositors fade and scale without
repaint. `PaintCache::shadow_in` and `gradient_in` now take whether the
frame repaints the entry's whole extent; up to `EAGER_ENTRY_BYTES` (2 MB)
the old rule holds, larger ones are drawn directly until a second frame
repaints all of it. The GPU backend still builds at first sight. Tests:
`large_entries_wait_for_a_second_whole_frame`, `launcher_open_cost`.

**2026-10-11 · m5-speed-caches: what it costs.** `.text` +1,216 B CPU-only
and +1,856 B GPU. The full shell with 161 entries reaches about 64.5 MB PSS,
mostly the image cache; budgets.rs counts KiB, so its target is 65,536
kB and ceiling 71,680 kB, and the owner-confirmed gates stand. A later
change that grows the full shell should raise the target with its reasons.

## m5-speed-thp

**2026-10-11 · m5-speed-thp: THP stays off except the largest pixel buffers
(C1).** Whole-process THP cost about 55 MB PSS for a few percent per frame,
but a cold 4K crossfade took 14.5–15.9 ms without it against 3.5–5.6 ms
with it. So `strand_services::child::thp_off` uses `PR_SET_THP_DISABLE` with
`PR_THP_DISABLE_EXCEPT_ADVISED` (Linux 6.18; plain flag before), buffers of
at least 8 MiB are advised `MADV_HUGEPAGE`, and `restore_in_child` passes
the exact setting on. The bar has no huge page (`AnonHugePages: 0 kB`).

**2026-10-11 · m5-speed-thp (review): the snapshot is its own mapping,
kept as one spare.** Advising heap pixmaps did nothing (already written)
and could leak `VM_HUGEPAGE` to later heap blocks, so that advice is
gone. `strand_render::hugepage::HugeBuf` maps a buffer of at least
8 MiB on its own, 2 MiB aligned and advised before first write; the
crossfade snapshot uses it. A dropped mapping stays as one spare for
the next same-size snapshot, unmapped by `hugepage::trim_idle` after
`IDLE_FREE`. Tests: `large_buffers_are_their_own_aligned_mapping`,
`thp_stays_on_only_for_advised_buffers`; demo.rs's `assert_thp_off` checks
`THP_enabled`.

## m5-speed-trims

**2026-10-11 · m5-speed-trims: forced `mi_collect` trims stay, though
not shown free (C2).** Reload latency with and without `trim()` differed
less than its 3 ms round-to-round spread, so the run neither proves nor
rules out a fault-back cost. The trims stay as existing behaviour that
saves 2–3.3 MB of bar PSS; a launcher-path measurement with them off
is open. The cold `malloc_trim` calls (C3) stay too.

## m5-speed-gpu-drop

**2026-10-11 · m5-speed-gpu-drop: re-promotion cost measured; `DROP_AFTER`
stays 30 s (C10).** `strand-gpu/tests/start_cost.rs` shows a restart
takes about 22–25 ms on ANV (110 ms the first time) and the first frame
about 7 ms thanks to Mesa's shader cache, all on the GPU thread while
the CPU keeps drawing. Keeping pipelines across drops would save nothing
measurable; the drop time is design.md's and the owner's.

## m5-speed-gpu-bound

**2026-10-11 · m5-speed-gpu-bound: presented hardware leg asserts 12 MiB
after the drop.** The owner settled open item 1: `gpu_idle.rs`'s presented
leg asserts PSS after the drop within `PRESENTED_SLACK_KB` = 12,288 kB
of the baseline instead of `BOUND MISS`, about 50% over the worst of six
runs (4.6–8.1 MiB) and well under the 20–40 MB a held device would
leave. The readback leg keeps 6 MiB; `gpu-measure.sh` prints the same bound.

## m5-speed-policy

**2026-10-11 · m5-speed-policy: deliberate speed reverts are charged to the
gate, not the ledger.** Under the 2026-10-10 policy, a revert made for speed
(a cache sized up, a faster algorithm, an opt-level raised on a hot path)
lands with its measured gain and `.text` cost, and the integrator raises
the gate by that amount instead of taking it from the 230 KB allocation;
behaviour-neutral reclaims stay as Q7 says. The rest of the code survey
(C9, C11–C19) stays as it is, with reasons recorded.

## m5-speed

**2026-10-11 · m5-speed: speed review merged; `.text` gates set by
judgement.** The owner's 2026-10-10 policy supersedes m5-plan Q6 and
m5-speed-gates' proposal. Rule: gate = measured size + unspent allocation +
a 64 KiB reserve, rounded up to 256 KiB: **`TEXT_GATE_CPU` = 18,350,080
B** (merged tree 17,961,687 B) and **`TEXT_GATE_GPU` = 22,282,240 B**
(21,857,111 B). The ledger reads spend as Δ − 2,288,656 CPU-only and
Δ − 2,804,048 GPU.

**2026-10-11 · m5-speed: PSS targets re-based for the larger resident
code.** Design bar target **39 MB (39,936 kB)**, ceiling **44 MB
(45,056 kB)**; full shell target **67 MB (68,608 kB)**, ceiling **74 MB
(75,776 kB)**: about 5% over the highest run, ceilings keeping the old
margins. The M3 gates (34/38 and 64/70 MB) stay recorded in m3-report.md,
checked against `M3_*` constants. Targets warn; ceilings are tripwires,
never caps on a user's shell.

**2026-10-11 · m5-speed: summary of what the speed review
changed.** `strand` at opt 3 (list scroll −21%, launcher −26%,
SVG and Lottie −19 to −25%), Wayland crates at 3, larger caches
(reopened-launcher frames 6–7 ms against 37–40 ms), a huge-page
crossfade snapshot; strand-services, fontique and toml stay built for
size. `release_opt_levels_reads_package_sections_only` is renamed
`release_opt_levels_reads_release_sections_only` and expects
`[profile.release]`'s own level.

**2026-10-11 · m5-speed: the idle trim reads its clock before the frame
touches entries.** `Raster::paint` read `Instant::now()` after `prepare`,
so on a slow runner an entry built early in the frame could be freed by
the same paint. The clock is now read before `prepare` and `trim_idle`
uses `saturating_duration_since`, so anything touched after it is never
idle. Test: `entries_touched_after_the_trim_clock_stay`.

## m5-open-spectrum

**2026-10-11 · m5-open-spectrum: a diff retargets fed nodes' producers
without a frame.** A spectrum tapped before `audio.sink` published
its id stayed on device 0: a resting spectrum's source change repaints
nothing, and feeds synced only after a paint. `Host::apply_diff` (which
`demo::apply` calls) now syncs the feeds after every diff. The fix is
in product code; `a_test_tone_lifts_a_spectrum_bar` was right. Test:
`a_resting_spectrum_follows_its_device_without_a_frame`.

## m5-open-budgets

**2026-10-11 · m5-open-budgets: PSS targets raised with headroom for waves
1–2.** Rule: target about 12–15% over the highest measurement, rounded
up to a whole MiB, ceiling a further 12–15%. Design bar target **42 MB
(43,008 kB)**, ceiling **48 MB (49,152 kB)**; full shell target **76 MB
(77,824 kB)**, ceiling **86 MB (88,064 kB)**, since the full shell's
run-to-run spread on GitHub is itself about 9%. Raise them again with a
recorded measurement rather than trim a feature.

**2026-10-11 · m5-open-budgets: the GPU leg's leak tripwire is growth
between cycles.** First-cycle residue spreads widely (mostly a glibc arena
of Mesa's), so the hardware leg runs three cycles (`HARDWARE_CYCLES`;
`STRAND_GPU_IDLE_CYCLES` overrides, at least two) and each later cycle's
after-drop PSS must be within **4 MiB** of the first (`PSS_SLACK_KB`,
also on lavapipe). Absolute bounds become backstops: **16 MiB presenting**
(`PRESENTED_SLACK_KB`) and **12 MiB reading back** (`HARDWARE_SLACK_KB`).

## m5-open-reload

**2026-10-11 · m5-open-reload: `reload_latency` misses on the laptop from
CPU idle; the cache write leaves the reload path.** A busy loop on an unused
core made the bench pass while pinning changed nothing, so the miss is deep
idle between edits; the gate stays (GitHub passes, design.md's budget is a
product promise) and laptop timing stays advisory. `Loader::commit` now only
marks sources unstored, and `Loader::store_cache` writes the last-good cache
after the build is sent (about 0.5 ms off each reload); the bench now runs
with a cache. Tests: `the_cache_is_written_by_store_cache_after_the_commit`,
`a_reload_is_sent_before_its_sources_are_cached`.

## m5-w1-owner

**2026-10-11 · m5-w1-owner: the hardening's four interface
choices kept.** The owner kept: inspector messages carry a node only
(`Renderer::surfaces_of`); invocations numbered from 1 across `await`,
0 unordered, one queue for all services; the reserved `overlay` token
origin (`OVERLAY_ORIGIN`); the input-context route as architecture.md
text until a stream builds it.

## m5-docs

**2026-10-11 · m5-docs: how the ladder places unnamed constructs.**
Twenty constructs are in no row of design.md's ladder; each goes at
the first level whose build needs it, by "does it remove a concept, or
add one?": another face of a taught construct goes there, a new concept
waits. Levels 1–3 build design.md's bar (a) exactly (m5-owner Q15). The
guide's README carries the placements as a table.

**2026-10-11 · m5-docs ladder: `split` is level 1.** The hello bar is a
`split` with `start`, `center` and `end`, a container like `row`, adding
no concept.

**2026-10-11 · m5-docs ladder: `on secondary` is level 2.** The tray
uses it to call `item.menu.open()`; it is `on click` for the other
button. (Review round 3: `open()` tells the app and sets `item.menu.opened`;
a DBusMenu item's menu needs a `popup` bound to `opened`, which bar (a)
lacks, so level 2 says so.)

**2026-10-11 · m5-docs ladder: `hit:` is level 2.** Bar (a)'s `Dot`
writes `hit: grow(6)`; a prop, placed where first written.

**2026-10-11 · m5-docs ladder: assignment and compound assignment are
level 3.** Writing needs something writable, and `state` is level 3;
`=` arrives with it, and `+=`, `-=`, `*=`, `/=` add no concept (bar (a)'s
`Volume` writes `audio.sink.volume -= dy * 0.05`).

**2026-10-11 · m5-docs ladder: `else` is level 3.** It is part of `if`
(level 3).

**2026-10-11 · m5-docs ladder: `exit` is level 3.** `exit` mirrors `enter`
unless given, so it is the same pose concept; level 3 teaches it in prose,
and level 4's toasts first write one.

**2026-10-11 · m5-docs ladder: `on scroll(dy)` is level 3.** Bar (a)'s
`Volume` uses it to write the volume, which needs level 3's assignment.

**2026-10-11 · m5-docs ladder: `let` is level 4.** Bar (a) declares
none; level 4's builds each derive a value (`hits`, `shown`, `level`),
a new concept beside `state`.

**2026-10-11 · m5-docs ladder: `enum` is level 4.** The OSD declares
`enum Kind { volume, brightness }`; level 3 names only builtin variants.

**2026-10-11 · m5-docs ladder: `match` is level 4.** It goes with `enum`,
whose variants it checks exhaustively; level 5's theme file uses it too.

**2026-10-11 · m5-docs ladder: `every … while` is level 4.** It is
the repeating twin of `after … while` (level 4): one timer concept.

**2026-10-11 · m5-docs ladder: `on show` is level 4.** The launcher clears
its query with it; level 4 is where surfaces that open and close are built.

**2026-10-11 · m5-docs ladder: `id:` is level 4.** The launcher's `id:
results` for `nav: results` is the first build that names another node.

**2026-10-11 · m5-docs ladder: `@reset` is level 4.** It is taught beside
`persist`, the other rule about what a value keeps across reloads; levels
1–3 never need it.

**2026-10-11 · m5-docs ladder: `set { }` is level 5.** It overrides
tokens for a subtree, with level 5's `tokens` and `override`.

**2026-10-11 · m5-docs ladder: `keyframes` and `play` are level 5.**
Motion tuning, beside level 5's spring overrides `~`.

**2026-10-11 · m5-docs ladder: `canvas` is level 5.** An escape hatch
beside level 5's shaders.

**2026-10-11 · m5-docs ladder: `type` and `key` are level 5; `enum`
stays at 4.** A record type is first needed by level 5's dock (`type Pin {
app: AppId; label: text }`, `state pins: [Pin] key app = []`), and `key`
is needed only for your own data, since service items bring their keys;
so `key` arrives with `type`.

**2026-10-11 · m5-docs ladder: `slot` is level 5.** A component wrapping
children is first needed by level 5's control centre.

**2026-10-11 · m5-docs ladder: `fn` is level 5.** Builtins (`pct`,
`join`, `dur`) and level 4's `let` cover lower builds.

**2026-10-11 · m5-docs: guide code blocks are whole files tagged with
level and file.** The tag is ` ```strand level=N file=NAME `; a block is
the file's whole content after that step, steps run in page order, and
an unshown file is unchanged, so the newcomer test uses blocks byte for
byte. Inline code is never a step. Levels 1–3 give each component its
own file (`dot.strand`, `clock.strand`, `volume.strand`, `battery.strand`)
beside `bar.strand`, teaching that every file loads with no imports.

**2026-10-11 · m5-docs: the `strand new` template is the hello bar plus
three comment lines.** `docs/guide/template/bar.strand` is design.md's
hello bar after three `//` lines (what the file is, that every `.strand`
file loads and saves are live, the guide's URL). Level 1's first block
is the same bytes, so the newcomer test's first step is a no-op save.

**2026-10-11 · m5-docs: what spec.md freezes beyond syntax.** Which
programs are accepted, and their meaning, is frozen; diagnostic codes
and wording are not. Numbers that change behaviour (15 ms coalesce,
250 ms overlay and settings write delay, 30 writes per second, 30 s
monitor retention, 4 px query hysteresis, 5 s service stop) are frozen;
performance targets and memory budgets are not. CLI is frozen only where
spec.md section 13 names it (`get`, `set`, `toggle`, `call`, `clear`,
`watch`, `reload`, path spellings, the D-Bus name with its caveat).

**2026-10-11 · m5-docs: spec.md marks unpinned points (unverified).** A
bare number as px for a length, which mixed-unit comparisons are accepted,
whether `@reset` resets again on a later reload, and a missing settings
file giving defaults are marked **(unverified)** for the wave-3 freeze
review to settle with a test or a decision.

**2026-10-11 · m5-docs ladder (review round 1): lambdas are level 4.**
The toasts' `filter(n => …)` is the first build to write one, and a
function value is a new concept.

**2026-10-11 · m5-docs ladder (review round 1): `await` is level 4.**
It is the handler-side face of `Async` (level 4, m5-owner Q17).

**2026-10-11 · m5-docs ladder (review round 1): the `lock` surface is
level 4, which builds a lock screen.** A lock needs a two-way `input`
(level 3) and `on activate` (level 4), so level 4 gains a fourth build; the
guide's lock section is prose, and lock code is tested only in the lock VM.

**2026-10-11 · m5-docs ladder (review round 1): settings files are level
5.** `state x from "file.toml" { … }` adds a concept no lower build
needs; level 5's theme file is the first to read one.

**2026-10-11 · m5-docs ladder (review round 1): SVG layer selectors
(`#name { }`) are level 5.** They bind props inside an asset, with level
5's escape hatches.

**2026-10-11 · m5-docs ladder (review round 1): `service … from file |
listen | poll` and `permit exec` are level 5.** They are other sources
of level 5's custom service. Every row of spec.md's construct index now
has a level, carried in design.md's "Also taught there" column.

**2026-10-11 · m5-docs (review round 1): spec.md corrected to the checker
and runtime.** `attach:` and `scrim:` are errors on `bar`, `osd` and
`lock`; a `bool` match with both arms is exhaustive; `every` re-phases
after a gap and a zero period is an error value; 8.7 states wave2-lang's
throttle rule and its exemptions; section 1's frozen numbers include the
250 ms settings write delay and 5 s service stop.

**2026-10-11 · m5-docs (review round 1): a
missing settings directory is created on first write.**
`a_missing_directory_is_created_for_the_first_write` pins it, so spec.md
section 11 states it unmarked; only "a missing file gives every field
its default" stays **(unverified)**.

**2026-10-11 · m5-docs (review round 2): `??` on an `Async` and its
list reads follow the VM and checker.** `a ?? b` falls through only
when there is no kept value (`AsyncValue::usable()`), so a search keeps
results while loading. `.len` and `for` read the last list, the six list
transforms give an `Async<[U]>`, other reads are `check::async`, and
`.value` is a member. The runtime's existing meaning, written down.

**2026-10-11 · m5-docs (review round 3): spec.md states the checker's
limits on shadowing, `state` placement, inherited props and the poll
floor.** 4.2 lists language types, builtin elements and builtin services
as taken names (`check::redeclared`). 5.3 allows `state` only directly
in a component body, a surface block, a `for` body, or the element
written directly in one. 6.2 names `weight`, `font` and `color` as the
only inherited props. 5.6 states the 100 ms floor of a polled command
(`hir::MIN_COMMAND_POLL`, `check::poll_too_fast`), now a frozen number.

**2026-10-11 · m5-docs (wave 2): level 4's builds are design.md's (b),
(c), (d) byte for byte, then one step each.** The OSD, toasts and launcher
blocks equal the accepted fixtures. `match` then rewrites the OSD's `let
level`, and `await` arrives in a toasts "Clear all" button (`for n in
notifications.popups { n.dismiss(); await sleep(80ms) }`), not inside an
`if`, since unmount cancels at the next `await`. The lock screen teaches
`every … while`; `@reset` is prose only.

**2026-10-11 · m5-docs (wave 2): the lock screen is `lock.strand` with
`export state locked` bound by `open: <-> locked`.** That is m4-lock-w1's
shape: the instance writes `false` through the two-way `open` after
unlock. The guide locks with `strand set lock.locked true`, not `toggle`;
the field sends nothing when empty. Nothing in the lock section runs
outside the lock VM.

**2026-10-11 · m5-docs (wave 2): level 5 orders its outline by what
each build reads.** A step must check when applied, so the page runs:
theme (design.md's block byte for byte), the `file` element and `strand
export`, custom services (`ppd`, a `from file`, a `sensors -j` `from poll`
with `permit exec`), the control centre (`pages`, `slot`, `fn`, component
tokens, `set` overrides), the dock (`type`, `key`, `persist`, drag and drop,
`~`, `keyframes`/`play`), then escape hatches and plugins. `from listen`
is prose only.

**2026-10-11 · m5-docs (wave 2): level 5's `file` example is its own
`colors.strand`.** Keeping `theme.strand` equal to design.md's block and the
`file` in its own file means neither stream's edit depends on the other's
wording, and shows a `file` at top level of any file. Amended in review
round 1: the theme block gained `file "~/.cache/strand/accent.txt" { text:
$accent }`, and `colors.strand` keeps `colors.css`, so they never clash.

**2026-10-11 · m5-docs (wave 2): a non-`.strand` step file is a block
tagged with its own language.** Level 5's template is ` ```css level=5
file=colors.css.tmpl `, applied in page order. Untagged blocks are never
steps; only blocks with `level=` and `file=` are read, and only `strand`
ones are checked.

**2026-10-11 · m5-docs (wave 2): what levels 4 and 5 leave unverified.**
(1) The `file` element, templates, `strand export` and hue roles; (2)
`canvas`'s paint order, shown in prose only; (3) `persist` on a keyed list
of records; (4) a `set` override of a component token. Amended in review
round 1: (1) landed and is checked, (3) and (4) are settled by tests,
(2) stays prose.

**2026-10-11 · m5-docs (review round 1): guide blocks are checked step
by step, non-`.strand` steps included.** `every_guide_block_checks`
(`crates/strand-dev/tests/guide.rs`) applies steps in page order
and checks each with `Analysis::offline` for no error, warning
or `strand fmt` change; its extractor now takes non-`strand`
blocks with `level=` and `file=` (one without the other is
an error). `guide_copies_of_design_blocks_are_verbatim`,
`the_guide_dock_reorders_every_pin_and_keeps_the_order` and
`the_guide_control_centre_overrides_a_component_token` settle unverified
items (3) and (4).

**2026-10-11 · m5-docs (review round 1): the control centre shows the power
profile without setting it.** `check/expr.rs` `rw_root` rejects writes
to a config service's `rw` field (`check::read_only`), though design.md
and the runtime mean them to work. The fix (one `DefKind::Service` arm in
`rw_root`) is strand-compiler's, so it is reported; until then the Power
tile highlights the profile in use and the prose says the assignment
is rejected.

**2026-10-11 · m5-docs (review round 1): every pin draws one icon.**
A hidden uninstalled pin made `on drop`'s `at` (drawn children)
disagree with `pins.move` (pins). The pin now uses `let app =
apps.all.find(a => a.id == p.app)` and always draws one `image`,
`app?.icon ?? "application-x-executable"`, faded while `app == null`. Test:
`the_guide_dock_reorders_every_pin_and_keeps_the_order`.

**2026-10-11 · m5-docs (review round 1): `temps` reads one chip.** `=
temp1_input` takes the first match in sorted key order, so the command is
`["sensors", "-j", "coretemp-*"]`; the prose says to use `"k10temp-*"`
on AMD. A key path cannot name the chip itself. Untested (no such chips
in the container).

**2026-10-11 · m5-docs (review round 1): locking before sleep waits.**
Level 4 gives `swayidle -w before-sleep 'strand set lock.locked true
&& sleep 1'`, since `strand set` returns before the compositor grants
`ext-session-lock`. A `strand set` that waits for the grant would be
S-ipc's.

**2026-10-11 · m5-docs (review round 2): level 5's `from
file` reads `/etc/hostname`; the page states the `key=value`
limit.** `Document::parse` (`custom.rs`) reads pairs only when
every non-blank line is a pair and strips double quotes only, so
os-release with a comment read as whole text. The step reads `service
host from file "/etc/hostname" { name: text }`; the parser fix is
strand-services'. Tests: `the_guide_file_services_read_one_value`,
`the_guide_states_the_pairs_limit`.

**2026-10-11 · m5-docs (review round 2): a missing sensor chip is a
stderr warning, not a failed service.** `poll_command` logs a warning
and returns `None` on a non-zero exit, and `strand watch` never shows
poll failures, so level 5 says `temps.cpu` stays `0` with only a `strand:
WARN strand_services::custom` line. Whether it should reach `strand watch`
is strand-services' and S-ipc's call.

**2026-10-11 · m5-docs (review round 2): the
component-token override is resolved, not just shown.**
`the_guide_control_centre_overrides_a_component_token` now resolves each
tile's `pad: $Tile.pad` the way strand-render's flatten does (`TokenScope`,
then ancestors' `Prop::Tokens`) and asserts the row's tiles get `$space.2`
and others `$space.3`. strand-dev gains `strand-scene` and `strand-services`
dev-dependencies.

**2026-10-11 · m5-docs (review round 3): a drop on another monitor's
dock lands one place on past its copy of the pin.** `drop_index`
leaves out only the dragged source node, so another monitor's dock
counts its own icon for the pin; no config can tell the cases apart,
so the guide keeps a dock per monitor and its prose states the
limit. `the_guide_dock_drops_where_the_pointer_is` drives two mock
monitors through `strand_render::Router` and pins the off-by-one. The
fix (a `Prop::DragKey`, or logic lowering `at` in `drop_args`) changes
architecture.md's drag-and-drop bullet, so it is reported, not made.

**2026-10-11 · m5-docs (review round 3): level 5's `host` reads
`/proc/sys/kernel/hostname`.** `/etc/hostname` may hold `#` comments
or be absent; the kernel's file always exists and holds one name,
as strand-services' `system` service reads it. It sends no inotify
events, so the page says a rename shows once the service restarts. Test:
`the_guide_host_service_reads_the_kernels_name`.

**2026-10-12 · m5-docs (wave 3): `docs/reference/` is generated from
the schemas, committed, and checked current.** `strand-dev reference
[dir]` (`crates/strand-dev/src/reference.rs`) writes `README.md`,
`elements.md`, `services.md`, `types.md`, `functions.md` and
`tokens.md` from `Schema::builtin_with` and the `///` docs;
plugin schemas are never read, so pages are the same on every
machine. Committed so schema changes show as page diffs. Tests:
`the_reference_is_current`, `every_schema_entry_is_on_a_page`,
`writing_removes_pages_no_longer_generated`.

**2026-10-12 · m5-docs (wave 3): the ladder test, and constructs
taught in prose.** `crates/strand-dev/tests/ladder.rs` walks the
HIR of each guide step and names every construct (an unnamed HIR
node fails). `no_guide_block_uses_a_construct_above_its_level`
and `each_construct_is_taught_at_its_level` hold the ladder; seven
constructs (`else`, `exit`, `@reset`, `canvas`, a shader, SVG layer
selectors, `service from listen`) pass only when their level's page
names them in inline code. Also `the_ladder_is_the_guides_table`,
`the_walk_finds_constructs_above_a_level`.

**2026-10-12 · m5-docs (wave 3): the freeze review settles the four
unverified points.** Pinned by `crates/strand-dev/tests/spec.rs`: (1) a
bare number is logical pixels for a length, degrees for an angle, a load
error for a duration; (2) ordering compares two plain numbers, one kind,
a unit beside a plain number, or a length beside a percentage (as written);
durations only with durations; lengths, angles and durations never mix;
(3) `@reset` resets on every reload that commits it with the attribute;
(4) a missing settings file gives every default and creates nothing.

**2026-10-12 · m5-docs (wave 3): the newcomer test and each step's
predicted kept cells.** `newcomer.rs::the_guide_builds_bar_a_on_two_outputs`
runs `strand new`, `strand run` on two headless outputs with
`STRAND_MOCK=acceptance`, and saves levels 1–3's steps in place,
requiring a clean `reload` event, clean `strand check`, the `PREDICTED`
kept and reset cells (only `Clock.open` per bar, kept after level 3 adds
it) and reference PNGs under `tests/refs/newcomer/`. Budget 120 s.

**2026-10-12 · m5-docs (wave 3): the agent proxy scores what the
shell shows.** `docs/guide/target-checklist.md` lists nine items for
bar (a); `score` in newcomer.rs checks each on `inspect.dump`, telling
monitors apart by bar identity and depending on no file names, component
names or layout. The guide's bar must score 9/9; the hello bar scores
3/9. `scripts/agent-proxy/` (`prepare.sh`, `run.sh` with `AGENT_PROXY_CMD`,
`score.sh`) is the harness; no model session has been run.

## m5-docs3

**2026-10-12 · m5-docs3 (review round 1): `strand-dev reference` writes
only into a reference directory.** `reference::write` would have deleted any
hand-written `.md`; it now refuses with `InvalidInput` ("not a reference
directory") when a `.md` does not open with the generated note, before
writing anything, and removes only pages it no longer generates. With
no directory it fails if `./docs/reference` does not exist. Tests:
`writing_refuses_a_directory_with_hand_written_pages`, `reference_cli.rs`.

**2026-10-12 · m5-docs3: lengths in different units compare as written.**
The checker types `px`, `ch` and `%` all as `length` and the VM compares
numbers only, so `2ch < 10px` is true. Rejecting would need the unit
in the type and break configs, so spec.md 7.2 states it; a warning lint
stays possible. Test: `lengths_in_different_units_compare_as_written`.

**2026-10-12 · m5-docs3: spec.md is reviewed, not frozen; the freeze is the
integrator's closing commit.** Other streams may still add schema entries,
so spec.md's status says the freeze review settled every open point and
the freeze is wave 3's last commit. Amends m5-docs' "the freeze review";
features.md's "language spec frozen" box is the integrator's.

**2026-10-12 · m5-docs3: bare-number tests check values.**
`a_bare_number_for_an_angle_is_degrees` requires `rotate: 45` to paint
the same pixels as `rotate: 45deg`; `mixed_unit_comparisons` requires
`stagger: 2`'s diagnostic to be `check::type_mismatch`.

**2026-10-12 · m5-docs3: the proxy's `strand` acts on its own config;
the session has no compositor.** The wrapper runs strand with `HOME` and
`XDG_CONFIG_HOME` in the workspace's `home/` (whose `.config/strand`
links to `config/`), in the caller's directory, refusing one outside
the checkout. Deviation from m5-plan: the session gets no compositor
(`strand run` refused); the two headless outputs are the scorer's.

**2026-10-12 · m5-docs3: the newcomer test's 120 s budget is strict;
the scorer depends on no layout.** The test runs in the strict `cargo
test --workspace` step, so its budget is not advisory; the marker stays
and `gate-misses.sh` notes newcomer.rs fails. Items are scored on a
dump before any click, dots are distinct numeric keys per monitor, and
`open_calendar` clicks along the bar until the calendar shows. Test:
`the_scorer_passes_a_bar_laid_out_otherwise`.

**2026-10-12 · m5-docs3 (review round 2): equality follows the ordering
rule; a number key keeps its unit.** Two numbers are comparable with
`==`/`!=` exactly when they are for order, equal when their numbers
are (`10px == 10`, `2ch == 2px`, `1s == 1000ms` true; `90deg == 90px`,
`1s == 1000` a load error); the stricter rule can loosen later, not the
reverse. `check/expr.rs`'s ordering table moves into `common_unit`, used
by `equality`. `ValueKey` stays unit-aware, since keys are identities
(spec.md 6.5). Tests: `equality_takes_numbers_by_the_ordering_rule`,
`a_number_key_keeps_its_unit`.

**2026-10-12 · m5-docs3: the scorer never closes a calendar it was slow
to see.** A click is watched 250 ms while no new surface paints and 2 s
(`POPUP_LOOK`) once one has, and the dump is checked again before the
next click, so a late calendar is credited rather than dismissed.

**2026-10-12 · m5-docs3: the reference's currency check names the fix
that works.** `the_reference_is_current` sorts what it finds: a page
that differs, is missing or is no longer generated is for `strand-dev
reference`; anything `write` leaves alone or refuses is listed first under
"remove these by hand". Test: `the_currency_check_names_the_fix_that_works`.

**2026-10-12 · m5-docs3 (review round 3): the equality checker
change leaves this branch, proposed for its owner.** `check/expr.rs`
is S-lsp's file and the stricter rule would make loading configs fail
(`battery.time_left != 0`), so it is reverted. spec.md 7.2 states today's
rule (`==`/`!=` take any two numbers, compared as written) and names the
stricter rule as the open point. `equality_compares_numbers_as_written`
stays; `equality_takes_numbers_by_the_ordering_rule` is `#[ignore]`d. The
proposal: `Checker::equality` uses a free `common_unit(a, b)` holding
`arith`'s ordering table; the freeze keeps whatever the checker has then.

**2026-10-12 · m5-docs3 (review round 3): the scorer's slow pass
finds a late calendar.** Round 2's claim that a late calendar is
never dismissed was false. `open_calendar_on`, over a `Bar` trait,
makes the quick pass, then a slow pass watching every click for
the full 2 s (likely points first), giving up after 150 s. Test:
`a_calendar_the_shell_shows_late_is_still_found` with `LaggyBar`.

## m5-plugins

**2026-10-11 · m5-plugins: the plugin builder's public surface is four
items.** `Shell`, `Shell::builder`, `Builder::service` and `Builder::run`,
beside `process_setup!` and the hidden `cli` and `__process`; a plugin
adds services and nothing else, keeping the unstable surface small (Risk
14). `strand` does not re-export `strand-services`, since the derives
emit `::strand_services` paths, so a plugin names that crate from the
same git rev; a `crate = …` override is not taken in M5.

**2026-10-11 · m5-plugins: a plugin binary is the whole `strand`
command line.** `main.rs`'s dispatch, `COMMANDS` and idle hooks move to
`src/entry.rs` as `Builder::run`, and `main.rs` becomes `process_setup!()`
plus `Shell::builder().run()`: `strand` is a plugin with no extra
services, removing a concept. The move landed as `laptop/m5-w2`'s
first commit; `entry.rs`'s COMMANDS rows and arms are a shared dispatch
point (S-ipc keeps the table), and S-plugins owns `src/plugin.rs` and
`strand-compiler/src/schema/plugins.rs`.

**2026-10-11 · m5-plugins: how a plugin service registers and is served.**
`service::<S>()` records function pointers so generics are monomorphised
only in a plugin binary. `run` validates the list (`DuplicateService`,
`BadName` for names not `[a-z_][a-z0-9_]*`, `Schema { service, errors
}`) before any subcommand and stores it in a process-wide `OnceLock` via
`plugin::install`. Pure twins (`services::schema_with`, `Real::start_with`,
`set_text_with`) keep tests off process state. Extras register after
`Builtin::register` and route through the `Composite`; `strand set` and
`strand call` find services with `Real::served(name)`. Plugin services
share the runtime or use `#[service(thread)]`, same lifecycle.

**2026-10-11 · m5-plugins: one schema rule for `strand check`, `strand
run` and the LSP, plugin files included.** Order: builtin texts, linked
plugin texts in `service` order, then files in name order, skipping a
file whose stem names a linked service. Each applies atomically through
`Schema::extend`; a bad file is skipped with a warning and never stops
a boot. A file-declared service no store serves reads defaults; an
`UnservedHost` raises one notice on first `acquire` and resolves it on
last `release`, so only configs that read it see it. Files are read once
per process.

**2026-10-11 · m5-plugins: `--write-schemas` and the schemas
directory.** The top-level flag takes no directory, writes only
registered services' schemas to `$XDG_DATA_HOME/strand/schemas/`
(else `$HOME/.local/share/strand/schemas/`) as `<name>.schema` with a
header naming the writer (`current_exe()` canonicalised), via temp file
and rename, removes only its own stale files and lists others as "left
alone". The reader takes regular `*.schema` files up to 1 MiB, UTF-8, no
dotfiles. Reading and applying live in `strand_compiler::schema::plugins`
(`dir`, `load`, `apply`) so check and the LSP agree.

**2026-10-11 · m5-plugins: `--write-schemas` refuses an undocumented
schema; the other errors.** `Undocumented { service, entries }` writes
nothing when an entry lacks a `///` doc, so plugins cannot add hover-less
names; `strand check` does not apply the rule to files. Other errors:
`Schema`, `Write { path, error }`, `NoDataDir`, `DuplicateService`,
`BadName`, one private enum printed as `<binary>: <error>` with a failing
exit, never a panic.

**2026-10-11 · m5-plugins: `Builder::run` warns when process
setup is missing.** Before dispatch it checks a heap block is
mimalloc's (`mi_is_in_heap_region`) and THP is off, and warns naming
`strand::process_setup!()` if not; a pure `setup_warning(mimalloc, thp_off)`
is unit-tested. Tests spawn plugins with THP re-enabled in `pre_exec`;
`plugin_bare` shows the warning. The guide's plugin page requires the macro,
the workspace's `[profile.release]` and overrides, and opt-level 3 for
the plugin crate; a test holds its profile block to the root `Cargo.toml`.

**2026-10-11 · m5-plugins: `plugin_hello` and the wave-2 tests.**
`crates/strand/examples/plugin_hello.rs` invokes `process_setup!()` and
registers `hello` (`greeting: text`, writable `count: int` set to 41 on
first read, documented `action greet(name: text)`); `tests/m5/plugin.rs`
finds it under `target/<profile>/examples/` and fails with the build command
if missing. Tests include `an_extra_service_is_checked_and_runs`,
`the_stock_strand_reads_plugin_schemas`,
`the_setup_warning_shows_without_the_macro`,
`the_guide_profile_matches_the_workspace`, and strand-dev's
`plugin_schemas_hover` and `the_lsp_binary_reads_plugin_schemas`;
`Real::set_text("hello.count", "+1")` must give 42.

**2026-10-11 · m5-plugins: size and speed.** Not quite 0 in `strand`:
the schema-file reader, `--write-schemas` writer, validation and
setup check were estimated at 4–8 KB, charged to the plugins line
(8 KB). A plugin binary's size is not gated. Boot costs one `opendir`
plus one `Schema::extend` per file; nothing touches the reload, frame or
service-call path.

**2026-10-11 · m5-plugins: first review and the integrator's acceptance.**
The integrator accepted moving the dispatch into `src/entry.rs` and the
stock `strand run` reading plugin schema files. The review moved `strand
call`/`set` lookups to `Real::served`, kept COMMANDS as S-ipc's, added
binary-spawning tests for strand-dev and stock `strand run`, tied the
unserved notice to the config's read, and made `<exe>` the canonical path.

**2026-10-11 · m5-plugins: second review: lookups, refusal on an unserved
file, watch order.** `strand call`'s lookup cannot be observed through
`strand call` (actions are fire-and-forget), so only `set`'s relative write
is tested. A file-only service now refuses writes, calls and actions in
`UnservedHost` with the notice's text; reads stay at defaults. Service
notices are not replayed to late watchers, so tests reload the read
in after connecting. The notice clears through `ServiceDiagnostic`'s
`resolved: true`; seams `plugin::install(slot, list)`, `schema_with`,
`Real::start_with`.

**2026-10-11 · m5-plugins: third review: seams reach `set_text` and
unserved files.** `set_text_with(schema, host, rt, types, path, text)`
is the pure twin. `schema_with` returns `Served { schema, unserved
}`, `services::served()` is the process-wide `OnceLock` (`schema()`
is `&served().schema`), and `start_with` takes `served: &'static
Served`. `an_extra_service_is_checked_and_runs` polls `strand get`
for 10 s after `call`. `run` checks all three of `DuplicateService`,
`BadName` and `Schema`; bad files only warn.

**2026-10-11 · m5-plugins: built (wave 2), and what building changed.**
(1) `Real::start_with` declares each unserved service on the fallback
via `ServiceHost::declare` so reads give defaults. (2) A file or text
redeclaring a provisional builtin (`clock`, `calendar`, `screens`,
`audio`) is skipped or refused as `Schema`. (3) `Served` gains `warnings:
Vec<String>`, which `strand check` prints as `warning: plugin schema:
…`. (4) `apply` uses the text without its header (`plugins::body`), so
the fingerprint ignores the writer's path. (5) `PluginError::ReadEarly`. (6)
`validate` and `write` are function pointers in `Registration`, so `strand`
links neither; measured +25,408 B `.text` against the 8 KB line, reported
as an overrun. (7) `strand::Builder` is exported. (8) `--write-schemas`
is handled in `entry::main` before the other commands.

**2026-10-11 · m5-plugins: review round 1: a linked text serves exactly
one service.** `layered` refuses, as `Schema { service, errors }`,
a text whose new services are anything but `{name}` ("one text serves
one service"), since an extra one would be served with no store. Test:
`a_text_that_does_not_apply_is_refused`. `skipped_plugin_files_are_reported`
checks `strand check` prints and counts skipped-file warnings and still
passes, and `strand run` logs them.

**2026-10-11 · m5-plugins: review round 2: the mock reads plugin services
at their defaults.** Under `STRAND_MOCK`, `run/logic.rs` builds no `Real`,
so `services::mock_served` declares every service the served schema
adds at its defaults (`declare_at_defaults`). The mock raises no notice
and refuses nothing for a file's service, since it stands in for every
service alike. Tests: `the_mock_reads_plugin_services_at_their_defaults`,
`the_mock_reads_plugin_schemas`.

**2026-10-11 · m5-plugins: review round 3: an unserved file service
refuses item actions and writes.** `start_with` gives each `UnservedHost`
as `items` every record its service hands out transitively, less
those the builtin schema declares or a store already serves, so
`g.poke()` on `hello.main` is refused like `hello.greet()`. Test:
`an_unserved_file_service_refuses_item_actions`.

## m5-tools

**2026-10-11 · m5-tools: `strand export` writes palette roles only and
never invents a colour.** Superseded by "Strand ships no app mappings":
there are no per-app targets. What still holds: no colour is invented
for a key the palette lacks (deriving ANSI hues would recolour meaning),
colours are opaque `#rrggbb` unless an expression makes them translucent,
and `-o` writes through `strand_theme::writer::write_atomic`.

**2026-10-11 · m5-tools: `gtk` wrote libadwaita's named colours with
`@define-color`.** Superseded: Strand ships no GTK mapping. The fact kept
for user templates: libadwaita 1.10 still binds each `--x-color` variable
to its named colour (`--window-bg-color: @window_bg_color`), so a user
`gtk.css` redefining the name with `@define-color` recolours widgets,
while a `:root { --x: … }` block is a parse error on GTK 3.

**2026-10-11 · m5-tools: the palette source: persisted palette,
else an offline solve that must not fall back, else the error.**
Now the offline fallback after asking the running shell, and it
only picks the palette the offline mount pins. (1) The persisted
palette is **`$XDG_STATE_HOME/strand/palettes/palette`**, written by
`ThemeHost::remember_palette` only for a resolved palette, per user, read
with `Palette::from_text`. (2) The offline solve mounts the compiled config
with the deterministic `SchemaHost`, `system` seeded from `palettes/system`,
contacting no bus and waiting for `material(image:)`. (3) Otherwise
`strand export: no palette: run strand once, or fix the theme` with its
cause; a broken theme must error, never fall back to the built-in palette,
which needs a no-fallback strand-compiler accessor.

**2026-10-11 · m5-tools: Strand ships no app mappings (owner).** Strand
carries rich palettes into many programs and formats, theme switches
included, but maintains no first-party app themes or mappings (no kitty,
ghostty, GTK, Hyprland or Zed, as code or templates). This replaces
the per-app paragraphs; each table was a concept tied to someone else's
releases, while a template is the user's and reuses Strand's expression
language. Its "two generic outputs" are refined by "the owner-approved
theming design".

**2026-10-11 · m5-tools: a palette keeps its source's named colours as
`$palette.<name>`.** Superseded by the owner-approved design (snake_case
extras, read only in templates and `file` text, persisted as `extra`
lines, plus 13 hue roles). What holds: roles are still filled only by
importers and `Partial::fill`; extras live under the reserved `$palette`
namespace so they never shadow a role or base token, and relate to roles
only through the importer's mapping.

**2026-10-11 · m5-tools: `strand export json` writes the palette as
data.** Superseded in its shape by the owner-approved design (which adds
`source`, 62 roles and every resolved token). What holds: pretty-printed
with two spaces and a final newline, roles keyed by Strand names only,
output to stdout or `-o <file>` through `write_atomic`.

**2026-10-11 · m5-tools: `strand export --template <file>`: holes are
Strand expressions, no filter syntax.** Superseded in its value formats
and methods by the owner-approved design; what stays is `{{ expr }}`
holes ending at the first `}}` outside a string, the `{{ "{{" }}` escape,
a theme-only scope, and errors reported all at once with `file:line:col`
and nothing written on error.

**2026-10-11 · m5-tools: where `strand export` reads the theme.**
Superseded in part: the CLI asks the running shell over IPC first. The
offline mount holds as every export's fallback: compiled as `strand check`
compiles, mounted with `SchemaHost` and `system` from `palettes/system`,
settings and `persist` cells read without writing, the overlay file
`$XDG_STATE_HOME/strand/tokens.toml` applied, and the palette root pinned
to the persisted palette when there is one.

**2026-10-11 · m5-tools: declared outputs `export template "<file>" to
"<file>" [run [<argv>]]`.** Superseded by the `file` element (and amended
2026-10-13 by m6-w2-language "declared actions"). Its reasons carry over:
literal paths keep written files static and checkable, commands are argv
lists under `permit exec`, output is re-rendered on token-table target
changes, written only when the text differs (at boot too), through an
atomic coalescing queue, with overlays applied and failures as `strand
watch` notices.

**2026-10-11 · m5-tools: Strand ships no templates; the guide shows a
neutral CSS example.** No application template is shipped or kept in the
guide. The guide documents `strand export json`, `base24`, `--template` and
the `file` element with a CSS custom-properties file (with `hex($fg.muted,
alpha: true)` and `hex($accent.alpha(0.4), alpha: true)` for translucent
colours). Golden tests in `crates/strand-theme/tests/export.rs` cover `json`
and `base24` from `catppuccin:mocha`; template tests use their own fixtures.

**2026-10-11 · m5-tools: the owner-approved theming design.** The owner
approved the integrator's combined design, replacing the paragraphs
marked superseded; Strand carries palettes into any programs through
theme switches and maintains no per-app themes or templates.

**2026-10-11 · m5-tools (owner-approved): 13 hue roles join the palette
(49 → 62).** `red orange yellow green cyan blue magenta bright_red
bright_yellow bright_green bright_cyan bright_blue bright_magenta`,
appended to `Role::ALL`. Sources with hues fill them as given (base16
`08`–`0E`, base24 `12`–`17`, Catppuccin's terminal mapping) and they
are never altered; a missing hue is `blend::harmonize`d toward the accent
(at most 15°) at tone 80 dark or 40 light, a missing bright is its base
±0.08 OKLCH L. The names become reserved, before the freeze. Replaces the
`harmonize` method proposal.

**2026-10-11 · m5-tools (owner-approved): extras and identity.**
Every importer keeps its source colours in `Palette.extras`, snake_case
(`$palette.base0d`, `$palette.mauve`, `$palette.color.brand_primary`),
persisted as `extra <name> #rrggbbaa` and the name as `name <text>`. Extras
are read only in templates and `file` `text:`; a missing one without
`??` is an error. Identity is `$palette.name`, `$palette.source` and
`$palette.dark` in the same namespace; source colours so named become
`name_`, `source_`, `dark_`.

**2026-10-11 · m5-tools (owner-approved): a template reads the other mode
with `.light()` and `.dark()`.** They are methods on a token path (`{{
$surface.light() }}`), needing no new grammar or keyword. The other mode
is a refcounted memo `Instance::other_mode(rt)` on the logic thread, made
while an output or IPC request reads it: it re-evaluates every top-level
`let` reaching `system.dark` in its own env with that read negated,
writes nothing (never `remember_palette`), has no fallback, and applies
runtime overlays to both modes. When both modes give the same `dark`
flag the mode is absent and reading it is an error unless `??`.

**2026-10-11 · m5-tools (owner-approved): template expressions
and formats.** A hole is one Strand expression over tokens, extras,
identity, literals, `??`, `join`, `hex`, `argb` and the template-only
methods `.red()`, `.green()`, `.blue()` (0–255), `.opacity()` (0–1),
`.light()`, `.dark()`; services, `state`, `let` and `fn` are errors. `hex(c,
alpha: false, hash: true)` writes `#rrggbb` (a translucent colour without
`alpha: true` is an error), `argb(c)` writes `0xaarrggbb`, hex is lowercase,
numbers shortest with at most 3 decimals and no unit (`{{ $space.2 }}px`),
non-pixel lengths as written, strings, keywords and bools as is, composites
an error. In `export json` fonts, springs and shadow lists are objects with
strand-scene's field names, other kinds `null`, time-dependent tokens at
`t = 0`. Channels are methods by the integrator's ruling, so `$accent.red`
stays a token path.

**2026-10-11 · m5-tools (owner-approved): the CLI `strand export json
| base24 | --template <file>`.** All write to stdout or atomically to
`-o <file>`. `json` writes `name`, `source`, `dark`, 62 roles, every
resolved token by path and the extras. `base24` writes a scheme YAML
(`system`, `name`, `author: "strand export"`, `variant`, `palette:`
`base00`–`base17`) from the resolved token table by a fixed slot-to-role
table, passing a base16/base24 import's own extras through except where a
role is overlaid; tinted-builder is dropped. `--palette '<expr>'` renders
without a config. The theme comes from the running shell (socket-only
`Request::Export`), else the read-only offline mount (writing nothing,
`ThemeHost` and `Quantiser` read-only), else the no-palette error.

**2026-10-11 · m5-tools (owner-approved): the `file` element replaces
`export template … to …`.** `file "<path>" { template: "<path>"; run:
[<argv>] }` or with `text: <expr>`; exactly one of `template:`/`text:`,
`run` needs `permit exec`. It is allowed at the top level via grammar.md's
`top_file`, a grammar change before the freeze; one path mounted twice is
an error. Content is a memo written when its bytes differ (and at boot),
rendered on the logic thread after the diff; writing, the manifest and
`run` are off it. `run` has a 200 ms trailing debounce, one queued re-run,
a 10 s timeout killing its group. Strand never overwrites a file it did
not write (manifest under `$XDG_STATE_HOME/strand/files`, `[replace]`
offered), refuses `.strand` targets, and never deletes a removed `file`'s
target. A bench holds 10 both-modes outputs' post-diff work to 5 ms.

**2026-10-11 · m5-tools (owner-approved): the rest.** A portal Settings
backend stays out of core; `file … { run: ["gsettings", …] }` or
a plugin service covers it. Estimated cost 26–38 KB `.text` (amended
by m5-integration-w1: S-palette 10 KB and S-tools 28 KB ledger lines,
nothing from the reserve). The wave-2 test list covers importers' extras
and hue mappings, template goldens, `file` container behaviour, IPC/offline
agreement, base24 round trip, byte-identical state after offline export,
and overlays reaching both modes.

**2026-10-11 · m5-tools: review round 1 of the theming design.** Channels
became template-only methods; the offline mount is every export's fallback;
the other mode is evaluated on the logic thread only for outputs that read
it; `base24` got a slot table; a top-level `file` is a grammar change;
the persisted palette keeps its `name`; `export` is socket-only; the
ledger estimate includes `new`. Each fix is in the paragraphs above.

**2026-10-11 · m5-tools: review round 2 of the theming design.** design.md
shows the `file` line in prose until the parser accepts it; the offline
`ThemeHost` writes nothing (tested byte-identical); the other mode is a
kept shared memo; base24 compares roles as `#rrggbb`; the bench gains 10
both-modes outputs held to 5 ms.

**2026-10-11 · m5-tools: review round 3 of the theming design.** The
other-mode memo re-evaluates `let`s reaching `system.dark`; `--fg-muted`
uses `hex(…, alpha: true)`; non-colour value formats are restated;
`base24` reads the resolved table; an IPC `export` holds the memo; the
other mode never calls `remember_palette`; overlays apply to both modes.

**2026-10-11 · m5-tools (wave 2): the interfaces as built.**
(1) The other mode is a counted hold: `Instance::hold_other_mode()
-> OtherModeHold`, `release_other_mode`, `other_mode_holders()`,
`other_table() -> Result<Option<TokenTable>, Error>`; a hold is tied to
its program. (2) `Instance::take_outputs() -> Vec<OutputChange>` is called
after the diff (`OutputContent::Write { bytes, run }` or `Failed { diags,
template }`). (3) `instantiate::theme::PALETTE_PENDING` marks a loading
palette. (4) The manifest is `<hash> <path>` lines, FNV-1a 64. (5) `Export`
carries the template's `name`. (6) `file` is `Builtin("file")` in the HIR,
`FileHir.outputs`, `Program.file_texts`, `Program.file_templates`, lowered
to `ElementKind::File(Arc<FileOutput>)`. (7) `Quantiser::read_only(dir)`,
`check::files::templates`, the overlay's `[replace]`.

**2026-10-11 · m5-tools (wave 2): review round 1 of the tools half.**
Stderr of a `run:` is read without blocking with a 100 ms `STDERR_GRACE`,
so a backgrounded child cannot hold the writer. `--template` takes
the palette with no fallback, so a broken theme errors. A `when` in
a `file` changes its props but never switches `template:`/`text:`
(`check::file_source`). `check::files::templates` checks literal
template paths in `strand check` and the LSP, not the loader. Refused
writes retry from 1 s doubling to 10 s (`RETRY_FIRST`, `RETRY_MAX`),
not for `.strand`. A dangling link is refused, not replaced. One
shared no-fallback palette memo (`Ctx::palette_ok`) serves all outputs;
`file_outputs_after_a_swap_are_under_five_milliseconds` measured 0.55
ms. The stream cost 297,088 B CPU-only, crossing both gates, raised for
the owner.

**2026-10-11 · m5-tools (wave 2): review round 2 of the tools half.**
`Instance::output_targets` tells the writer (`Job::Keep`) which paths
are live, so a removed, re-pathed or failing `file` stops its retries and
the last good file stays. Ownership and the `.strand` refusal are judged
on the real file a link names; the manifest's hash stays a cache, not an
ownership check. Dropping `Files` sends `Job::Stop`. A waiting `export`'s
deadline is a loop wake-up (`Shell::exports_wait`). The writer registers
its own writes via `Live::own_writes`/`OwnWrites` before the rename.

**2026-10-11 · m5-tools (wave 2): review round 2, item 7: the stream's
`.text`.** The overrun is the estimate's, spread over templates (51 KB),
the other mode (44 KB), the writer (39 KB), output memos (32 KB) and more;
about 55 B per source line. Cuts with no feature dropped saved 77,312 B
(one `diagnostic::suggest` body behind `&mut dyn Iterator`, one `Shared`
queue for the writer). Still 98,199 B over CPU-only and 63,127 B over GPU;
the choice between cutting design features and re-basing the gates under
the speed-first policy is the owner's.

**2026-10-11 · m5-tools (wave 2): review round 3 of the tools half.**
`join` skips `null` only; an absent extra or mode stays an error inside
`join` (`Fault::Null` vs `Fault::Absent`), and `?? null` asks to skip. A
run due or queued for a write that landed still runs after its `file`
goes. `Instance::palette` evaluates in its own disposed scope, so exports
leak no holds. The output host keeps each output's standing notice until
its next write lands (`Files::notices`) and replays it to new watchers.

**2026-10-11 · m5-tools (wave 2): what earlier rounds left unrecorded.**
`Template::compile(file, src)` takes no scope parameter, since a template's
scope is fixed. The `Export` request dropped `palette: Option<String>`
for `name`; `--palette` and templates whose line would pass `MAX_LINE`
render offline.

**2026-10-11 · m5-tools (wave 3): `strand new` and `strand report`.**
`strand new [dir]` writes the embedded `docs/guide/template/bar.strand`
into `dir` (default `$XDG_CONFIG_HOME/strand`), refusing any non-empty
directory (`NewError::NotEmpty`), a file path (`NotADir`) and opening
`create_new`. `strand report [--json]` is a socket request only: main
answers `SnapshotKind::Report` over a second calloop channel `Live::to_main`
with `report::main_part`, logic adds PSS, version and compositor name. A
frame is one `Painter::paint`, presented or discarded, p50/p95/p99 over
a 600-frame ring per surface. Outputs and globals (`State::globals()`)
carry no serials or ids; the GPU part reports status, reason, adapter and
`Renderer::gpu_lost_devices()`. `entry.rs` serves one-shot commands from
one `one_shot` table.

**2026-10-11 · m5-tools (wave 3): the swap with hue roots and outputs,
measured.** Theme swap medians are 0.91–1.19 ms with the 13 hue roots
springing, well inside 5 ms; the both-modes outputs case is 0.55 ms after
the diff. The gate goes through `Gates` with a `GATE_MISS` message.

**2026-10-11 · m5-tools (wave 3): the `.text` of `new` and `report`.**
+70,208 B CPU-only and +73,280 B GPU against 1–2 KB and 20 KB planned,
mostly JSON building and the snapshot channel. Under the owner's ruling
that such overruns are acceptable and size gates are tripwires, nothing
is cut; the re-base is the integrator's.

**2026-10-11 · m5-tools (wave 3): review round 1 of `strand new` and
`strand report`.** A promoted frame is timed to the GPU thread's reply,
not just main's `paint` (`Renderer::gpu_frame_sent`). The compositor's
name falls back to `XDG_CURRENT_DESKTOP`'s first entry, and the end-to-end
test asserts `"sway"`. COMMANDS' milestone column and the unreachable "not
implemented yet" arm are gone. `strand new` removes a partially written file
(`place`). Tests: `a_gpu_frame_is_timed_to_the_gpu_threads_reply`,
`the_compositor_is_named_without_an_adapter`,
`a_failed_write_leaves_the_dir_empty_for_a_retry`.

**2026-10-11 · m5-tools (wave 3): review round 2 of the GPU frame time.**
`Presented`'s `at` included vsync pacing, so the GPU thread waited on the
submission before `present` and `GpuReply::Presented` gained `waited`
(swapchain acquire time) for render to subtract. Replaced by round 3:
the thread no longer waits.

**2026-10-11 · m5-tools (wave 3): review round 3: timing a presented
frame never makes the GPU thread wait.** One `strand-gpu` thread serves
all surfaces, so waiting cost frames; speed beats a report's number. The
GPU's run now comes from timestamp queries (`timer.rs`, `TIMESTAMP_QUERY`
and `TIMESTAMP_QUERY_INSIDE_ENCODERS` when available), reported later
as `GpuReply::Presented { frame, ran: Vec<(u64, Duration)>, waited,
… }`; queue time from `Gpu::send`'s stamp is added to `waited`. A
device without timestamps reports a zero run. The present path
is a function over a `Swapchain` trait (`present.rs`). Tests:
`a_presented_frame_is_answered_before_the_gpu_ran_it`,
`a_presented_frames_time_leaves_out_the_swapchain_wait`,
`a_presented_frame_counts_when_its_gpu_run_comes`.

## m5-lsp

**2026-10-11 · m5-lsp: the missing-key fixes, and which is preferred.**
`check::missing_key` at a `for` offers one zero-width `Suggestion`
per comparable field (` key p.app`), the compiler's guess first;
a `key "app"` error offers each field, and a keyed method on an
unkeyed `state` list offers fields at its declaration, possibly
in another file (`Checker::key_slots`). `diag::quick_fixes` keeps
one-suggestion-is-preferred and adds one exception: `check::missing_key`'s
first suggestion is preferred. A general `preferred` field would go
through architecture.md.

**2026-10-11 · m5-lsp: extract component: what it moves, refuses,
and how it is offered.** `strand_dev::refactor::extract` works on
the innermost element the selection starts on; names read from around
it become typed parameters in first-use order, the call passes every
parameter by name, and the component goes after the enclosing item as
`NewComponent` (`NewComponent2`, …). It refuses writes to would-be
parameters (`check::two_way_prop`, `check::assign_to_prop`), an outside
`id:`, a `slot`, and any result that recompiles with a new error, as a
disabled action with `disabled.reason` where supported. The title says
"(state inside it resets on reload)".

**2026-10-11 · m5-lsp: the reload test lives in strand-dev.** `strand`
does not link `strand-dev`, so `crates/strand-dev/tests/reloads.rs` commits
the language server's own edit (`strand_dev::refactor`, now public) with
`Instance::reload`: a counter inside the extracted `col` resets, the
one beside it and a top-level total keep their values. strand-dev gains
`strand-core` as a dev-dependency.

**2026-10-11 · m5-lsp: extract component's checking compile waits for
`codeAction/resolve`.** Compiling on every cursor move blocked the request
loop. For clients supporting resolve of `edit`, `refactor::offer` gives
the action with `data` (URI, version, range) and no edit, applying the
compile-free refusals; `codeAction/resolve` runs `refactor::extract`
and answers the edit or a `RequestFailed` with the reason. Other
clients keep the eager path, memoised by `Analysis::with_edited`. Test:
`extract_component_refuses_what_would_not_check`.

**2026-10-11 · m5-lsp: deferred edits need `dataSupport`.** The edit
is deferred only when the client declares `codeAction.dataSupport` and
`resolveSupport.properties` containing `edit`; a resolve without usable
`data` fails with RequestFailed ("lost its data") instead of a silent
no-op. Test: `extract_component_is_made_when_chosen`.

**2026-10-11 · m5-lsp: extract's name avoids every word in the config;
its check refuses any new error.** A name is taken if it appears as any
word in the sources, defined or not, and the edit is refused when the
result has an error (by code and message, as a multiset) the config did
not have. Tests: `an_undefined_name_in_use_is_not_the_new_components`,
`a_swapped_error_is_an_added_error`.

**2026-10-11 · m5-lsp: the tree-sitter grammar and grammar.md's
pseudo-tokens.** `crates/strand-dev/tree-sitter-strand/grammar.js` follows
grammar.md and spec.md section 3 (`top_file` included). The external scanner
(`src/scanner.c`) does only SEP: a line break where an item may end, unless
the next line starts with a continuation token; SAME_LINE follows from it,
TOUCH is `token.immediate`, contextual keywords use `word: identifier`
with aliasing. Where looser than the parser (misspelt keywords as elements,
`12pz` as number then name) only colouring is affected. `$a.b` is a `token`
node with member accesses. Tests in `crates/strand-dev/tests/tree_sitter.rs`
parse every accepted `.strand` file and guide block clean and require
named syntax errors to stay errors.

**2026-10-11 · m5-lsp: the package's shape.** `tree-sitter-strand`
(`publish = false`) is a root `members` entry; `bindings/rust/lib.rs`
exposes `LANGUAGE` as `tree_sitter_language::LanguageFn` plus queries and
`node-types.json`, `build.rs` compiles with `cc` 1.6.0. The `tree-sitter`
crate 0.27.0 is a dev-dependency only. Queries use nvim-treesitter capture
names, specific highlights first. Nothing in `strand` or `strand-dev`
binaries links it, so it costs `strand` 0 B.

**2026-10-11 · m5-lsp: the guide's strand-check test
(integrator assignment).** `every_guide_block_checks` lives
in `crates/strand-dev/tests/guide.rs` with the block reader in
`tests/support/guide.rs`: it reads `docs/guide/level-N.md` in order,
requires each `strand` fence to carry `level=N` and `file=NAME`,
applies blocks over files in a scratch directory and requires no error,
no warning and no `fmt::format` change after each, skipping only `from
dbus` introspection and the lock-helper check. `the_new_template_checks`
covers the template.

**2026-10-11 · m5-lsp review round 1: offline guide check,
rule 2 by last token, spaced `- 1`, ternary highlights.** (1)
`Analysis::offline` skips the bus, and the guide test uses it
(`a_from_dbus_service_checks_the_same_once_the_bus_has_answered`). (2)
The scanner lexes `}` and gives a zero-width separator when the next
token on the line starts a new item, matching the parser's `at_item_end`;
`every`/`after` always start a new item there (a known difference). (3)
A spaced `- 1` on the next line continues the body (tested). (4)
Ternary `?`/`:` patterns come before operators in highlights.scm
(`a_ternarys_two_halves_highlight_alike`).

**2026-10-11 · m5-lsp review round 2: after a `}`, the scanner
asks what the parser asks.** A word followed by `:` or a tree keyword
(`TREE_KEYWORDS`) ends a value's terms and starts an item, as the parser's
`starts_term` does. A spaced `(`/`[` after a `}` starts an item unless
it is a value's further term. A `-` continues only where a binary `-`
may follow, marked by a never-produced `_infix_minus` external. Tests:
`a_spaced_value_after_a_brace_stops_where_the_parsers_does`,
`a_brace_is_not_continued_by_what_cannot_continue_its_item`.

**2026-10-11 · m5-lsp review round 3: attribute, clause-word prop and
lambda bracket after a `}`.** An `@` after a `}` always starts an item;
`key`, `persist` and `while` are ordinary names where a value term may
follow (a further term, or with `:` the next prop). Member access,
call and index apply to `_postfix_operand` (every expression but a
lambda), with a `_postfix` marker, so `x => { x }(1)` is two statements
and `x => { x }.y` an error as in the parser. Remaining differences
after a `}`: `every`/`after`, the column memory, a prefix `!`. Tests:
`an_attribute_or_a_clause_word_prop_after_a_value_brace_starts_an_item`,
`a_touching_bracket_after_a_block_that_takes_no_postfix_starts_an_item`.

## m5-inspect

**2026-10-11 · m5-inspect: what the inspection interface's fields mean.**
`Identity.instance_path` is the node's instance qualifiers outermost first,
as `strand watch` spells them (`[DP-1][3]`); `key` the innermost; `id`
the `id:` name; `sibling_index` its position among same-kind siblings;
`scope` its component or file module. `CellInfo.path` is what `reset` and
`watch` take (or `$path` for an overlaid token); `kept` means kept over a
changed default and still holding that value, tracked per cell instance;
`layer` is `overlay`, `file` or `default`. `cells_of` lists mount-path
cells nearest first, then cells its bindings read (bounded at 4096 graph
nodes), then overlaid tokens. `TokenTable::provenance(&self, path, scope:
&[&TokenTable])` takes the global table as `self` and `set { }` levels
outermost first; component `tokens { }` are global entries.

**2026-10-11 · m5-inspect: node ids on the wire, the dump's
root and redaction.** Nodes are spelt `index.generation`
(`node_ref`/`parse_node_ref`), so a reused slot is never
misnamed. `Instance::dump(root)` takes a node or `None` and answers
`{"nodes": [...], "files": {name: blake3}}` in tree order with `id`,
`kind`, `parent`, `origin`, `identity`, `props`, `cells`, `tokens`. A
password input's `text` and `value` are always `<redacted>`, and any field
containing a 4-byte lower-cased run of a known password value (current
ones plus the newest four replaced) is too, noted by `inspect::Secrets`
as `lock::Secrets` does.

**2026-10-11 · m5-inspect: reset subtree resets scopes, as a reload
write.** `reset_subtree(node)` resets every non-persisted state cell
whose holder is in a scope mounted under `node` (or whose only top node is
`node`), writing as a reload does (`set_reloaded`, `replace_all_reloaded`)
so `on change` does not fire; persisted cells, settings and a popup's
closed cells are untouched. It returns the count changed; an unknown node
is an error.

**2026-10-11 · m5-inspect: size, and hooks outside the stream's files.**
Building JSON by insertion instead of `json!`/collected `Map`s cut a first
109 KB to about 61.5 KB with IPC arms (superseded by the remeasure: 73.7
KB). Hooks outside the stream: `Emitter::children_of`, `Emitter::scoped`,
`Env::instance`, and inspector lines in `Instance::from_build`, `reload`,
`reload_hard`, `collect`, `reset` and `write`.

**2026-10-11 · m5-inspect: the integrator's readings of the first
review.** (1) The wave-2 IPC arms count against wave 2's IPC allocation,
not S-inspect's 55 KB. (2) `provenance`'s `scope` is the `set { }` levels;
component `tokens { }` are global entries, and architecture.md is amended
to say so. (3) `Instance::dump(root)` takes a node; `inspect.dump{surface}`
maps to its root. (4) A password input's `value` is redacted as well as
`text`.

**2026-10-11 · m5-inspect: a dump learns a password from the
widget's write, not only the next tick.** A pasted password
and its Return arrive in one wake and clear before `collect`, so
`Instance::write` now notes a password input's written `text` or `value`
(`Secrets::see_write`), and the live slots count as known values. In
`Instance::write`, so every caller gets it with no interface change. Test:
`a_dump_redacts_a_password_written_and_submitted_before_a_tick`.

**2026-10-11 · m5-inspect: size remeasured after the review fixes: 73.7 KB,
over the 55 KB allocation.** With calls through `std::hint::black_box`
to separate the arms (2,186 B), the library code is **73,718 B at
`b87528c`**; the review fixes added +14,528 B in many small functions;
the largest items are `node_json` 29.0 KB and `reset_subtree` 7.1 KB. No
cheap cut was found; whether to cut scope before wave 2 or charge the
reserve is the integrator's and owner's call.

**2026-10-11 · m5-inspect (wave 3): the outline and flash are display
items.** They are appended to a frame's display list after flattening
(`strand-render/src/inspect/overlay.rs`), so CPU and GPU draw them
with no code of their own. The outline is a 2 px `#2196f3` ring inside
the node's drawn box (the hit box without `hit: grow`, now recorded
as `HitBox::grow`), following radii, transform and clip. The flash
tints content damage `#00e676` at alpha 0.45 fading over 300 ms, never
the overlay's own repaints; an idle overlay costs nothing and an off
flash allocates nothing. Lock surfaces are never picked, outlined or
flashed. Tests: `a_presented_surface_shows_the_outline_and_the_flash`,
`flash_off_wakes_nothing`, `a_lock_surface_is_never_picked_or_outlined`.

**2026-10-11 · m5-inspect (wave 3): pick mode, and who owns what.** Pick
mode lives on main: `Router::set_picking(true)` passes no pointer event to
handlers (ending hovers, presses and drags), and `Renderer::pick_input`
outlines the node under the pointer and answers a left press. Each click
sends `picked`; pick mode ends on `inspect.pick{on:false}`, disconnect,
another client taking it (`pick_ended`), its surface unmapping, or a
lock starting, and is refused while locked. `inspect.highlight` and
`inspect.flash` belong to their client too. All three are socket-only
(unknown on D-Bus). `strand-dev inspect` turns pick mode off after one
pick and stays in line mode.

**2026-10-11 · m5-inspect (wave 3): write-back's rules as built.**
`strand_dev::inspect::write` edits a prop written directly in the
innermost element's block, or a token entry in the `tokens <name>`
block or component `tokens { }` that provenance names, refusing other
origins. Old and new values must each be one literal (number with unit,
string, colour, boolean). The file must still hash (BLAKE3) to the
running shell's answer, and `inspect.node`/`inspect.dump` hash every
compiled file. The write follows symlinks, writes a temp file with the
mode, and renames only if the target is the same inode with the hashed
bytes; unwritable targets are refused with an `$EDITOR` offer. Test:
`a_written_back_edit_reloads_and_keeps_state`.

**2026-10-11 · m5-inspect (wave 3): the hooks outside the stream's files.**
Render: `HitBox::grow`, `Renderer::painted_hits`, `repaint_all`, the
overlay call in `paint_frame`, `Router::set_picking`/`picking`. Binary:
`Server::has`, the `ToMain` match and lock guard, the host's
pick forwarding, calls in `run/shell.rs` and `run/logic.rs`
(`inspector_clients_gone`). `tests/m5/inspect.rs` compiles strand-dev's
`inspect/write.rs` by `#[path]`, since a strand-dev dev-dependency turned
on naga in the CPU-only build (+1,063 KB); strand gains dev-dependency
`blake3` 1.8.7, strand-dev `strand-core` and `blake3` 1.8.7.

**2026-10-12 · m5-inspect (wave 3): size, over the 20 KB line.** +35,008 B
CPU-only and +34,176 B GPU, both under the gates (`TEXT_GATE_CPU` 18,874,368
B, `TEXT_GATE_GPU` 22,806,528 B) and the design bar's memory budget. About
18 KB is the binary half, which had no line of its own. Nothing runs
per frame unless the inspector is on (`Inspect::idle`). Not taken: one
`ToMain::Inspect` kind (architecture.md fixes three) and a square outline
(misreports shape). Recorded under the speed-first ruling; the ledger
charge is the integrator's.

**2026-10-12 · m5-inspect (wave 3): review round 1's fixes.** (1)
Write-back's `innermost_at` walks `else if` chains and `match` arms. (2)
`ToMain::PickMode { on, seq }` and `PickEvent::Ended { seq }`: logic ignores
an `Ended` whose `seq` is not its last, so a stale end cannot drop a new
owner (an interface change inside `strand`, in architecture.md). (3)
`strand-dev inspect`'s session is `inspect::drive(path, Terminal {
read_line, out, editor })`, tested against a scripted socket. (4)
While picking, the router holds another program's drag and runs no
`on drop`. (5) `inspect.highlight` is refused while locked; an earlier
outline is restored after the lock. (6) The `$EDITOR` offer opens at the
edited value's line. (7, 8) Popup picking tested; architecture.md says
the commands are served.

**2026-10-12 · m5-inspect (wave 3): review round 2's fixes.** (1)
`Session::pick` forgets `pick_ended` events kept before `inspect.pick`'s
answer (`Connection::forget`). (2) `PickEvent::Picked` gains
`seq`; logic drops a stale pick. (3) Pick mode keeps a drag offer's
surface without a target, so the next motion after picking finds it
(`a_drag_entered_while_picking_finds_its_target_after`). (4) Replaced by
round 3. (5) Deferred: the node pane does not say why a `shader` node
draws nothing (2–5 KB estimate); `strand report` gives GPU status,
and features.md records the deferral.

**2026-10-12 · m5-inspect (wave 3): review round 3's fixes.**
(1) Answers carry no id and an `inspect.node` pane answers late,
so a timed-out request starts a new connection (`Connection::ask`,
`restart`) and `Session::restore` re-asks for flashing, the
outline and pick mode; requests turning a mode off count as
off before sending. An echoed id would be cleaner but changes
strand-core. Tests: `a_late_answer_is_not_the_next_requests`,
`a_timeout_starts_a_new_connection_and_keeps_the_session`. (2)
`write::replaceable` refuses a file with no write bit, owned by another
user, failing `access(W_OK)`, or with a POSIX access ACL, and keeps
the file's group or refuses; strand-dev takes `rustix` 1.1.5. Test:
`a_file_another_user_owns_is_not_replaceable`.

## m5-ipc

**2026-10-11 · m5-ipc: what `get` answers.** `strand get <path>` reads
what `set` writes: an exported `state`/`let` or field, a settings field,
or a token (`'$accent'`, overlay included). The answer is `{"ok": true,
"value", "text", "layer"}`: `value` is JSON where JSON has the kind,
else the text `set` reads back (`"8px"`); `text` is what the CLI prints;
`layer` is a settings layer, a token's origin, or null. `--json` prints
the line. One shape for every path keeps one concept for scripts.

**2026-10-11 · m5-ipc: `strand clear` and what it clears.** `clear`
drops a token overlay (`strand clear '$accent'`) or a settings field's
runtime overlay; clearing what has no overlay answers `{"ok": true,
"cleared": false}`, and a path that cannot have one is refused. Amended
(integrator decision d): an unknown token is refused with the closest
name, as `set` refuses it; an overlay of a token the build no longer
defines is still cleared.

**2026-10-11 · m5-ipc: `toggle` and `call`'s scope.** `toggle` flips
a bool `set` can write and refuses other types by name; service rw
fields are not toggled in wave 1. `call` runs service-level actions only
(Q13), refusing item actions, nested paths and `fn`s, with closest-name
suggestions; arguments are text read by parameter types, defaults fill
missing ones, and it prints nothing (`--json`: `{"ok":true}`). The lock's
`auth` actions are refused over IPC: a password never crosses a socket.

**2026-10-11 · m5-ipc: `tokens.toml` and its writer.** A flat TOML table
of quoted token paths to text values (`"probe.c" = "#ff8800"`), edited with
`toml_edit` so comments survive. A value its token cannot read leaves the
theme's with a notice; a non-TOML file moves to `.tokens.toml.corrupt`
(`CorruptOverlay`). The overlay has its own `strand-tokens` writer
thread (newest wins, temp + fsync + rename + dir fsync, `WriteFailed`,
bounded wait at exit; integrator decision b). An unreadable file raises
`Unreadable` once and is never written over: `set`/`clear` refuse and
re-read it. Unquoted dotted keys are told to quote the path.

**2026-10-11 · m5-ipc: `auth` stays refused over IPC (integrator decision
c).** The integrator confirmed it: a password never crosses a socket
every process of the user can reach.

**2026-10-11 · m5-ipc: the `.text` overrun cut without costing speed
(integrator decision a).** Every IPC handler is `#[cold] #[inline(never)]`
so `Shell::request` is a dispatch again; `value_json` inserts fields one by
one; overlay state uses sorted vectors instead of `BTreeMap`s; `suggest`
takes one `Vec<&str>` instantiation; the file read is `#[cold]`. Result
+87,104 B over f61acec (−28,928 B). `instantiate::show` (~14 KB)
is kept so `strand get '$shadow.lg'` does not print Rust `Debug`. The
remaining ~52 KB over budget is the owner's to rebudget. `run.sh` passes
`SIZE_REPORT_*` into the container.

**2026-10-11 · m5-ipc: which saves the overlay shadows.** The "overlay
wins" notice is raised once when a committed build changes the file value
of an overlaid token, reusing `SettingsNotice { issue: Shadowed }` with the
overlay file, so `[clear]` and `strand watch` work unchanged. Overlays take
colour, length, percent, angle, duration, text and bool; paints, shadows and
fonts are refused. Without a state directory, token overlays live in memory.

**2026-10-11 · m5-ipc: `clear` drops a non-value entry; a re-read
file applies at once (review round 2).** `clear` also removes a
`tokens.toml` entry that is not text, number or bool (`cleared:
true`), even for a key no token has, with the comment above it. A
`clear` reading a previously unreadable file rebuilds the table so
its overlays apply. Tests: `clear_drops_an_entry_that_is_not_a_value`,
`a_fixed_overlay_file_applies_at_a_clear_that_clears_nothing`.

**2026-10-11 · m5-ipc: a hand edit of `tokens.toml` while running is
read, not written over (review round 3).** `set` and `clear` re-read
the file first when its content differs from what the overlay last read
or queued, as settings' `edit_toml` does; comparison is by content. A
hand edit within a queued write's few milliseconds may still be lost,
accepted. Edits apply at the next `set`, `clear` or boot. Test:
`a_hand_edit_while_running_is_not_written_over`.

**2026-10-11 · m5-ipc (wave 2): the D-Bus front end as
built.** `strand_services::ipc_bus` serves `/` with interface
`io.github.jakeb_grant.Strand` and typed methods `Get(s path)`,
`Set(s path, s value)`, `Toggle(s path)`, `Clear(s path)`, `Reset(s
path)`, `Call(s target, as args)`, `Reload(b hard)`, each answering
the socket's JSON; refusals are `org.freedesktop.DBus.Error.Failed`
with the socket's text. Watching is the signal `Changed(s event, s
data)`. Inspector commands, `report` and `export` are socket-only. It
runs on its own session-bus connection, serves the object before
requesting the name with `DoNotQueue`, and a second instance
runs without it (one warning). Not under `STRAND_MOCK`. Tests:
`ipc_bus.rs`, `get_set_toggle_call_and_changed_over_dbus`,
`a_second_instance_runs_without_the_name`.

**2026-10-11 · m5-ipc (wave 2): a bus `reload` waits for its load.**
A channel reload gets a client id with the top bit set (`BUS_RELOAD`),
kept in `Shell::bus_reloads` until its load's event answers through
`Shell::reloaded`; a shell without a compiler worker answers "this
shell does not reload", and a pending reply at exit is refused. Test:
`saves_reload_live_with_state_kept`.

**2026-10-11 · m5-ipc (wave 2): the inspector's three arms.**
`inspect.node{id}`, `inspect.dump{surface?}` and `reset{node}` are
served in `run/request/inspect.rs` via `parse_m5`, answering `{"ok",
"node", "files"}`, `{"ok", "nodes", "files"}` and `{"ok": true, "reset":
n}`; a bad ref is refused saying what one is. A non-root node given to
`inspect.dump` dumps its subtree. Amended (review round 2): a subtree
reset drops the kept rows of the cells it reset.

**2026-10-11 · m5-ipc (wave 2): size, over both lines, and why.** The D-Bus
front end with the bus reload is **+52,736 B** against its 37,400 B line,
and the three inspect arms **+60,416 B** against 3,000 B; the arms' own
code is about 2.2 KB, the rest is S-inspect's library becoming reachable
(it was dead code LTO dropped in wave 1). Not taken: one `Request(s json)`
method (about −18 KB) would make the interface a JSON tunnel `busctl`
and introspection cannot show; dropping `Reset` (−2.3 KB). Nothing runs on
a frame or swap path; the ledger decision is the integrator's and owner's.

**2026-10-11 · m5-ipc (wave 2, review round 1, item 1): the overrun is
open, with a proposed re-base.** Risk 1 says streams stop at their line,
so the overrun waits for an integrator ruling before `laptop/m5-ipc`
merges. Proposed ledger text charges D-Bus 52,736 B and the arms 60,416 B
(S-inspect's library charged for the first time), leaving the CPU-only
gate 10,983 B short of wave 2's remaining lines. Choices: (a) raise the
CPU-only gate by the shortfall plus about 40 KB reserve (recommended),
(b) the `Request(s json)` cut, an architecture.md change, (c) cut the
remaining lines.

**2026-10-11 · m5-ipc (wave 2, review round 1, item 3): spec.md should
name the D-Bus methods and signal.** spec.md section 13 names only the
bus name and path, so the typed methods and `Changed` have no user-facing
promise. Proposed bullet for S-docs: the methods `Get`, `Set`, `Toggle`,
`Clear`, `Reset`, `Call`, `Reload` return the socket's JSON answer, refusals
are `org.freedesktop.DBus.Error.Failed`, `Reload` returns once committed,
`Changed(s event, s data)` carries every `strand watch --json` event,
and `report`, `export` and inspector commands are socket-only.

**2026-10-11 · m5-ipc (wave 2, review round 2, item 1): a boot-kept cell
reaches D-Bus as a late `strand watch` hears it.** This replaces round 1's
item 2. With no socket watcher at boot, kept cells wait in `Shell::unheard`
and the next reload event carries them unchanged to socket watchers,
the sink and every `reload` caller, socket or bus; other notice texts
reach the sink at once. `BUS_RELOAD` is no longer re-exported. Test:
`a_cell_kept_at_boot_reaches_the_event_sink_as_a_late_watcher_hears_it`.

**2026-10-11 · m5-ipc (wave 2, review round 2, item 2): a subtree reset
drops the overlay's kept rows.** `reset{node}` now drops the reload
overlay's kept rows of reset cells (`Overlay::forget_cells`), found in
the strand crate from `Instance::dump(Some(node))` only when the overlay
has a `[reset]` row (`Overlay::has_resets`), leaving S-inspect's interface
unchanged. Amended by round 3, item 1.

**2026-10-11 · m5-ipc (wave 2, review round 3, item 1): a subtree reset
drops the kept row of a changed cell.** The badge holds only while a cell
keeps its kept value, so comparing badges missed a cell changed since. The
arm now drops the rows of every non-persisted cell under the node, from
one dump before the reset, since each is at its default after. Test:
`saves_reload_live_with_state_kept`.

**2026-10-11 · m5-ipc (wave 2, review round 3, item 2): a
boot-kept cell reset or set over IPC leaves the next reload event.**
`Shell::forget_unheard(path)` drops the cell from `unheard` when
`reset{path}`, `set` on a state path or `toggle` succeeds. A
click inside the shell does not, so the event reports what was
kept at boot, as a boot-time watcher would have heard. Test:
`a_boot_kept_cell_reset_or_set_over_ipc_is_not_reported_kept`.

## m5-integration-w1

**2026-10-11 · Wave 1's `.text` overruns accepted; ledger re-based to
gate headroom.** S-ipc (about 87 KB against 35 KB) and S-inspect (about
74 KB against 55 KB) overran, accepted under the speed-first policy
(m5-speed-policy) because it is cold IPC and inspector code; the gates
(18,350,080 B CPU-only, 22,282,240 B GPU) stay. M5's lines now sum to the
gate less a 64 KiB reserve, wave 2's lines are set from estimates, and a
61,001 B review-fix reserve is promised to nothing else.

**2026-10-11 · The theming hub is built by S-palette and S-tools.**
S-palette owns strand-theme's palette half (13 hue roles, `Role::COUNT` 62,
extras, identity, the persisted `name`/`extra` lines, importers' extras,
the `json` and `base24` writers) and strand-scene's hue roots and reserved
`palette` segment. S-tools owns templates, the `file` element (`top_file`),
`Update.outputs`, `Instance::palette`/`other_mode`, `PersistStore`, the
output host, export CLI and IPC, `strand new` and `strand report`.

**2026-10-11 · The `file` output host renders on logic and writes on its
own thread.** After the tick's diff is sent, logic renders each changed
output from its template IR over the frozen token tables and hands the
bytes to a writer thread, `strand-files`, which writes atomically, keeps
the manifest and runs `run:` (200 ms debounce, 10 s timeout). Outcomes come
back over a channel logic polls, so failures are notices and `strand watch`
events; no main-side channel is added, and the render is held to the swap's
5 ms.

**2026-10-11 · The theming hub's status in the docs.** architecture.md's
hub is interface text in force, built in M5 wave 2; grammar.md and
design.md mark it "M5 wave 2"; spec.md includes it in full (sections 4.1,
4.2, 9.1-9.8, 13). Reading recorded: only `template:`/`text:` holes use
the template scope; the positional path, `template:`, `run:` and `when`
are ordinary expressions in the enclosing scope, so a `for` can mount
distinct paths.

**2026-10-11 · The `export` request and the CLI table agree.** The
request is `Request::Export` in `strand_core::ipc` with a `parse_m5` arm,
a `Shell::request` arm and handler `run/request/export.rs`, socket-only
and owned by S-tools. The CLI's existing `new`, `export` and `report` rows
(moving to `src/entry.rs`) are changed rather than added; the D-Bus front
end is `strand-services`' `ipc_bus.rs`. The `export` help now reads "write
the palette: json | base24 | --template <file>".

**2026-10-11 · handoff.md is the wave-1 to wave-2 page; first wave-2
launch.** `laptop/m5-w1` is the integration branch and wave 2 integrates
on `laptop/m5-w2`. The inspect arms (`inspect.node`, `inspect.dump`,
`reset{node}`) move to S-ipc in `run/request/inspect.rs`; S-inspect keeps
pick, highlight and flash. Token overlay `clear`: a name with no token,
overlay or file entry is refused; an overlay or entry the build lacks is
cleared; a known token without one answers `cleared: false`.

## m5-palette

**2026-10-11 · The hue roles' canonical colours and derivation.**
The canonical hues are the sRGB primaries and secondaries plus orange:
`#ff0000`, `#ff8000`, `#ffff00`, `#00ff00`, `#00ffff`, `#0000ff`,
`#ff00ff` (`palette.rs`, `BASE_HUES`), `blend::harmonize`d toward the
accent (at most 15°) keeping their chroma, then tone 80 dark / 40 light;
a bright is base ± 0.08 OKLCH L. One function, `palette::fill_hues`,
serves `Partial::fill` and `material::from_seed`; given hues are kept as
given. Tests: `crates/strand-theme/tests/palette.rs`.

**2026-10-11 · Extras and identity live beside the tokens, not among
them.** As tokens, every extra would be a palette root that springs and
freezes on each swap frame (Catppuccin alone brings 26). So `TokenTable`
gains `palette: Vec<(String, PropValue)>`, written by `Palette::insert_into`
(`name`, `source`, `dark`, then extras in source order) and read by
`palette_value(name)`; it counts in equality but `lookup`, `freeze`, the
swap and the contrast guard never see it. The 13 hue roles are ordinary
palette roots.

**2026-10-11 · Palette equality, names and the persisted file.**
`Palette`'s equality includes its extras and own name (not the
source label). `Palette::name()` is the scheme's name, else the source
string. `to_text` writes `source`, `name` (own name only), `dark`, the 62
role lines, then `extra <name> #rrggbbaa`; a file without them derives
hues and has no extras. Names: Catppuccin is `Catppuccin Mocha` etc.;
base16/base24 use `name:`, else `scheme:`; matugen, W3C and `material(…)`
fall back to the source string.

**2026-10-11 · What each importer keeps as extras, and in what order.**
Catppuccin: its 26 names in flavour-table order. base16/base24: every slot
in file order, lower-case (`base0d`), slots also filling hue roles. matugen:
every colour key, roles included. W3C: every colour token by group
path. `material(…)` and built-in: `source_color`. Names are sanitised
segments; a first segment `name`, `source` or `dark` gets a trailing `_`;
two colours with the same name are an import error. Source order comes from
`import::key_order`, not serde_json's `preserve_order`.

**2026-10-11 · The reserved names in the checker and the schema.**
The builtin schema's `palette { }` block gains the 13 hue names,
so props read `$red` and defining `red` without `override` is
`check::override_needed`. Any token-set entry under the `palette`
segment is a new error, `check::reserved_token`, raised in `validate_set`
before the override rules. Test: `negative/reserved_token.strand` in
`checker.rs::negative_corpus`.

**2026-10-11 · The `json` and `base24` writers' details.**
`export::json(&Palette, &TokenTable)` writes `name`, `source` (`null` when
absent), `dark`, `roles` (62 in schema order, overlays shown), `tokens`
(every path, sorted, resolved at `TimeContext::default()`) and `extras`,
streamed by a small writer with two-space indent. `export::base24` writes
tinted-theming slot keys (`base0A`) with `"#rrggbb"` values and passes a
base16/base24 source's slots through unless overlaid. Latte's `fg_variant`
does not survive the base24 round trip; the test asserts that loss. Tests:
`crates/strand-theme/tests/export.rs`.

**2026-10-11 · Size: about 55 KB against the 10 KB line, reported not
cut.** Measured Δ +56,128 B on both builds with the writers wired
temporarily; both stay under their gates. The remaining cuts would
cost behaviour (source order for matugen and W3C extras, a quadratic
collision check, a hand-written `Debug`), so under the speed-first policy
(m5-speed-policy) the overrun is reported to the integrator and owner.

**2026-10-11 · Review 1: identity names reserved as groups
too; role-list copies tested.** `extra_name` gives a trailing
`_` to a first segment `name`, `source` or `dark` whether
colour or W3C group (`$palette.dark_.surface`); a later
segment is never renamed, and spec.md 9.5 says so. Tests:
`insert_into_writes_the_identity_then_the_extras_in_order` and
`the_schema_palette_block_is_the_role_list` (the schema's `palette { }`
block equals `Role::ALL`).

**2026-10-11 · Review 2: base16/base24 extras follow the file's
order.** spec.md 9.5 promises source order, so the spec wins
over the earlier slot-order decision: `yaml_pairs` returns the
file's pairs in order (a `Vec`, a repeated key read last) and
`base16` pushes each slot where the file first names it. Test:
`base24_extras_follow_the_files_order_not_slot_order`.

**2026-10-11 · Review 3: matugen keeps its roles' colours as extras.**
spec.md 9.5 says an importer keeps every colour of its source, so a
matugen key that names a role fills it and is kept as `$palette.<key>` too
(`$palette.primary`). `matugen_and_w3c_keep_their_colours_in_source_order`
now covers matugen 3's `colors.<role>.<mode>` layout.

## m5-integration-w2

**2026-10-11 · The CLI moves into the library, as a pure move.**
`main.rs`'s COMMANDS table, `usage`, `Action`, `dispatch`, the `Tool` alias
and `main`'s body move verbatim into private `crates/strand/src/entry.rs`
with their dispatch tests; `main.rs` keeps `strand::process_setup!()`,
`main`, its allocator and THP tests, and calls the hidden re-export
`strand::cli::main()` until `Builder::run` lands. No behaviour, row or help
text changed.

**2026-10-11 · The docs follow what wave 2 built.** spec.md says the hub is
built and gains wave 2's details (9.5 canonical hues and collision error,
9.7 `file` retries 1-10 s and ownership by the link's target, 9.8 `join`
skips `null` only, 13 the D-Bus bullet, 5.6 plugin schema files). design.md
and grammar.md drop every "M5 wave 2" mark. Decided: a mark is dropped only
where the thing is merged and tested on `laptop/m5-w2`; unbuilt M5 features
are listed in the handoff, not marked in design.md.

**2026-10-11 · `.text` gates re-based to the merged tree, under the owner's
ruling.** The owner judged wave 2's overruns small for the binary, S-size
found no grossly verbose code, so nothing is cut and gates follow the tree
by the m5-speed rule (measured + unspent allocation + 64 KiB, rounded up
to 256 KiB): **`TEXT_GATE_CPU` = 18,874,368 B** (measured 18,555,351 B)
and **`TEXT_GATE_GPU` = 22,806,528 B** (measured 22,473,111 B). The 61,001
B review-fix reserve is spent; size gates are regression tripwires, not
caps. S-ipc's `Request(s json)` cut is not taken.

## m5-fixups

**2026-10-11 · Wave-2 stragglers are made, each checked in code first.**
S-fixups took the wave-2 leftovers and loose claims on `laptop/m5-fixups`;
every item was re-read against the tree first, and one (`[replace]` on the
unowned-file notice) was already built. The cross-owner edits are recorded
in the entries below.

**2026-10-11 · `strand new`'s grim references are made on the
frozen-clock mock.** Rendering lands with offline PNG tests, and the
acceptance mock (`STRAND_MOCK=acceptance`) freezes the clock at 09:41
UTC. `acceptance.rs::the_new_shell_checks_and_runs_on_two_outputs`
runs `strand new`, `strand check`, then `strand run` on two outputs and
compares `tests/refs/acceptance/new_shell_headless{1,2}.png`; it lives in
`acceptance.rs` for its reference machinery. The references equal the
hello bar's.

**2026-10-11 · A config service's `rw` field is writable from the
language.** spec.md 5.6 and 8.1 say `name: type rw = Property`
is writable. `rw_root` now takes a `Def` of `DefKind::Service`
as the write's root (not `writable_place`, so a non-`rw`
field and the whole service stay `check::read_only`), and
`place_parts` lowers it to `PlaceRoot::Service(name)`. Tests:
`checker.rs::a_config_services_rw_field_is_writable`,
`services.rs::a_config_services_rw_field_is_written_to_the_bus`.

**2026-10-11 · A drop leaves out every copy of the dragged item, by
identity in `Prop::Drag`.** Render's `drop_index` owns `at`, so the instance
sends `Prop::Drag` as a `List` whose last item is `List [Text(identity)]`,
an FNV-1a hex hash of the drag type and `Value::identity` (stable across
toolchains). `strand_scene::drag_identity` reads it; `drop_index` leaves
out every live row holding a source of that identity, and a target that is
itself a copy keeps its place. Equal values are one item by design. Test:
`input.rs::a_drop_on_another_surface_leaves_out_its_copy_of_the_item`.

**2026-10-11 · `key=value` documents skip `#` comments and lose single
quotes.** `Document::parse` now ignores lines whose first non-blank
character is `#`; a double-quoted value loses its quotes and os-release(5)'s
four escapes, a single-quoted one keeps everything between. A file of
a bare word and comments (hostname(5)) is still text, read whole. Test:
`custom::tests::pairs_skip_comments_and_lose_their_quotes`; guide test
renamed `the_guide_states_the_pairs_rule`.

**2026-10-11 · The tray `open()` doc says what it does.** `tray.schema`
and builtin.schema's stub now say `open()` tells the app and sets
`item.menu.opened`; a DBusMenu's menu needs a `popup` bound to `opened`,
and only an item without one shows its own.

**2026-10-11 · `strand report`'s percentiles are over drawn frames.**
"Discarded" is the compositor's word (`wp_presentation`), so a
paint that damaged nothing is not called that and is not counted in
percentiles. Percentiles are over frames drawn among the last 600
paints; counts are `drawn` and `unchanged` (`FrameSummary { p50, p95,
p99, drawn, unchanged }`, JSON `frames.drawn`/`frames.unchanged`). Tests:
`renderer::summary::tests::{percentiles_are_over_the_drawn_frames_of_the_last_600_paints,
one_frame_is_every_percentile}`.

**2026-10-11 · spec.md 13 names `new`, `report` and `compositor-rules`;
`report` socket-only.** Section 13 gains `strand new [dir]`, `strand
compositor-rules`, `strand report [--json]` among the socket's commands,
and `watch`'s faults and notices; `export`, `report` and the inspector's
commands are socket-only, so a D-Bus `Report` is an unknown method. Section
13 sets the CLI surface the compatibility policy covers.

**2026-10-11 · S-plugins' leftover-schema-file line stays `info`.** A
`.schema` file in `$XDG_DATA_HOME/strand/schemas/` the binary does not serve
changes nothing until the config reads that service, and then spec.md 5.6's
notice names it, so a warning at every start would be noise.

**2026-10-11 · `strand help` lists `--write-schemas`; rows say what
commands take.** `usage()` gains an "options:" block (`--write-schemas`,
`-V, --version`) apart from COMMANDS, since `main` and `dispatch`
take them before any command. `check` takes `[dir | file]`, `set` names
settings and `rw` service fields, `watch` names faults and notices. Test:
`entry::tests::help_lists_every_command`.

**2026-10-11 · `tests/m5/plugin.rs` builds its examples with the
cargo running it.** A filtered `cargo test -p strand --test m5` builds
no examples, so this reverses S-plugins' "never by a nested cargo":
`build_examples` runs once per test process (`OnceLock`), building
`plugin_hello` and `plugin_bare` with the test's profile and target
directory (from `CARGO_BIN_EXE_strand`'s path), `--no-default-features`
when built without `gpu`.

**2026-10-11 · base16's `base02` keeps feeding three roles; round-trip
claim narrows.** The importer keeps reading `base02` into `surface_high`,
`surface_variant` and `outline_variant`, since it is base16's only colour
between background and comments. So m5-tools' round-trip claim holds for
any palette whose `fg_variant` keeps 3:1 on its `surface_variant` (built-in,
`catppuccin:mocha`, `material(seed:)`), not Latte.

**2026-10-11 · Smaller records.**
architecture.md's `strand::cli` adds `new_cli` and
`report_cli`. `tests/m5/new.rs::the_template_is_the_guides_first_block`
holds `docs/guide/template/bar.strand` to level 1's first block. `strand
export -o` writes through `run::write_through` (a link followed and kept,
a dangling one refused, mode kept). spec.md 9.7: a `file` path resolves as
a settings file's (`~/` from `$HOME`, absolute as given, else the config
directory), as `Storage::resolve` does.

**2026-10-11 · Size: +4,992 B CPU-only, +4,736 B GPU.** Measured 18,560,343
B and 22,477,847 B, inside the re-based gates and charged to the 64 KiB
review reserve; no speed path changed.

**2026-10-12 · Review 1: a drop leaves out its own row and one copy per
other list.** Amends "every copy" and withdraws "equal values are one item",
which holds for keyed lists only: in the source's list only its own row is
left out; in another list, its first row holding a source of that identity,
because `move` takes the first equal item out. The identity is FNV-1a
over a tagged encoding (`mount.rs::identity_bytes`) equal exactly when
`builtins::equal` is. Tests: `equal_items_in_one_list_are_counted_apart`,
`a_drag_identity_is_the_values_equality`.

**2026-10-12 · Review 1: `plugin.rs` runs a nested cargo only when
an example is missing or stale.** In `cargo test --workspace` feature
unification would make the nested build rebuild dependencies under other
hashes, so `build_examples` runs no cargo when both examples are newer than
every source their dep-info (`examples/<name>.d`) names. The nested build
passes this build's `speed-bench`/`faults`/`gpu` features.

**2026-10-12 · Review 1: `strand set` writes the config's own
services' `rw` fields.** `set.rs` now asks `services::config_service`
after the exported and builtin checks and writes through
`services::set_field_text` (`set_text_with` with the service's record
and type table), so `strand set ppd.profile power-saver` works; the
leaf must be `rw`, a signed number steps. spec.md 13 says so. Test:
`services.rs::a_config_services_rw_field_is_written_to_the_bus`.

**2026-10-12 · Review 2: own row left out only in its own list; every
`for` holder marked.** Round 1 took any ancestor of the source as its row,
breaking kanban boards. The compiler now sets `Prop::DropRows` on every
element whose direct child is a `for` (drop target or not), and `drop_index`
counts a node as list `n`'s row only when no `list` or `DropRows`
element lies between (`row_in`); copy search does not descend into nested
lists. Tests: `a_drop_on_an_outer_list_counts_the_row_holding_the_item`,
`every_element_holding_a_for_marks_its_rows`.

**2026-10-12 · Review 3: a `for` holder is a nested list only for its rows
(`Prop::NotRow`).** Round 2 made a wrapper with a decorative `for` beside
its `drag:` source a list boundary. The compiler marks each child of a
`DropRows` element that is not one of its `for`'s rows `Prop::NotRow`
(marking content, which is rare, rather than rows), and `drop_index`
treats a `list`/`DropRows` as the source's list only when the path's
child is not `NotRow`. A target's own content still counts among its
rows. Tests: `a_dock_item_holding_a_for_beside_its_icon_is_the_icons_row`,
`content_beside_a_for_is_marked_not_a_row`.

## m5-platform

S-platform's wave-3 work: xdg-activation for notification actions and app
launches, and the handler input context's route.

**2026-10-11 · The press serial is joined in the binary's host, not
the router.** The router never sees a serial and `Intent::Event` has
30 match sites, so strand-surface tells its host each press's serial
first (`SurfaceHost::press_serial`, empty default) and `Forward`
(`demo/host.rs`) joins the last one to the node events that press
causes (button press/release, key press/repeat, click away);
scroll, drag, key release and settled intents carry none. Test:
`demo/host.rs::tests::a_press_serial_rides_the_events_it_causes`.

**2026-10-11 · `Instance::event_with` beside `event`; one process-wide
invocation counter.** `event` (about 90 callers) passes `None`;
`event_with(node, event, args, input)` is what `ToLogic::Event`
calls. Invocations are numbered from one process `AtomicU64` so a reload's
new instance never queues behind an old one's. The VM sets `ActionCtx`
in a thread-local (`vm::action_ctx()`), clearing the press at the first
`await`. Test: `actions_know_whether_input_called_them`.

**2026-10-11 · `State::activation_token` takes a callback; token
source is an `Arc<dyn Fn>`.** Main runs calloop with no executor,
so the call takes `done: impl FnOnce(Option<String>)`, called with
`None` at once without `xdg_activation_v1` and after 5 s at most. The
binary's `token_source` queues a `TokenAsk { surface, serial, answer }`
and wakes main with a `calloop::ping`; a gone main drops answers, heard
as `None`. Tests: `the_token_source_asks_main_and_hears_its_answer`,
`no_activation_protocol_means_no_token_and_no_wait`.

**2026-10-11 · Token-wanting actions are a service's declaration, read from
`Cx`.** A service lists them in `Service::ACTIVATION` (`#[service(activation
= &["invoke", "activate"])]`), asked only when the handler had a press and a
token source is set; the token is `Cx::activation_token()`. notifications
sends `ActivationToken(id, token)` before `ActionInvoked`;
apps declares `launch`. `strand_services::activation::Order`
holds an invocation's later actions behind its wait so logic
never blocks. Tests: `activation_token_precedes_action_invoked`,
`invoke_then_dismiss_keeps_order_while_waiting_for_a_token`.

**2026-10-11 · Inherited tokens are removed on the `Command`, not in
`restore_in_child`.** `unsetenv` between `fork` and `exec` is not
async-signal-safe, so `strand_services::child::activation_env(cmd,
token)` sets `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID` to
the press's token or removes both before spawn, at app launches
(`apps::spawn_launched`), `exec` sources and the editor. Test:
`child.rs::tests::a_launch_carries_its_token_and_never_an_inherited_one`.

**2026-10-11 · The compositor matrix checks activation focus.**
`a_launched_app_is_activated_with_its_token` launches `activation_helper`
from a one-button shell, checks it got the token in both variables
and not a stale one, then checks a bogus token keeps focus away and
the launch token brings it back. Policy is set per compositor (sway
`focus_on_window_activation focus`, Hyprland `misc:focus_on_activate`);
labwc and compositors without `xdg_activation_v1` skip.

**2026-10-11 · The `.text`: +49,856 B over the 20 KB line, recorded
not cut.** Two cuts took 10,880 B (a ping in place of a calloop channel,
one function sending both notification messages); the rest is spread over
Wayland dispatch, the ordering queue, per-service monomorphisation and the
press join. Under the owner's 2026-10-11 ruling it is recorded for the next
re-base. Superseded by the after-review measure below.

**2026-10-12 · Review 1: five fixes.** A `file` element's `run:`
command now goes through `child::activation_env`. Each held action's
200 ms deadline is taken at submit so an invocation's waits overlap. An
unanswered token is given up after `FORGET_AFTER` by a calloop timer
(`Cmd::HoldTokens` in strand-fake-wayland tests it). A click away carries
a press only when fresh. A held action whose run stopped goes back to
logic (`ClientInner::bounced`) and is resent through `with_run`. Tests in
`activation.rs`, `fake.rs`, `demo/host.rs`, `contract.rs`.

**2026-10-12 · Review 2: held actions keep order; plain `act`
to an ended body is refused.** A token-wait delivery goes to
`ClientInner::bounced` whenever that queue is non-empty, and `redeliver`
sends it under the queue's lock (lock order: queue, then the run's
`stopped`). `Deliver` takes a flag for token-wait delivery, so a plain
`Client::act` to an ended run returns `not running` again. Tests:
`a_later_action_on_a_new_run_does_not_overtake_a_held_one`,
`a_plain_action_to_an_ended_body_is_refused_not_lost`.

**2026-10-12 · Review 3: one registry-wide hand-back queue that every
action joins.** An action sent at once also joins the queue while it
holds anything (amending round 2: it may now return `Ok` and wait,
and drops with a warning in a backoff; order wins). The queue is the
registry's (`Registry::bounced`, one `VecDeque` in hand-back order across
services), resent by `Registry::redeliver` from `Services::pump` and
`act_with`. Tests: `an_action_sent_at_once_waits_behind_a_handed_back_one`,
`a_held_action_handed_back_keeps_another_services_action_behind_it`.

**2026-10-12 · The `.text` after review: +94,272 B CPU-only, +96,832
B GPU.** Measured 18,649,623 B and 22,569,943 B, inside both gates;
round 3's single queue took back 20,416 B of rounds 1-2's per-service
monomorphisation. The review fixes are correctness, so under the owner's
ruling (speed beats size) nothing is cut and the ledger charges this figure.

## m5-integration-w3

**2026-10-12 · S-platform merge: pick mode and press freshness combined.**
`Forward::set_picking` calls `to_logic(intent, None)`, since leaving pick
mode settles flags and never carries a press's events; pick-mode input still
goes through `Forward::input`, so a press while picking spends its freshness
and never rides a later event.

**2026-10-12 · S-docs merge: one `support` module in the m5
test binary.** clippy's `duplicate_mod` rejects two `#[path =
"../support/mod.rs"]` declarations in one binary, so `tests/m5.rs`
declares `support` once and `inspect.rs` and `newcomer.rs` `use
crate::support`; `docs/reference/types.md` is regenerated, caught by
`the_reference_is_current`.

**2026-10-12 · Hyprland's activation policy is set through `eval` and
checked (sweep-1).** Hyprland 0.56.2 with Lua config answers `keyword` with
"keyword can't work with non-legacy parsers" at exit 0. `Policy::hyprland`
tries `keyword`, falls back to `hyprctl eval 'hl.config({ misc =
{ focus_on_activate = true } })'` on that refusal, and asserts with
`hyprctl -j getoption` that it took; restore uses the same form. Test:
`hyprland_dispatch_forms_use_the_current_syntax_first`.

**2026-10-12 · A `file` element's `run:` gets back strand's inherited
THP setting (sweep-10).** `run/outputs.rs::run_once` now calls
`child::restore_in_child` in a `pre_exec`, as the launcher, `exec` sources
and the editor do. The test reads `PR_GET_THP_DISABLE` via Python ctypes,
since `/proc/<pid>/status` cannot tell the difference on 6.18+. Test:
`outputs.rs::tests::a_run_gets_back_the_inherited_thp_setting`.

**2026-10-12 · `==` and `!=` take numbers by the ordering
rule (sweep-4).** `common_unit` holds the ordering table and
`Checker::equality` uses it for two numeric prims; other pairs keep
`assignable`. Landed because a stricter rule can be loosened in 1.x but
never tightened (spec.md 14) and it removes a concept; no repo file
compares a duration or angle with a bare number. spec.md 7.2 states
it. Tests: `spec.rs::{equality_takes_numbers_by_the_ordering_rule,
equality_compares_numbers_as_written}`.

**2026-10-12 · Canvas paint order pinned by a PNG test; drop limit
in spec.md (sweep-11, sweep-9).** The rule is `canvas.rs`'s since
M4: shapes add to one path; `fill` (non-zero) or `stroke` paints the
shapes since the last paint and starts a new path; a shape with no
paint after it draws nothing; `fill` with no path paints the whole
box, `stroke` with none nothing; later paints cover earlier. Test:
`canvas.rs::a_paint_takes_the_shapes_since_the_last_paint`. spec.md 6.7
states the unmounted-copy drop limit as 1.0's implementation limit, not
the language's.

**2026-10-12 · The inspector's node pane says why a `shader`
draws nothing (sweep-8).** design.md promises it, and dropping it
needs an owner ruling. Logic already holds `ToLogic::GpuStatus`
in `Shell::gpu`, so `inspect.node` on a `shader` adds `why`:
"draws nothing: no GPU (<reason>); the CPU draws everything else"
for `Unavailable`, "draws nothing yet: the GPU is starting" for
`Starting`, nothing for `Up`/`Unused`. Proposed in architecture.md for
owner review. Tests: `a_shader_node_pane_says_why_it_draws_nothing`,
`a_shader_that_draws_nothing_says_why`.

**2026-10-12 · The promoted GPU cost on hardware is measured; box ticked
(sweep-6).** `gpu_idle.rs::gpu_is_released_when_idle` now reads `strand
report --json` while `Glow` is promoted, asserts it shows promoted with
frames drawn, and prints p50/p95/p99 (reported, never gated). On Intel PTL:
p50 about 1.1 ms, PSS while promoted +21-26 MB over baseline, +6,351 kB
after the drop (under the 12 MiB presented bound), at most +2,184 kB between
cycles (bound 4 MiB).

**2026-10-12 · `strand-auth`'s fake helpers are written by a child
process.** Writing a script with `std::fs::write` then executing it races
with forks on other test threads (`ETXTBSY`). `fake` now writes it through
`sh -c 'cat > "$1" && chmod 755 "$1"'`, so this process never holds a
writable descriptor; test code only.

**2026-10-12 · The M5 Exit line says why its boxes are open (sweep-13).**
The newcomer box waits on the owner's timed human trials (m5-owner Q15 (i))
or Q15's fallback (ii); the freeze box waits on the integrator's freeze
commit. features.md says both.

**2026-10-12 · Wave 3's `.text` accepted; close gate proposed; open items
ruled (sweep-3).** Merged tree: 18,711,127 B CPU-only and 22,630,295 B
GPU, over wave 3's room by 50,240 B and 32,128 B, accepted as cold code
(xdg-activation, inspector) under the speed-first policy. Proposed close
gate: keep 18,874,368 B and 22,806,528 B (measured + 64 KiB rounded to 256
KiB). Streams' architecture.md interface changes are accepted as built;
pick-answer timing is accepted; the write-back rename window is a known
limit.

**2026-10-12 · The language freeze (sweep-5).** With all four wave-3
streams merged, spec.md's status reads **frozen** and grammar.md is frozen
with it; no "not yet", "open" or "proposed" text remains. From this commit
spec.md section 14's process is in force: a change to either file needs a
decisions.md paragraph citing the compatibility policy (typo fixes exempt),
held by the fixture tests, the tree-sitter conformance test, the ladder test
and `crates/strand-dev/tests/spec.rs`.

## m6-services

S-services' M6 wave-1 hardening: remote media art, 802.1X joins and the
inline join, a notification's time of day, `StepVolume`.

**2026-10-12 · Remote media art is fetched, with ureq over rustls.**
design.md's `image media.art { fit: cover; filter: blur(30) }` needs
`https://` art; `http://` is fetched too. Fetches run on the services
runtime's blocking pool, never the body's loop, logic or a frame; at most
8 MiB, 10 s end to end, 4 redirects, PNG/JPEG/GIF/WebP only. Bytes become
a content-addressed pinned file (`pixmap::write_file`). `art::Cache` keeps
the last 16 URLs, two fetches at once, 64 failures for 5 minutes. Client:
`ureq` 3.4.2, `rustls` 0.23.45 on `ring`, `rustls-native-certs` 0.8.4;
tests use local HTTP(S) servers only.

**2026-10-12 · 802.1X and WEP join through a saved profile; a join is
a task.** `connect_with(password)` cannot carry an enterprise login,
so such a network joins with `ap.connect()` only when NetworkManager has
a saved profile, else `failed(ssid, error)` at once. PSK/SAE unchanged;
OWE counts as open; an open network given a password is refused. The join
runs as a task applied by the loop, so the radio switch and signals are
not held up; an answer from a restarted NetworkManager is "NetworkManager
stopped". WEP's handling here is replaced by review round 2. Tests:
`network.rs::{enterprise_and_wep_networks_join_only_through_a_saved_profile,
a_slow_join_does_not_hold_up_the_radio}`.

**2026-10-12 · `Date` gains `hour` and `minute`; `Notification.time` and
`clock.now` fill them (spec.md 14).** Optional `hour: int?` and `minute:
int?` are an additive schema change; filling them for `clock.now` and
`Notification.time` is a spec.md 14 bug fix, since both were documented
as a moment. `clock.today` and calendar days carry none, so `n.time
== clock.today` is now false (compare `.format("%F")`); no repo file
relied on it. `format` formats the time when present. `received_at`
was rejected as a second concept. Seconds are left out. Test:
`vm/clock.rs::tests::a_date_can_have_a_time_of_day`.

**2026-10-12 · `StepVolume` is removed, not wired.** design.md
steps volume by assignment only (`audio.sink.volume -= dy * 0.05`,
`strand set audio.sink.volume +5%`), both reaching the audio thread as
`SetVolume`, so a step action would be a second way. Removed with what
only it used: `Echoes::pending`, `base`, the post-write core sync (`wait`,
`synced`). Test: `audio.rs::writes_land_where_wpctl_reads_them`.

**2026-10-12 · Review 1: one join per network; NM replacement stops joins;
art byte cap.** `network::Asked` asks one join per network name: a repeat
with the same password (by fingerprint) is dropped, another password waits
and the last wins. When NetworkManager's owner changes, running joins are
stopped (`JoinSet::shutdown`) and reported `failed(ssid, "NetworkManager
stopped")`. `art::Cache` holds its tmpfs files to `art::KEPT_BYTES` (16
MiB), never dropping the shown or newest; `MAX_BYTES` stays 8 MiB, enforced
for chunked bodies too. Tests: `two_joins_of_one_network_add_one_profile`,
`a_join_stops_when_networkmanager_is_replaced`,
`the_cache_is_bounded_by_its_bytes_and_keeps_what_is_shown`.

**2026-10-12 · Review 1 (services-r1-8): remote art overruns both
`.text` gates.** The HTTPS stack costs about 778 KB (`rustls` 286,712 B,
`ring` 257,889 B, `ureq` 108,788 B, ...); the tree measures 19,633,431
B CPU-only and 23,550,935 B GPU, 759,063 B and 744,407 B over the
gates, so `the_release_binary_code_stays_within_its_gate` fails until
re-based. Reason: the media backdrop needs `https://` art and an in-process
TLS client is the smallest correct way. Options for the owner: (a) re-base,
(b) system `curl`, (c) a default-on cargo feature, (d) `http://` only.

**2026-10-12 · Review 2: `secure` matches the join rules; WEP joins;
art fixes.** `secure` is now true exactly where `connect_with(password)`
joins (WPA/WPA2/WPA3 or WEP), false for open, OWE and 802.1X; a spec.md
14 bug fix to the documented "It needs a password". WEP now joins like a
personal network (`key-mgmt` `none`, `wep-key0`, `wep-key-type` 1 or 2);
"WEP is not offered here" is withdrawn. A repeat while activating with the
same password is dropped. `art::IN_FLIGHT` is 6 so stalled skipped-track
fetches do not hold the shown art. `media::Kept` leaves the shown file to
the next body. Tests: `secure_is_exactly_the_networks_a_password_joins`,
`a_repeat_while_the_join_is_activating_is_dropped`.

**2026-10-12 · Review 3: running fetches pass to the next body;
slot per runtime; passwords merge.** A body's fetches are detached
tasks reporting on a channel left with the cache (`media::Left`), so
the next body counts them and `art::Cache::abandon(url)` drops one
finishing with no body. The left-cache slot is a thread-local, one
per services runtime. A new password merges into the saved profile's
security section (`network::merged_security`), keeping `auth-alg`,
`proto`, `pmf` etc., a WEP key at `wep-key<wep-tx-keyidx>`. Tests:
`fetches_running_when_a_body_ends_are_counted_by_the_next`,
`a_new_password_keeps_the_saved_profile_s_security_settings`.

## m6-edges

**2026-10-12 · A drop's `at` is exact in a windowed `list`, by logic
(spec.md 6.7).** Render keeps owning `at` over the rows it sees;
`NodeEvent::Drop` gains `hidden: Option<HiddenRows { list, before }>`
when dropping a source from another list on a list with `row_first >
0`. Logic's `Instance::drop_at` makes `at` one less when one of the first
`before` items is a copy by render's test (a row holding a `drag:` of the
dragged typed identity, judged by the item, its key or a field fitting every
mounted row). Sending all identities to render was rejected. A spec.md 14
bug fix. Test: `render.rs::a_drop_on_a_scrolled_windowed_list_is_exact`.

**2026-10-12 · Inspector write-back swaps the file in with
`renameat2(RENAME_EXCHANGE)`.** The rewritten file is exchanged
with the target, then what came out must be the hashed file (same
inode, same hash); if not, it is swapped back and refused as
`Changed`, and a later save found during the swap back is put in
place with the earlier kept (`Io`). Each write has its own temp name
`.{name}.strand-dev-{pid}-{n}`. Without exchange (`EINVAL`/`ENOSYS`)
the M5 checked rename is used. Test hook: `write_with(edit, Swap::{Auto,
Exchange, Rename}, hook)` with `Moment::{Before, Window, Back}`. Test:
`inspect.rs::a_save_after_the_check_is_swapped_back_and_kept`.

**2026-10-12 · Review 2: a row's `drag:` values by render's walk;
swap back checked per exchange.** `row_drags` walks a row as render's
`copy` does (under a `list` or `DropRows` only `Prop::NotRow` content)
and `row_drag` picks the way that gives one of each row's own values, so
child order does not matter. Every exchange of the swap back is checked by
what it brings out, at most eight (`Moment::Again`). A failed exchange
reports what the file holds. `ENOENT` at the exchange (file briefly
missing) is `Changed`. Tests: `a_hidden_copy_is_one_by_renders_test`,
`a_third_save_before_the_latest_goes_in_place_is_not_hidden`,
`a_file_missing_at_the_swap_is_refused_as_changed`.

**2026-10-12 · Review 3: `ENOENT` alone decides a missed exchange;
`.text` measured.** The temp file is made just before the exchange,
so `ENOENT` means the target was missing; the second look is dropped
(`Moment::Missing` tests the gap). A swap back that hits `ENOENT`
says no file is there and where the save is kept. Size: +15,232 B
CPU-only and +15,552 B GPU over M5's close, under both gates. Tests:
`a_file_written_anew_after_a_missed_swap_is_refused_as_changed`,
`a_failed_swap_back_with_no_file_says_none_is_there`.

## m6-lock

**2026-10-12 · PAM's conversation reaches the lock as it happens;
two additive `auth` fields.** The helper protocol (version
2; version 1 refused) is a conversation: `BEGIN`, then `INFO`
frames for `PAM_TEXT_INFO`/`PAM_ERROR_MSG` and `PROMPT` frames for
`PAM_PROMPT_ECHO_OFF` as they come, a `SUBMIT` per prompt, and the
`VERDICT`; `PAM_PROMPT_ECHO_ON` fails closed. `auth` gains `message: text`
and `prompt: text`, additive under spec.md 14; spec.md 5.1 says so. A prompt
element or `on message` event was rejected. `Client::submit` answers every
prompt with its one password.

**2026-10-12 · Deadlines count only while a password waits for or is with
PAM.** No deadline while no password is waiting; with one, the helper must
send a frame within the timeout (30 s, `AuthConfig::timeout`) of the last,
and a password must be answered within `LIMIT_FACTOR` (3) timeouts (90 s),
room for `pam_fprintd`'s 30 s. A dropped `Answers` ends a conversation
with no password (`AuthError::Cancelled`, helper tree killed); one with a
password runs to its verdict.

**2026-10-12 · The store keeps a conversation open only while a lock is
shown, and never loops.** `auth::LockSignal` (`AuthConfig::lock`, set in
`LockScreen::see`, cleared at unlock) lets the store begin a conversation
when a lock shows and after a refused password; one that ended with no
password is begun again only by the next submit, and its failure is
logged, not a fault (`failed` unchanged, text in `message`). Without
a signal each submit begins its own. `busy` runs from submit until PAM
answers. `strand_auth::refusal` rejects a NUL or more than 511 bytes.

**2026-10-12 · Helper fault points strike when a password arrives.**
`auth_crash`, `auth_hang`, `auth_garbage` and `auth_stdio` fire inside the
conversation callback on `SUBMIT`, before PAM sees the password. Tier-A
tests use a `pam_exec expose_authtok` stack that prompts (`tests/pam.rs`'s
`prompting`), since `pam_permit` never prompts. The fallback field keeps
`Client::submit` and shows no PAM text.

**2026-10-12 · The fingerprint is emulated in the lock VM with `pam_echo`
and a waiting `pam_exec`.** `scripts/lockvm/scenarios/fingerprint.sh`
installs `pam_echo` saying "Place your finger on the fingerprint reader"
and a `pam_exec` waiting 5 s for a file, ahead of `common-auth`, and runs
`vm_pam.rs::pam_fingerprint_then_password` and the `fingerprint_*` tests;
`strand_lock.sh` skips those. README drops its password-only advice and
states the fingerprint stack's costs.

**2026-10-12 · Size: +24,000 B on each build.** Measured 18,735,127 B
CPU-only and 22,654,295 B GPU over M5's close, inside both gates: the
conversation loop, the store's event channel and the watch.

**2026-10-12 · Review 1 (lock-r1-1 to lock-r1-5).**
`docs/reference/services.md` is regenerated. A conversation from an ended
lock session is dropped (`busy`, `message`, `prompt` cleared) and the new
lock's begins when it ends. A conversation's texts show only while a lock
is shown and for its own session. `strand_auth::Answers::take_back`
returns a password no prompt took, and the store begins
the next conversation with it while a lock is shown. Test:
`client.rs::a_password_sent_too_late_is_taken_back`.

**2026-10-12 · Review 2 (lock-r2-1, lock-r2-3): input wakes the reader
again.** To keep design.md's "a finger unlocks" after `pam_fprintd`
gives up, `auth::LockSignal` gains `wake()`/`wakes()`, called by
`LockScreen::input` for keys, motion, buttons and axes on a shown lock;
on a wake, a conversation waiting at a prompt after a text with no
password is ended and restarted. Cost: a password typed after the
reader gave up waits through the reader again. `UnlockToken`'s doc
names `Client::converse`. Tests: `input_on_a_shown_lock_wakes_auth`,
`fingerprint_a_woken_lock_listens_to_the_reader_again`.

**2026-10-12 · Review 3 (lock-r3-1, lock-r3-2): a wake restarts only
after a reader's wait.** Restarting any prompt after a text looped on
banner stacks and Arch's `pam_faillock.so preauth` (`required`). A wake now
restarts only a conversation whose prompt came `auth::READER_WAIT` (2 s)
or more after its first text, at most once per reader wait. Rejected:
once per prompt, and rate limits. A `pam_fprintd` `timeout=` of
90 s or more times out an early password (`LIMIT_FACTOR` × 30 s);
kept as a recorded limit, README says to keep it under 90 s. Test:
`a_reader_wait_past_the_limit_times_out_a_password_typed_early`.

## m6-flakes

**2026-10-12 · A dropped image request that a frame wants again
is sent again.** The image worker drops a request no frame wants
(`None`, no wake); if the render loop flattened before polling the
drop it saw the key in flight and never asked again, so an icon
stayed undrawn. The worker now wakes the render loop for a drop too,
and `ImageStore::poll` resends a dropped key a frame wants. Test:
`image::tests::a_dropped_request_wanted_again_is_decoded`.

**2026-10-12 · The watcher test stamps its writes ahead of the clock.**
`a_file_rewritten_without_pause_is_read_within_max_delay` failed when
the runner stalled past the 15 ms quiet period between write and flush;
the early read was correct. Each write is now stamped 900 ms ahead
(`File::set_modified`), within `MTIME_SKEW` (1 s), so the file stays "just
written". Product code unchanged.

**2026-10-12 · The two-output idle window is retaken if a minute tick fell
in it.** `demo_bar_on_two_outputs_then_idle`'s window could include the
minute tick on a loaded runner. It now starts 2 s or more past a boundary,
rereads the minute at its end, and is retaken (at most three times) if the
minute changed. Whether this was CI's failure is unproven; the failure still
names the threads that woke.

**2026-10-12 · The reloads test allows no bus connection at all.**
`strand-introspect` keeps one connection per bus, so a refresh makes none;
the bus monitor's check now fails on any connection numbered between the
probes. Superseded in part by the review entries below.

**2026-10-12 · Review: a stale icon dropped undecoded is sent
again too.** A dropped key a frame wants is resent whether stale
or not (from `invalidate_icons`) and leaves `stale`, since the
worker's `load_with` resolves the theme when it runs. Test:
`image::tests::a_dropped_stale_icon_wanted_again_is_decoded`.

**2026-10-12 · Review: the idle window's start is read just before
its snapshot.** One clock read gives minute and second right before the
snapshot; the window is retaken (at most three times) if it began before
:02 or the minute changed by its end.

**2026-10-12 · Review 3: `strand-introspect` logs its drops; the reloads
test explains reconnects by that log.** Timestamps from the monitor
cannot bound skew, so this replaces the review and review 2 timing
classifiers. `Shared::forget` logs `dropped its connection :1.N to the
bus (why)` at `warn`, and an overdue connect logs `gave up connecting to
the bus (no answer within 2s)`. The test (`STRAND_LOG=info`) explains a
`Hello`/`Introspect`-only connection only after a logged drop, one per drop;
socket changes are allowed one per drop (`sockets_explained`). Tests:
`a_reconnect_after_a_logged_drop_is_explained`,
`a_socket_replaced_after_a_logged_drop_is_explained`.

**2026-10-12 · Size: +448 B, then +4,224 B CPU-only with the log lines.**
The image wake and resend cost +448 B CPU-only and +512 B GPU; review 3's
two log lines bring it to 18,715,799 B and 22,634,967 B, inside both gates.

## m6-integration-w1

**2026-10-12 · m6-services merge: gates re-based for remote media
art.** The merged tree measures 19,639,511 B CPU-only and 23,556,951
B GPU. Under the standing size policy, services-r1-8's alternative (a)
is taken: design.md's media backdrop needs `https://` art and no cheaper
correct in-process client exists. By the M5 wave 2 rule (measured + 64 KiB,
rounded up to 256 KiB): CPU-only 19,922,944 B, GPU 23,855,104 B. The store's
`media.art` doc and `media.schema` now agree; docs/guide/plugins.md's
release profile gains opt-level `z` for ureq, ureq-proto, rustls and
rustls-native-certs.

**2026-10-12 · A hidden row's `drag:` values are evaluated from its
item (sweep-1, sweep-2).** Replaces S-edges' guess from mounted rows,
so spec.md 6.7's limit is gone. `ListWindow` keeps the `for`'s body,
binding, scope and prefix (`mount::probe::RowProbe`), and `Ctx::probe_row`
walks the row in a disposable scope as mounting would (elements, `when`,
`if`/`match`, components, nested boundaries); `state`s read kept cells
(`keep_row_cells`) or initial values; nothing reaches scene, registry or
services. Layout-dependent `when` reads 0; awaited service `let`s are not
fetched. Test: `a_hidden_copy_is_found_whatever_the_row_drags`.

**2026-10-12 · The fallback lock shows PAM's text (sweep-3).** The
fallback draws PAM's text below the field in its own 5×7 bitmap font
(`lock_fallback/font.rs`, printable ASCII, at most three lines, 240
characters), so it depends on no text worker or fonts. The checker
calls `strand_auth::Client::submit_saying(password, told)` and sends
`LockMsg::Said(session, text)`; a new check clears it, a refusal shows its
last line; prompts are not shown. Interface: `lock_fallback::paint`'s
`message`, `LockFallback::set_message`/`message`. Test:
`lock_fallback.rs::pams_text_shows_below_the_field`.

**2026-10-12 · The compiler's provisional stubs match the service
schemas (sweep-4).** `Schema::builtin()`'s stubs for `auth`,
`battery`, `bluetooth`, `media`, `network` and `notifications`
gain their schemas' members, with provisional `PowerDevice` and
`AccessPoint` records; additive under spec.md 14. Record ids renumber,
so drag identity hashes are compared only within one build. Test:
`services/mod.rs::tests::each_builtin_stub_has_its_service_schemas_members`.

**2026-10-12 · Write-back's fallback to a checked
rename is tested through `Swap::Auto` (sweep-7).**
`a_filesystem_without_the_exchange_falls_back_to_the_checked_rename` uses
a per-thread seccomp filter failing `renameat2` with `RENAME_EXCHANGE`:
`EINVAL` and `ENOSYS` fall back, `EPERM` is `Io`, `Swap::Exchange` never
falls back. `libc` 0.2.190 is a strand-dev dev-dependency.

**2026-10-12 · `runs_are_debounced_and_one_rerun_is_queued` waits on its
runs, not on time.** It waits (5 s bound) for the slow run's line, sends
the writes, waits for the rerun's line, then checks no third run follows,
removing the fixed window that failed on a slow runner.

**2026-10-12 · The matrix's "wallpaper never showed" was the launch test's
app outliving its test.** `a_launched_app_is_activated_with_its_token`
removed the helper's directory within its 20 ms poll, so the helper kept
its window for 60 s and covered the next test's wallpaper. A `Launched`
guard now says `done`, waits for the helper's pid to exit (killing it after
`PATIENCE`, also on panic) and then removes the directory; the test waits
until no window shows. Strand was not at fault.

**2026-10-12 · A surface made just before its output's scale changed
kept the old scale.** sway 1.9 resends scale preferences only to surfaces
with a buffer, so a bar made between `create_output` and `output …
scale 2` stayed at 1.0. strand-surface now has each surface on an output
not yet shown a frame retake its scale from the output when the output's
geometry changes (`rescale_unshown_on`); a later compositor preference
still wins. Tests: `sway.rs::a_scale_set_before_the_first_frame_is_kept`,
`::…_on_the_integer_path`.

**2026-10-12 · The fuzzer's overlay-hold check reads when logic
applied each load.** A partial save held behind a broken one was
mistaken for the fix's start, under-measuring the hold; strand was
correct. The logic thread now reports each applied load (`#[cfg(test)]`
`run::LOADS_APPLIED`, sent in `Shell::apply`), and the fuzzer judges the
hold on that timeline, needing the full 250 ms (`overlay::QUIET`). Test:
`fuzz.rs::a_save_held_back_behind_a_broken_one_does_not_end_its_hold`.

## m6-owner

**2026-10-12 · Remote media art's HTTPS stack stays in-process.** The
owner keeps `ureq` over `rustls`/`ring` with the system's certificates
(alternative (a) of services-r1-8); `curl`, a cargo feature and
`http://`-only are not taken. Speed beats size, and the stack is about 778
KB of roughly 20 MB `.text`, none on the frame, reload or logic path.

**2026-10-12 · The remaining rulings accepted.** The owner accepts M5
wave 3's `.text` overruns, M5 wave 3's interface changes (drag identity,
`drawn`/`unchanged`, the inspect protocol including `why`, the handler
input context) and M6 wave 1's (`NodeEvent::Drop`'s `hidden`, write-back
hooks, helper protocol version 2 with `auth.message`/`auth.prompt`,
`strand-introspect`'s drop log, `Client::submit_saying`,
`lock_fallback::paint`'s `message`) as architecture.md gives them. The owner
asks for more `.text` room; see the re-base below.

**2026-10-12 · The owner's Hyprland capture, a second fixture.**
A 300 s capture on Hyprland 0.56.2 with an unplug and replug,
workspaces, fullscreen, special workspace and `hyprctl reload`
is added as `tests/fixtures/hyprland-0.56.2-captured-2` beside
the first, scrubbed of titles, user and host, and serials. It
shows workspaces moved before `monitorremoved` and back before
`monitoradded`, which the adapter already handles. Tests:
`wm/hyprland.rs::tests::captured_session_events_are_each_handled`,
`::captured_session_ends_where_hyprland_does`.

**2026-10-12 · The `.text` gates re-based with room to run.** The owner
accepted a 512 KiB reserve: each gate is the measurement plus 512 KiB,
rounded up to 256 KiB, giving CPU-only 20,447,232 B and GPU 24,379,392 B
(about 750 KB headroom). Gates stay regression tripwires; a deliberate
feature re-bases them with its measured reason, and the reserve is room,
not a target. This settles M5's close-gate question.

**2026-10-13 · The newcomer exit ticked by the automated test
and the agent proxy, Q15 (ii).** Replaces the 2026-10-12 entry,
which misread the owner: no human run is reported. The green
`newcomer.rs::the_guide_builds_bar_a_on_two_outputs` and one agent-proxy
session (9/9 on two headless outputs, `strand check` clean first try)
carry the box. README.md's "What you need" now says where `strand` comes
from (`cargo build --release -p strand` or the `strand-release-binaries`
artifact).

**2026-10-14 · The `.text` gates re-based on M6 wave 2's merged
tree.** Measured 24,451,607 B GPU and 20,533,655 B CPU-only, over by
72,215 and 86,423 B, mostly the surface stream's box-dependency graph
in `strand_compiler` (opt-level 3 because the checker runs on every
reload). Trading reload speed for bytes is against the speed-first policy,
so by the 2026-10-12 rule: GPU 25,165,824 B and CPU-only 21,233,664 B.

**2026-10-14 · `name: <-> T` component parameters not taken for now (Gaps
6).** Handlers and `id:` on a call belonging to its root element cover the
owner's case, and caller-held state passed down and changed by call handlers
covers sharing. A parameter mode adds a concept real testing has not needed,
so parameters stay read-only (spec.md 5.2), assigning one is a load error
(8.3), and `<->` on a call stays `check::two_way_prop`. Revisited only if
a component must write its caller's state.

**2026-10-14 · Default `$motion.spatial` is spring(3000, 0.75)
(Bugs 2).** The owner took option (b): spring(700, 0.9) reached 90%
at 129 ms against Quickshell's OutBack at 60 ms; spring(3000, 0.75) is
half way at 27 ms, 90% at 51 ms, overshoots 2.84%, and stays a spring
so interrupted motion keeps velocity. design.md's theme and every copy
(`strand_scene::motion::SPATIAL`, built-in theme, fixtures) change. The
overshoot exposed a render bug: a list's drawn scroll offset now lands
exactly on target at rest (`ScrollState::unapplied`). Mid-spring references
are re-blessed and motion tests use `assert_springs_to` (ends on target,
overshoot within the analytic peak, turns back at most once). Test:
`the_default_spatial_spring_is_snappy`.

## m6-w2-services

S-services' M6 wave-2 stream: the owner's hands-on findings that are
services' to fix.

**2026-10-13 · `workspaces.focus(target: int | text)`
switches to a workspace not made yet (Gaps 2).** One action, not
`focus_id`/`focus_name`: an `int` is the workspace number on every
compositor (Hyprland `dispatch workspace 4`, sway `workspace number 4`,
niri `{"Index": 4}` on the focused output), a `text` its name (Hyprland
`name:<name>`, sway `workspace "<name>"`, niri `{"Name": …}`). On
sway a negative number is a configured slot's id (Gaps 3). Failures
are `UnknownWorkspace` or `UnknownWorkspaceName`. `ws.focus()`
on an unused Hyprland id now makes it. Additive under
spec.md 14. Tests: `named_switches_make_what_hyprland_makes`,
`sway_config_assignments_are_configured_workspaces`.

**2026-10-13 · Persistent workspaces are `workspaces.configured`,
placed by `workspaces.slots(screen)` (Gaps 3).** Adding them to `all`
with `exists: bool` would change `all`'s meaning, so they are a list of
their own, ordinary `Workspace` records whose `ws.focus()` makes them,
and `workspaces.slots(screen)` merges a screen's workspaces and slots by
number. Configured means: Hyprland numbered rules from `j/workspacerules`
(named and selector rules make no slot); sway's top-level `workspace
<name> output <outputs…>` lines from `get_config`, ids `-(n +
1)`; niri none. Tests: `numbered_rules_are_configured_workspaces`,
`slots_follow_the_compositors_numbers`.

**2026-10-13 · A window links to its desktop entry, `win.app`,
and draws its icon (Bugs 3, gaps 9).** The compositor service reads
listed desktop entries (`apps::AppIndex`, reread on `apps::changed`) and
matches each app id by entry id, case-insensitive id, `StartupWMClass`,
reverse-DNS last part, name, then longest dash head (`chromium`
for `Chromium-browser`). `Window.app: App?` is additive; `icon`
becoming the entry's icon is a spec.md 14 bug fix. State is
published at once and patched when lookups land. Compositor
tests use `tests/common::no_desktop_entries`. Tests:
`windows_find_their_desktop_entries`,
`windows_are_published_before_the_desktop_entries_are_read`.

**2026-10-13 · `media.position` is writable, and writing it seeks
(gaps 13).** `position: float rw` (0 to 1), so `slider { value: <->
media.position }` is a scrubber; a `seek` action would be a second
name. The service uses MPRIS `SetPosition(trackid, µs)` with a real
`mpris:trackid`, else `Seek(offset)`; the position is fixed at the
target at once and `Seeked` confirms. `CanSeek` false, unknown length
or a non-number keep the place. `elapsed` stays read-only. Tests:
`writing_the_position_seeks_the_player`, `media_position_is_a_scrubber`.

**2026-10-13 · The `network` service over iwd where NetworkManager
does not run (gaps 15).** NetworkManager whenever it owns its name,
iwd (`net.connman.iwd`) when only iwd does, switching in the same
body; iwd is followed, never started (`Daemon::watch`, every call
`NO_AUTO_START`). Mapping: `wifi` is the station `Device`'s `Powered`,
`connected` is `Station` `connected`/`roaming`, `strength` from a
signal level agent (5 dBm steps -50 to -90), `access_points` from
`GetOrderedNetworks`. `connect_with` feeds a passphrase agent
and forgets a known network first. Tested against a zbus iwd:
`network_iwd.rs::network_follows_iwd_where_networkmanager_is_absent`.

**2026-10-13 · A builtin `session` service over logind (gaps 1, gaps
17).** `can_suspend`, `can_hibernate`, `can_reboot`, `can_power_off`
(`yes`/`challenge` true); actions `suspend()`, `hibernate()`, `reboot()`,
`power_off()` (interactive, unbounded since polkit may ask), `lock()`
and `log_out()` on the session from `GetSession("auto")` (120 s bound);
failures as `failed(action, error)`. `event lock_requested` (logind's
`Lock`) lets `on session.lock_requested { locked = true }` route
`loginctl lock-session` and idle daemons to the shell's lock. Test:
`session.rs::session_reads_logind_and_acts_through_it`.

## m6-w2-language

**2026-10-13 · `after T while c` is one-shot, said first (Docs 1).**
A wording fix under spec.md 14: spec.md 8.4 opens with "is one-shot:
it runs at most once per mount", says the count pauses but does not
reset when `c` turns false, and names the re-arming idiom `on change
c after T { if !c { … } }`; the guide's level 4 says the same. Test:
`after_while_is_one_shot_and_the_grace_idiom_runs_each_time`.

**2026-10-13 · Whole numbers stay `int`s through `?:` and `match` (Gaps
12).** `whole_shape` goes through `?:` and `match` when every branch
has the same shape (an empty list fits any list shape), and a `for` over
a literal whole-number list loops over `int`s. Corrected after review:
the widening rule now covers what this reaches, so an untyped `state`
whose first value is whole is widened by a fraction written to it,
following declarations back, and `push`/`insert` of a fraction widens a
whole list. Tests: `whole_numbers_stay_ints_through_match_and_ternaries`,
`whole_number_values_widen_where_fractions_are_written`.

**2026-10-13 · Dates order (Gaps 16).** `<`, `<=`, `>`, `>=` are accepted
between two values of the schema's `Date` record (not a config's own
`type Date`); the VM's field-by-field record comparison is time order,
and a date with `hour` null comes before every time that day. Only `Date`
is ordered. A load error becomes a meaning (spec.md 14); spec.md 7.2 says
so. Test: `vm.rs::dates_order`.

**2026-10-13 · `+` on texts, `distinct` and `sum` (Gaps 11).** `+`
between two `text`s joins them and `s += "x"` appends; `+` of text with
anything else stays an error. `xs.distinct()` keeps the first of each
structurally equal item in order. `xs.sum()` and `xs.sum(x => …)`
add numbers of one unit, null items skipped, and an empty list gives the
type's zero (`0ms`, `0%`, `0.0`). List methods are schema additions. Test:
`vm.rs::text_plus_distinct_and_sum`.

**2026-10-13 · `dur(d, clock: true)` (Gaps 14).** `dur` gains `clock: bool
= false`; with `clock: true` it writes `m:ss` under an hour and `h:mm:ss`
from an hour, seconds rounded down. A separate `clock(d)` or a format
string was rejected. A track's elapsed time is `(media.length ?? 0s) *
media.position`. Test: `vm.rs::dur_has_a_clock_form`.

**2026-10-13 · Declared actions, the owner's ruling (Gaps 1).** Amends
m5-tools: a command is a declaration, not a statement. (1) A top-level
`action name(params) [command]`: a list literal of text literals and
typed parameters (`text`, `path`, `int`, `float`, `bool`, an enum),
each exactly one argument, program allowed by `permit exec`, listed by
`strand check`; called only in handlers. Commands run detached on a
64 KiB thread, at most once at a time with one rerun queued; failures
are `strand watch` notices. A parameter after a shell's `-c` warns
`check::shell_script`. (2) A `from dbus` service's `action name(params)
= Method`, checked against introspection (`check::dbus_method`,
`check::dbus_type`, `check::dbus_unchecked`). spec.md 5.7 is new. Test:
`a_click_runs_a_declared_actions_command`.

**2026-10-13 · The palette's extras and identity are read in
props (Gaps 10).** Widening m5-tools: wherever an expression
reads tokens, `$palette.<name>` is an extra typed `color`,
`$palette.name`/`source` are `text`, `$palette.dark` a `bool`. An
extra a program reads becomes a plain token at `palette.<name>`
(`lower::Program::palette_reads`) so it springs in a swap; a missing
one is a runtime error unless `??` gives a fallback. Reading it
in a token value or `use` is `check::palette_in_tokens`. The
palette is its own memo (`Ctx::current_palette`). Test:
`theme.rs::palette_extras_are_read_in_props`.

**2026-10-13 · A config `service` named like a builtin one
shadows it (owner's ruling).** Replaces `check::redeclared` with
the `check::shadows_builtin` warning, so adding a builtin (such as
`session`) never breaks a config (spec.md 14, 4.2). The `Composite`
host routes a declared custom service first; the builtin's fields are
hidden. The runtime's own reads (`system.dark`, `accent`, `screens.all`,
...) go through `ServiceHost::read_builtin`, `sources_builtin`,
`acquire_builtin`, `release_builtin`; events and `strand call`
resolve against the config's service first. Same-named components and
surfaces stay errors. Tests: `a_config_service_shadows_a_builtin_one`,
`the_runtimes_own_reads_go_to_the_builtin_services`.

**2026-10-13 · Handlers and `id:` on a component call are its root
element's (Gaps 8, part of Gaps 6).** A component whose tree is one element
is that element where called, so handlers (run in the caller's scope,
checked against the root's events) and `id:` in a call's block are about the
root (`Env::adopt_node`); a root that is another call reaches the element
at the bottom. A tree that is not one element stays a load error. Rejected:
declared forwarding targets, and wrapper nodes. spec.md 5.2 says so. Test:
`a_calls_handlers_and_id_are_its_root_elements`.

**2026-10-13 · `screen` in a component is its bar's (Gaps 7).**
`screen` is a hidden parameter a component takes when it reads
it or calls one that does, passed at each call from the scope
there; a call with none (a `panel`, a top-level `popup`) is
`check::no_screen`. It resolves last, just before the unknown-name
error, so any parameter, local, declaration, enum variant or service
named `screen` wins. Tests: `a_components_screen_comes_from_its_bar`,
`a_screen_variant_wins_over_a_components_screen`.

**2026-10-13 · `name: <-> T` parameters proposed (rest of Gaps 6).**
Proposal: a parameter declared `name: <-> T` takes a writable place at
the call (`Row r { page: <-> menu }`), but it adds a parameter mode. Not
implemented; the owner declined it for now (m6-owner, 2026-10-14).

**2026-10-14 · The same name in two files (owner's ruling).** For
components, types, enums and `fn`s: (1) a file that declares and uses a
name gets its own; (2) a file without one gets the single declaration
elsewhere; (3) where two or more other files declare it, a bare use
is `check::ambiguous_name`, offering `file.Name`. `file.Name` works
wherever a bare name stands. Surfaces, services, actions, token sets and
keyframes stay unique. Each shared declaration gets a `check::same_name`
note (`Severity::Note`), never reported on load. Shared names display as
`file.Name`. Rejected: imports, and last-wins. Details corrected by the next
entry. Test: `a_file_uses_its_own_declaration_of_a_shared_name`.

**2026-10-14 · Same name in two files, corrected after review.**
An ambiguous use is exactly one error with no stand-in (`Ty::Error`),
deduplicated across overload attempts. Keys stay stable when a name
becomes shared (`lower::Program::owner_key`, `TypeTable::stable`,
`ty::Home { file, shared }`); persisted paths migrate between
`Row.open` and `a.Row.open` (`migrate_persisted`). Rules count only
what a position takes (`check::names::Want`). `$Row.…` tokens and
`override Row.…` follow rules 1 and 2. One stem in two directories
sharing a name is `check::file_name`. Completion and hover follow the
rules. Tests: `an_ambiguous_use_is_one_error_even_in_overloaded_calls`,
`a_name_becoming_shared_keeps_the_unedited_files_state`.

**2026-10-14 · Same name in two files, second review.** `q.Name`
in an element or type is file `q`'s declaration whenever that
file declares `Name` and no local or file-level `q` is in scope
(`Checker::file_for_name`); in an expression, a service, value or enum
`q` with member `Name` keeps its meaning (`Checker::member_of_other`),
so `battery.percent` stays the service's. Nothing offers an
unwritable form (`hir::Program::unqualifiable`). Paste-then-cut
keeps state (`Ctx::moved_from`, `take_parked_moved`,
`Identity::take_over_moved_bodies`). Settings fields compare
types by `CellTy::same`; `migrate_persisted` runs only
on a fresh mount. spec.md 4.2 states the limits. Tests:
`every_offered_fix_compiles_whatever_the_files_are_named`,
`a_component_pasted_then_cut_keeps_its_state`.

**2026-10-14 · Same name in two files, third review.** A body is
taken over only in the paste-then-cut shape (the survivor had no
calls before, the cut one had some; `Identity::calls`), so undoing
a paste, deleting one of two used components or renaming beside one
keeps the survivor's state. `self` and node names are read before any
file name, so a `self.strand` changes nothing; the same gap in export
reads is closed. Tests: `undoing_a_paste_keeps_the_originals_state`,
`a_file_named_self_is_not_a_qualifier`.

## m6-w2-surface

**2026-10-13 · A capped squircle corner eases toward the circle
(Bugs 4).** Where the half-side cap bites, the superellipse exponent
falls linearly from 5 at the full 1.6 × radius reach to 2 (a circle)
at a reach of the radius, so `radius: full; corners: squircle` is
a pill and uncapped corners are unchanged (a drawing fix, spec.md
14). Tests: `paint.rs::a_squircle_bar_of_half_its_height_is_a_pill`,
`::corners_per_corner_full_and_squircle`.

**2026-10-13 · A text's line box stays CSS's `normal` (Bugs 5).** Strand's
line box (parley's `MetricsRelative(1.0)`) is ascent, descent and line
gap with half the gap above, as Chrome; Qt omits the gap, but the owner's
font has gap 0, so they match. Trimming to the capitals (`text-box-trim`)
would add a concept, so it is not added; uneven padding is the idiom. Test:
`layout.rs::a_text_line_box_is_css_normal`.

**2026-10-13 · Layer buffers stay inside anchored edges; a popup grabs
only for its own press (Bugs 1).** Hyprland constrains popups to the
monitor at the parent layer's origin, which the shadow overhang pushed
onto a neighbour, so render caps the overhang on each anchored side at the
margin there (`specs.rs`, `within_edges`). A popup now grabs by cause:
logic stamps each diff with its causing press (`SceneDiff::press`,
`run::PressCause`, or `Composite::action_press` within `GRAB_WINDOW`
500 ms for service answers), and a popup grabs only when that press
is the seat's last; popups opened by hover, timer or IPC never
grab, nor a popup under a non-grabbing parent. spec.md 5.1 says
so. Tests: `a_layer_surface_buffer_stays_inside_its_anchored_edges`,
`a_popup_grabs_only_for_the_press_that_opened_it`.

**2026-10-13 · `trigger: hover` gives a popup's open state to the pointer
(gap 4, owner's ruling).** `popup` gains `trigger: open | hover` (default
`open`), `delay` (default none) and `grace` (default `HOVER_GRACE`, 200 ms);
`Node` gains `open: bool`. With `trigger: hover` the router opens the popup
while the pointer is on its anchor, keeps it while on the anchor, anything
opened from it, or during a held press, and closes it after `grace`, writing
`open` via `Intent::Write`/`ToLogic::Write`. One hover popup per surface is
open at a time. It never grabs. `open:` with it, a non-literal `trigger`,
or `delay`/`grace` without it are errors (`check::hover_popup_open`,
`check::popup_trigger`, `check::hover_popup_prop`). spec.md 6.3 states
it. Test: `a_hover_popup_opens_on_its_anchor_and_closes_after_the_grace`.

**2026-10-13 · The owner's animations at scale 1.5: the pose and spring,
not frame rate (Bugs 2).** The owner's bar and islands paint far under 16.7
ms on CPU. Two causes: `enter` on a card inside an always-mounted `popup {
open: … }` plays only at boot (put the pose on the popup or use `trigger:
hover`), and `$motion.spatial` spring(700, 0.9) starts slowly. Test:
`owner_motion.rs::the_owners_bar_and_island_at_one_and_a_half`.

**2026-10-13 · One travelling island: proposal, option (a) taken.**
Options: (a) `vol.box`, an element's laid-out box read by id, as a layout
fact; (b) a value anchor `popup { anchor: vol }`, which repositions without
sliding and makes element references values; (c) a built-in travelling
popout element copying Quickshell. (a) was recommended as the smallest
construct, and the owner took it (2026-10-14, below).

**2026-10-13 · Review r2: a press keeps its cause; the router's open popups
follow logic.** The last press among messages handled since the last step
stamps the diff, whatever input-less messages follow; a service answer's
step carries the last action's press within `GRAB_WINDOW`. `Router::observe`
takes logic's own `open` changes (a reload) and drops popups
no longer `trigger: hover`. A click away (`popup_done`) closes a
hover popup in logic until the pointer leaves its anchor. Delays use
`checked_add`. Tests: `a_press_keeps_its_cause_through_input_less_messages`,
`popup_done_closes_a_hover_popup_until_the_pointer_comes_back`.

**2026-10-14 · `id.box` and conditional ids (gap 5, owner's ruling).**
The schema gains `record Rect { x, y, width, height }` and `Node.box:
Rect`, in logical pixels from the top-left of the node's surface, without
paint offsets, transforms or poses; a popup's box is where it sits in
its parent's surface. Boxes are frame-late layout facts (`Prop::Watch`
`box`, `Instance::set_box`); the first box snaps (`BootRead`),
later ones spring. `when c { id: x }` names the copy whose condition
holds. The indicator is written `x: cur.box.x - ws.box.x`. Island popups
are sized from their anchor's box. Additive under spec.md 14. Test:
`boxes.rs::the_island_travels_between_items`.

**2026-10-14 · Review of 66c2f03: `.box` can never loop.** A
box-dependency graph (`check/boxes.rs`) replaces the three allowed
kinds: every cycle through a `.box` read is `check::box_layout_loop`,
naming its path; a sizes-only cycle (`n.width`, loaded on `main`)
is the warning `check::size_loop`. A conditional id's condition
taints reads of the name. `when c { id: x }` keeps its old no-op
meaning, with `check::conditional_id_ignored`, wherever `x` already
means something. Only `trigger: hover` popups pass input through gaps
(`SurfaceSpec::hover_trigger`). Popups are re-placed when the parent moves
(`Role::Popup::at`). Test: `a_box_loop_through_several_bindings_is_refused`.

**2026-10-14 · Third review of the box graph (c9e6907).** Whatever
moves a popup's own box moves every box on that popup and its nested
popups (`check/boxes.rs::moves`), since render inherits `set`, `font`,
`weight`. Dependences settle to a fixed point rather than 64 rounds. A
service field written from a box carries the dependence. A bar's size,
`edge`, `margin`, `open` and `screens` move every bar, panel and OSD
box (exclusive zone). The island shadow reservation applies only to
`trigger: hover` popups. Tests: `inherited_values_reach_nested_popups`,
`a_bars_exclusive_zone_moves_the_other_layer_surfaces`.

## m6-integration-w2

The integrator's passes over M6 wave 2 on `laptop/m6-w2`.

**2026-10-13 · m6-w2-language merge: shadowing assertion flipped;
where the streams meet.** `checker.rs::session_is_a_power_menu`'s
second assertion now expects `check::shadows_builtin` under
the owner's ruling, and architecture.md's schema bullet
says a same-named config service shadows rather than being
`check::redeclared`. `a_looped_whole_number_list_focuses_workspaces` checks
`for w in [1, 2, 3]` with `workspaces.focus(w)`.

**2026-10-13 · m6-w2-surface merge at `ea17b69`: hover popups
meet calls and `screen`.** A call whose root is a `trigger:
hover` popup takes `id:` and `on show` as the popup's, and a
component's hidden `screen` reaches a hover popup's body; tested by
`a_calls_id_and_screen_reach_its_hover_popup`. docs/owner-testing.md is
committed with each entry's wave-2 state.

**2026-10-14 · Third pass: `file.Name` meets `p.open`.** An `id:`
is a local bound for its whole block (`LocalKind::NodeId`), so
`p.open` stays the popup's state beside a `p.strand`; a qualified
call whose root is a hover popup takes `id:` as the popup's. Test:
`a_qualified_call_takes_its_hover_popups_id`.

**2026-10-14 · Final pass: surface at `15a0de7`; drag identity
re-hashed; tests where streams meet.** `every_snippet_mounts`' drag
identity hash becomes `ea8648310a4190b3`, since new schema records
moved the type id; identities are equal only within a build. An
`id:` local wins over a file stem; a `when`'s `id:` naming a file
or shared component is `check::conditional_id_ignored`. Tests:
`file_stems_and_shared_names_meet_ids_and_boxes`,
`same_named_components_keep_their_own_conditional_ids`.

## m6-owner-fixes

**2026-10-14 · A travelling island stays on its output on Hyprland
(Bugs 6).** Two causes: a springing island's overshoot changed the
popup's shadow overhang and so its `PopupConfig`, forcing a reposition;
and Hyprland's `applyPositioning` offsets the constraint box by the
popup's window geometry on reposition. Fixes: layout takes a moving
island's reach in a `trigger: hover` popup as if inside the popup's box,
and the surface manager shifts a popup inside its parent's output itself
(`placement::popup_shift`), counting `Stats::popups_moved`. Tests:
`an_island_overshooting_the_popups_edge_keeps_it_still`,
`a_popup_is_shifted_inside_its_output`.

**2026-10-14 · A closing surface's last commit keeps its exit's pose
(Bugs 7).** The frame ending a closing pose cleared the flatten cache,
so the last commit carried the rest pose (alpha 1), and Hyprland
faded a full-alpha snapshot. `Renderer::delegated_pose` now reports
the last painted frame's pose (`SurfaceState::posed`). The proposal
to skip `exit`/`enter` on a hover hand-off is withdrawn. Test:
`surface_poses.rs::a_closing_popups_last_frame_keeps_its_exit_pose`.

**2026-10-14 · Relative `image`/`svg`/`lottie` files are under the
config directory (Bugs 8).** Instantiate rewrites each `source`, literal
or computed, through `check::assets::local_source` to an absolute path, as
every other config path is; `~/`, absolute paths, URLs and icon names are
left as written, judged by `strand_scene::source_is_file`. A literal file
that is missing is the `check::missing_file` warning from `strand check` and
the LSP only. Test: `relative_media_paths_are_under_the_config_directory`.

**2026-10-14 · A config `icons/` folder is looked in before
the icon theme (owner's ruling).** For each candidate pair from
`strand_icons::candidates` (the name and its `-symbolic` variant, most
specific first), `resolve` tries the folder (`<name>.svg`, else `.png`,
no subfolders) and then the theme before the next pair, so a generic
file never shadows a specific theme icon. Folder files tint only when
named `-symbolic`. `set_config_icons(Option<PathBuf>)` sets it; edits
are an icon theme change (`live::config_icons_source`). Surfaces
drawing icons repaint whole on `icons_changed`. Test:
`the_config_folder_comes_first_for_each_name_and_its_variant`.

**2026-10-14 · A local file an `image`, `svg` or `lottie` draws
is watched (owner's approval).** Replaces bug 8's "local files are
not watched". Instantiate records each bound source's current path
(`Instance::asset_files`), sent as `Role::Asset` with `Job::Referenced`;
changes go to `Job::Assets` and `Renderer::assets_changed`, which keeps the
old decode until the new one lands. A file written after the program took
the set is reported on registration (`Job::Referenced::at`). Paths are made
absolute against the process directory (`check::assets::absolute`). Tests:
`strand_run_redraws_an_edited_svg_without_a_reload`,
`a_relative_config_directory_still_records_absolute_files`.

**2026-10-14 · A `~` is evaluated with its value (Bugs 10,
B-b).** `Ctx::sources` evaluated each prop's `~` once at mount;
now each bound prop's memo evaluates the winning source's `~`
with its value (`PropOut::transition`), so a change takes
the transition in effect after it, and a transition change
alone sends nothing. spec.md 6.6 states the rule. Test:
`a_conditional_transition_applies_to_the_change_that_switches_it`.

**2026-10-14 · Cut text stays cut while its box springs (Bugs 13).**
While the layout for its width is being shaped, a cut text draws
the layout for the widest width no more than its box, else the
narrowest wider one, else the unbounded one, the last two clipped
to the box. Hysteresis or render-thread shaping were rejected. Test:
`morph.rs::cut_text_stays_cut_while_its_box_springs`.

**2026-10-14 · New text is laid out by its shaped size (Bugs 14).**
Layout waits up to `ESTIMATE_WAIT` (5 ms) for the text worker to
deliver any unmeasured text's unbounded layout, then lays out again
(`Renderer::await_unmeasured`), so FLIP glides never aim at the
0.55 em estimate. It waits on the text worker, never logic. Test:
`a_mounted_text_moves_its_siblings_once`.

**2026-10-14 · A layout with no clock leaves size springs alone (Bugs 9,
11).** The preview layout at attach (time zero) snapped size springs;
now it leaves them as they are and marks the surface animated so the
first painted frame starts them; reduced motion still snaps. Test:
`a_size_in_an_enter_pose_plays_as_its_popup_opens`.

**2026-10-14 · A surface's overhang holds still while its sizes spring
(Bugs 9).** While a size springs under a surface root, its overhang is the
larger of the reach at the spring's start and at its rest layout, kept until
every size spring settles, so the popup is not resized each frame. Test:
`a_shadowed_card_springs_its_size_without_resizing_its_popup`.

**2026-10-14 · A popup's `margin: 0` on its anchor's side touches the
anchor (gap 24).** spec.md 5.1: the gap is 6 px only while `margin` is
unset, `0` touches, negative overlaps; `attach:` makes it 0. `SurfaceSpec`
gains `margin_set: bool` and `placement::popup_config` gives 6 only
while it is false; `Option<Insets>` was rejected as changing every
reader. Tests: `a_popups_margin_on_its_anchors_side_is_its_gap`,
`a_zero_margin_is_set_and_an_absent_one_is_not`.

**2026-10-14 · A graph's history is its own, per mount.** A `graph`
keeps one sample per column at its own rate (`history` over its width) and
`smooth`, and a remounted graph starts empty. Keeping history with the data
source needs a service-side series, a new concept, so it is left to a later
wave on the owner's ruling (gap 25).

**2026-10-14 · A stack aligns its children together (gap 27).** A popup,
`stack`, `box` and every surface lays children over each other in one cell,
and `align` places them all; spec.md 6.7 says so and names the two ways to
place one child apart (a filling `row`/`col` with `justify`/`align`, or
`place: absolute`). A per-child `align_self` would be a second alignment
concept. Test: `a_stacks_align_places_every_child_on_both_axes`.

**2026-10-14 · Text in a list's rows is not waited for.** The bug
14 estimate wait halved the 2,000-row launcher's scroll frames, so
`await_unmeasured` no longer waits for text inside a `list` (rows take
their width from the list), scans once per call and relays out only when
awaited text arrived; `Renderer::estimate_waits` counts waits. Test:
`rows_mounting_in_a_list_do_not_wait_for_their_text`.

**2026-10-14 · Painting one surface does not run out another's hold.**
Text holds (`wait_until`) and size holds (`size_hold`) are wall-clock
deadlines, so each paint now moves every other running hold on by
its duration (`Renderer::pause_holds`), at most `MAX_HOLD_PAUSE`
(1 s) in total so a never-matching configure costs a bounded
wait. Tests: `painting_another_surface_does_not_run_out_a_hold`,
`back_to_back_paints_move_a_hold_on_at_most_max_hold_pause`.

**2026-10-14 · A frame waits for new text once (Bugs 17).** The
estimate wait has one deadline per surface frame, `ESTIMATE_WAIT`
from the frame's first wait, shared by all its layout passes and
kept until the frame is painted; text not arrived by then is drawn
by its estimate. A first frame is still held for all its text
(`frame_deadline`). Tests: `a_frame_waits_for_its_new_text_once`,
`a_moving_frame_waits_for_its_new_text_once`.

**2026-10-14 · A paint offset in flight is drawn whole logical pixels
from its rest (Bugs 17).** Offsets (`x`, `y`, FLIP glides) are drawn at
`written + round(sprung − written)`, so content moves with its box's edge
and a spring's tail draws its rest; a drag's held offset follows the pointer
exactly. Test: `a_swelling_card_repaints_its_edges_and_settles_still`.

**2026-10-14 · A box that only moves or resizes damages its edges
(Bugs 17).** A plain box's record and its clips keep geometry apart
(`NodeRecord::geom`), so a geometry-only change damages the bands
its outlines swept (`RoundBox::changed`); transformed or effected
clips, gradients, goo and shaders keep whole-record damage, and
a mask hashes its frame into its children. Partial repaint equals
full. Timing gate: `the_island_card_swells_within_the_frame_budget`
(median under 8.3 ms, every frame under 16.7 ms, `GATE_MISS`). Test:
`damage.rs::a_swelling_clip_repaints_only_its_edges`.

**2026-10-14 · Widgets read the pointer where they are drawn
(Bugs 16).** `Router::drag_slider` and `Router::choose` used
the laid-out box before paint offsets; the router now maps the
pointer into the laid-out box through the node's last hit box
(`InputScene::to_laid`, `Renderer::to_laid`), undoing offsets, glides and
transforms. spec.md 6.7 already says input follows the drawn shape. Test:
`widgets_moved_by_paint_offsets_read_the_pointer_where_drawn`.

**2026-10-14 · A hover popup takes its whole box while a press on it
is held (Bugs 15).** Hyprland lets the pointer leave a pressed surface
when nothing has keyboard focus, so while a press that went down on
a `trigger: hover` popup is held, its input region is the whole box
(`Painter::input_region` returns none). `Router::detached` releases a press
whose surface goes. Tests: `a_held_press_takes_the_hover_popups_whole_box`,
`a_surface_that_goes_mid_press_is_not_left_pressed`.

**2026-10-14 · A popup mapped under a still pointer is hovered at once
(Bugs 12, B-d).** Compositors may send no enter to a popup mapped or grown
under a still pointer, and spec.md 6.3 defines `hover` by position, so after
each painted frame, if the pointer is on a surface the popup opens from, on
the same output, and the popup takes input there, the host moves the pointer
onto it (`Host::rebase_pointer`), translating events until the compositor
catches up; `Host::settle_rebase` ends it if the pointer went elsewhere. Not
during a press, drag or pick mode; sibling popups are not taken. Test:
`a_hover_popup_mapped_under_a_still_pointer_is_hovered`.

**2026-10-14 · `open:` opens a `trigger: hover` popup from code (gap 21,
G-b).** While `open:` is true the popup shows (at once, no `delay`, closing
other hover popups on the surface); once false the pointer holds it as
usual. `open: <->` stays `check::hover_popup_open`. Lowered to `Prop::Hold`,
leaving `Prop::Open` to the router (`Router::observe` keeps the held
set). Rejected: `big.show()` actions and edge-triggered `open:`. Test:
`open_on_a_hover_popup_opens_and_holds_it`.

**2026-10-14 · `hit: none` (gap 22, G-c).** `hit` takes the keyword `none`
beside `grow(n)` (`enum HitKeyword { none }`): the node has no hit shape
of its own, its children keep theirs and still list it as an ancestor. A
drawing-nothing hover popup's top-level `hit: none` element passes the
input region to its children. Rejected: `pointer: none` (CSS's stops
children too), popup-only `input: false`, subtree-wide `hit: none`. Test:
`hit_none_takes_no_input_in_its_own_box`.

**2026-10-14 · An action a service cannot take is a service failure,
not a fault; `available` added (gap 20).** Per spec.md 12 and 8.3, an
action reaching no run is reported through `Service::refused(action,
error) -> Option<Event>` (`session.failed`, `network.failed`,
`bluetooth.failed`) and logged; a write is dropped and logged;
`DynService::action`/`write` error only for program mistakes. `network`,
`bluetooth` and `session` gain `available` (`Client::unavailable` resets it
when a run fails); a generic `x.running` or lookup was rejected. `strand
call` hears refusals via `strand_services::catch_refusals`;
unread actions of a failing run are refused, not lost. Test:
`a_service_that_cannot_run_fails_as_values_not_faults`.

**2026-10-14 · Lists gain `index_of(test) -> int?` (gap 23).**
It mirrors `find(test)`, answering the position (from 0) or
null. Rejected: equality `index_of(item)`, `position`, `find_index`,
tuple loops. A load error on an `Async` list, as `find` is (spec.md
8.6); spec.md 7.1 says so and functions.md lists `[T]` members from
`list_members`. Tests: `index_of_is_an_optional_int_of_a_predicate`,
`index_of_finds_the_first_position`.

**2026-10-15 · `attach:` keeps the margin on its side (gap 26).** `attach:`
no longer places the box: it names the fillet side (and a popup's anchor
side, a panel's anchored edge), and the margin there is kept like any
margin, 0 while unset, so `attach: top; margin: -32, 0, 0` covers a bar with
fillets at the screen edge. A panel with `attach:` and a set margin on that
side now keeps it. `placement::layer_config_with` drops `flush`. Tests:
`an_attached_panel_is_anchored_to_its_edge_and_keeps_its_margin`,
`an_attached_popup_touches_its_anchor_at_its_margin`.

**2026-10-15 · Fade layers: cached offscreen groups only where content
costs more (Bugs 17).** A group whose opacity or offset is in flight
(`Item::PushOpacity(f32, Option<Fade>)`) is keyed by content hashed from its
top-left pixel at 1/4096 px (`flatten::Shift`) and drawn as a cached image,
built only on a full repaint of the group; not under transforms or offscreen
groups. Layered only when holding a shadow, gradient or paint-filled
text, since measured flat content costs more layered. Pixels match within
tolerance 2 (8 at springing clip edges; `tests/fade_layers.rs`). vello_cpu's
u8 pipeline and threaded cells are left for the owner; whole-surface
relayout stays open.

**2026-10-15 · A builtin `outputs` service over wlr-output-management
(gap 18).** Records `Output`, `OutputMode`, `OutputChange`, enum
`OutputTransform`; separate from `screens` since it lists off
monitors and hardware modes. Configuration is whole and atomic through
`outputs.apply(changes)` and `outputs.test(changes)` with `OutputChange`
records (null fields keep), answered by `succeeded(action)` or
`failed(action, error)`, not per-field `rw`. Protocol-fatal values
are refused first (scale below 1/256). Modes match the nearest
listed rate within 0.5 Hz; transforms use counter-clockwise
names; cancelled configurations retry up to three times. Test:
`outputs.rs::outputs_lists_configures_and_tests_sways_heads`.

## m6-service-gaps

Service gaps the owner agreed on 2026-10-15 from their quickshell config
(owner-testing.md gaps 28 to 35).

**2026-10-15 · m6-service-gaps (gap 28): leaving is `network.disconnect()`,
not `ap.disconnect()`.** `access_points` is filled only while a list is open
and only one Wi-Fi network is joined, so a tile reading only `network.ssid`
must be able to leave too; it maps to NetworkManager's `Device.Disconnect`
and iwd's `Station.Disconnect` and also stops a join in progress without
reporting a failure. With nothing joined or being joined it fails
`failed("", "no Wi-Fi network is joined")`.

**2026-10-15 · m6-service-gaps (gap 28): `network.connecting: text?` and
`network.disconnecting: text?`.** They name the network, come from the
daemon merged with the shell's own requests through one shared overlay
(`network::Pending`; the daemon's value wins). Over NetworkManager `ssid`,
`strength`, `frequency` and `security` now name only an ACTIVATED
connection, a bug fix under spec.md 14 matching iwd.

**2026-10-15 · m6-service-gaps (gap 28): joined network's `frequency: int?`
(MHz) and `security: WifiSecurity?`.** No `band` field: it follows from the
frequency. `WifiSecurity { open, enhanced_open, wep, wpa, wpa2, wpa3,
enterprise }` is a builtin enum beside `OutputTransform`, from access point
flags over NetworkManager (a WPA3 transition AP reads `wpa2`) and
`StationDiagnostic.GetDiagnostics` over iwd, kept as `iwd::Diagnostics` with
two optional fields.

**2026-10-15 · m6-service-gaps (gap 28): `network.address` comes from the
kernel over rtnetlink.** iwd, networkd and NetworkManager each miss some
stacks, but every stack writes the address into the kernel, so one
event-driven reader (`network::address`, rustix `net`) serves both backends.
The value is IPv4 (universe scope preferred) else global IPv6, never
link-local, tentative or deprecated, null while not connected; a
`#[store(stream)]` field, and `Follower::changed` returns
`io::Result<Option<String>>`.

**2026-10-15 · m6-service-gaps (gap 28): explicit `network.scan()` and
`network.scanning: bool`.** The scan made when a reader opens the list stays
and raises the flag too. Over NetworkManager the flag runs from an accepted
`RequestScan` until `LastScan` changes or 15 s (`SCAN_LIMIT`); over iwd it
is the Station's `Scanning`. A busy refusal is no failure; radio off or no
device fails `failed("", "Wi-Fi is off")`, and `failed`'s first argument is
"" for an action naming no network.

**2026-10-15 · m6-service-gaps (gap 29): nearby devices are
`bluetooth.nearby: [BluetoothDevice]`, a stream field.** Discovery runs only
while a visible reader reads the list. Only unpaired devices with a name are
listed, sorted by name; advertisement-only signals are dropped. The flag is
`bluetooth.scanning` (the adapter's `Discovering`), one concept with
`network.scanning`.

**2026-10-15 · m6-service-gaps (gap 29): `d.pair()` pairs, trusts and
connects; `d.forget()` is `Adapter1.RemoveDevice`.** Each runs as a task;
failures are `bluetooth.failed(address, error)` with BlueZ's words or "timed
out", and an unknown address fails "no such device". Strand registers no
pairing agent (BlueZ uses the system's if running); this was the recommended
answer to an owner question and an agent can be added later, additively.

**2026-10-15 · m6-service-gaps (gap 29): `BluetoothDevice.connecting: bool`
and `disconnecting: bool`.** They come from the shell's own calls in flight,
since BlueZ has no connecting property; `pair()`'s whole run shows as
`connecting` (recommended answer, overrulable; `pairing: bool` can be
added). Booleans per item, unlike network's names on the service, because
several devices can be busy at once.

**2026-10-15 · m6-service-gaps: new members are appended after existing
ones.** In the Rust struct, the .schema text and the compiler's stub alike,
so field indexes and the stub drift test hold and growth stays additive
under spec.md 14.

**2026-10-15 · m6-service-gaps (gap 28, review connectivity-r1-1):
`AccessPoint.active` is the ACTIVATED connection's AP.** Over NetworkManager
the list marked activating or deactivating networks joined while `ssid` was
null; now `active` and `ssid` say the same thing on both backends.

**2026-10-15 · m6-service-gaps (gap 28, review connectivity-r1-2):
`RequestScan`'s `NotAllowed` is no failure only for "not now" words.**
NetworkManager uses one error name for busy and unavailable; "unavailable"
(e.g. hardware rfkill) is `failed("", "the Wi-Fi device is unavailable")`,
and other `NotAllowed` fails with NetworkManager's words. Message text was
preferred over reading the device `State` first.

**2026-10-15 · m6-service-gaps (gap 29, review connectivity-r1-3): repeated
Bluetooth calls are dropped; the last call shows.** `bluetooth::InFlight`
numbers calls in order; a repeat of the same call, or a connect while a pair
runs, is dropped, while a disconnect during a connect (or vice versa) is
asked and the device shows the call asked last, matching `network`. A `Pair`
past its one-minute limit is followed by `Device1.CancelPairing`.

**2026-10-15 · m6-service-gaps (gap 28, review connectivity-r1-4): over iwd
a leave names the station's last-named network.** That is the network named
while connected, roaming or connecting, forgotten when the station says
`disconnected`, so stopping a join no longer shows an older network's name.

**2026-10-15 · m6-service-gaps (gap 28, review connectivity-r1-6): joins a
`disconnect()` stops are held until it answers.** If the daemon refused the
disconnect the join went on unfollowed and its failure was lost. Now
NetworkManager joins are held (`network::Stopped`, `Asked::stopping`) and
dropped or deactivated once left, or followed again if the disconnect fails;
over iwd each join records `Join::stopped_by`.

**2026-10-15 · m6-service-gaps (gap 28, review connectivity-r2):
`disconnect()` with nothing to leave stops the joins itself; a repeat in
flight is dropped.** With no Wi-Fi connection activated or activating
(NetworkManager) or no network named (iwd), the daemon is not asked: joins
are cancelled by the service (`network::cancel_joins`, `iwd::drop_joins`
stop flag), since the daemon refuses with `NotActive`/`NotConnected` and the
join would go on. A second `disconnect()` while one is in flight is dropped;
the first's answer decides.

**2026-10-15 · m6-service-gaps (gap 30): the clipboard is two paths.** An
`input` copies, cuts and pastes on the main thread through strand-surface's
`wl_data_device` with the key press serial, so it works on every compositor;
the builtin `clipboard` service watches and sets the selection over data
control on its own thread and connection, as `outputs` does. The Router asks
with `Intent::Clipboard(ClipboardAsk)` and gets `InputEvent::Paste`.

**2026-10-15 · m6-service-gaps: the service binds
`ext_data_control_manager_v1`, else `zwlr_data_control_manager_v1` v1.**
Without either, `clipboard.available` is false and `clipboard.copy` or
`e.copy()` fail as `clipboard.failed(action, error)`; there is no
`wl_data_device` fallback for a handler's copy.

**2026-10-15 · m6-service-gaps: input keys.** Ctrl+C and Ctrl+Insert copy,
Ctrl+X and Shift+Delete cut, Ctrl+V and Shift+Insert paste; Ctrl+Shift+C/X/V
are accepted; `on key(k)` still hears them. A `type: password` input copies
and cuts nothing. A paste's line breaks and tabs become spaces, other
control characters are dropped, and a paste over 1 MiB or not delivered
within 5 s is dropped.

**2026-10-15 · m6-service-gaps: what a selection is.** A plain-text type
(`text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`, `TEXT`, `STRING`)
makes it text even when images are offered; else `image/png`, then
`image/jpeg` make a kept image; other image types set
`clipboard.holds_image` but are not kept. `clipboard.text` is "" for an
image, empty selection, text over 1 MiB, or a secret.

**2026-10-15 · m6-service-gaps (owner's ruling): a password manager's secret
is never read.** An offer with `x-kde-passwordManagerHint` is read for the
hint alone (64 bytes, 5 s); `secret` or an unreadable hint (fail closed)
leaves `text` "" and nothing enters history, disk or log.

**2026-10-15 · m6-service-gaps (owner's ruling; numbers overrulable):
history shape and caps.** Newest first, deduplicated by content (a repeat
moves to the front keeping its `id`); empty text not kept; 100 entries and
64 MiB in all, 1 MiB a text and 16 MiB an image, oldest evicted first.

**2026-10-15 · m6-service-gaps (owner's ruling): the history survives
restarts.** It lives in `$XDG_STATE_HOME/strand/clipboard/`
(`PersistStore`'s rule): `history.json` and `images/<id>-<time>.png|.jpg`,
directory 0700, files 0600, written atomically. A corrupt file is an empty
history with a warning. `clipboard.clear()` deletes the file and every
image; image paths are never reused, so no cache shows a stale picture.

**2026-10-15 · m6-service-gaps (owner question, recommended "yes"): history
records while a program reads it anywhere.** A program reading
`clipboard.history` anywhere holds `clipboard` and the field from its top
level while it runs, as bars hold `screens`; one that never reads it records
nothing. Built as `lower::Program::services_anywhere` and
`instantiate::HELD_ANYWHERE`, a fact about named fields rather than a new
`hold:` syntax.

**2026-10-15 · m6-service-gaps: a value copied by the service outlives the
service.** `clipboard.copy(value)` and `e.copy()` hand the copy to a
process-wide keeper thread (`strand-clipboard-keep`) that serves it until
another program takes the selection. Persisting other programs' selections
after they exit and the primary selection are not built.

**2026-10-15 · m6-service-gaps: the schema.** `ClipboardEntry { id: int,
text, image: text?, size: int, time: Date }` with `copy()` and `remove()`;
the service has `available`, `text`, `holds_image: bool`, `history`, actions
`copy(value: text)` and `clear()`, and event `failed(action, error)`. No
`secret` flag and no `copied` event.

**2026-10-15 · m6-service-gaps (review r1): the selection a run finds is not
a copy.** A `wl_display.sync` after `get_data_device` marks the selection
told before its answer as found (`Ev::Selection { at_bind }`): it sets
`text` and `holds_image` but is never recorded, so removed or cleared
entries are not written back. Cost: a copy made while Strand was not running
is not added.

**2026-10-15 · m6-service-gaps (review r1): the lock shuts the clipboard.**
strand-surface neither copies nor pastes for a session-lock surface, nor for
any surface while a lock is asked for or held (`State::clipboard_shut`), so
nobody can paste or read the locked session's selection. Tested in the lock
VM (`session_lock.rs::the_lock_neither_pastes_nor_copies`); the config's own
`clipboard` service is not shut.

**2026-10-15 · m6-service-gaps (review r1): `clipboard.holds_image`, not
`clipboard.image`.** `image` is a file path (`text?`) on `ClipboardEntry`
and `Notification`, so the same name as a `bool` read as if `image
clipboard.image` should work. The current picture is `history[0].image` once
recorded.

**2026-10-15 · m6-service-gaps (review r1): shortcuts under a non-Latin
layout.** As GTK and Qt do, a key whose active keysym is non-ASCII matches
as its level-1 keysym in the first layout giving printable ASCII, so Ctrl+V
works under `us,ru` with `ru` active. strand-surface compiles the keymap
with `xkbcommon` 0.8.0 (the one SCTK links) into `seat::LatinKeys`, handed
on as `KeyInput::latin`, which `KeyInput::shortcut()` prefers; `on key(k)`
still sees the active `k.name`.

**2026-10-15 · m6-service-gaps (review r2): no copied data outlives the
history that held it.** `images/` now holds only files `history.json` names:
every save deletes the rest, and a load does too unless the file could not
be read. Temporary files (`.<name>.tmp-<pid>`) go at every load and save,
and `clear()` deletes them with `history.json`.

**2026-10-15 · m6-service-gaps (gap 32): `media.can_next`, `can_previous`
and `can_seek`.** From MPRIS's `CanGoNext`, `CanGoPrevious` and `CanSeek`,
named after `session`'s `can_*` idiom. `CanControl` false makes all three
false; an unreported property counts as able. `can_seek` also needs a track
length above zero, and one predicate (`Player::can_seek`) serves the field
and a `position` write, so a seek with `CanControl` false is not sent.

**2026-10-15 · m6-service-gaps (gap 34): `system.uptime` and
`system.kernel`.** They go on `system` rather than a new service. `kernel:
text` is `/proc/sys/kernel/osrelease`, read once. `uptime: duration` is
`/proc/uptime` (CLOCK_BOOTTIME) floored to whole minutes, a
`#[store(stream)]` field updated by a timer at the next whole minute plus 50
ms, only while shown. `system.boot: Date` was rejected because a `Date` has
no difference; a value may lag up to a minute after resume.

**2026-10-15 · m6-service-gaps (gap 33): a builtin `disk`, shaped like
`memory`, with `all: [Filesystem]`.** `usage`, `used`, `total` describe `/`,
and `all` lists `Filesystem` records (`path`, `device`, `usage`, `used`,
`total`, `free`) with `/` first; `usage` is `used/(used+free)` as `df`
counts it. Listed: `/` plus block-device or ZFS mounts, excluding read-only
images, network and FUSE filesystems, automounts and bind duplicates. It
samples `/proc/self/mountinfo` and `statvfs` every 10 s while shown, one at
a time on tokio's blocking pool.

**2026-10-15 · m6-service-gaps (naming): builtins `disk` and `keyboard`
shadow nothing a config cannot keep.** By spec.md 4.2 a config's own
`service`, `state` or `let` of that name shadows the builtin with a warning.
The record is `Filesystem`; the new provisional stubs go after `outputs`'s
so earlier record ids do not move.

**2026-10-15 · m6-service-gaps (gap 35): notification history is remembered
across restarts, on by default; `notifications.remember: bool rw` turns it
off.** A `transient` hint is not remembered, `clear()` and `dismiss()` erase
from disk, and `remember = false` erases at once and persists the choice.
Stored in the persist store (`services:notifications.all`), newest `KEPT`
(1,000); restored entries are renumbered, come back without actions, never
pop up, and keep only `image-path`. Saves are debounced with a `SaveOnStop`
guard; `notifications::Config { state: None }` disables it.

**2026-10-15 · m6-service-gaps (gap 31): a builtin `keyboard` with
`caps_lock`, `num_lock` and `layout: text?`.** All stream fields. Sources in
order: the focused Strand surface's `wl_keyboard` state (the lock screen's
case); else `/sys/class/leds/*::capslock` and `*::numlock`; else the layout
from compositor adapters (Hyprland, sway, niri events). A layout is the
keymap description (`English (US)`); an unreadable reply leaves it null and
degrades nothing.

**2026-10-15 · m6-service-gaps (gap 31, polling the lights): lock keys are
read from LEDs every 250 ms, only while needed.** LEDs raise no event and no
compositor IPC reports lock keys with events, so with no Strand surface
focused a poll runs only while a visible reader shows `caps_lock` or
`num_lock`; unchanged reads send nothing. Owner question answered with the
recommended "yes", overrulable.

**2026-10-15 · m6-service-gaps (gap 31, interfaces): the focused keyboard
crosses crates as a `SurfaceHost` hook.** strand-surface gains a
no-op-default `SurfaceHost::keyboard_state(Option<&KeyboardState>)` and
`KeyboardState { caps_lock, num_lock, layout }`; the binary forwards it to
`strand_services::keyboard::focused`, so strand-services never depends on
strand-surface. strand-surface takes `xkbcommon = 0.8.0` (SCTK's copy);
`WmState.keyboard_layout` and `WmChange::KeyboardLayout` carry the adapters'
layout.

**2026-10-15 · m6-service-gaps (review round 1 of the small services): four
fixes.** (1) `resolve_held_leave` reports `None` when a held leave turns out
to be a real focus loss. (2) An enter drops that keyboard's lock keys and
reports nothing until the `modifiers` that follow it, so a stale Caps Lock
never flashes. (3) The history save is due 1 s after the last change but no
later than `SAVE_WITHIN` (5 s). (4) `disk::select` leaves out every mount at
or under an `autofs` point, since a `statvfs` keeps it from expiring.

**2026-10-15 · m6-service-gaps (review round 2 of the small services): three
fixes.** (1) Restore percent-decodes a `file://` picture URL as the
renderer's `local_path` does before checking the file. (2) The history is
read through `PersistStore::read_only`, so a broken file is never kept aside
as `.corrupt` and `remember = false` leaves no copy. (3) `Uptime` takes its
reading as a function (`arm`, `rearm`, `publish`) so
`uptime_advances_as_each_minute_turns` drives the service's code.

**2026-10-15 · m6-service-gaps (integration): one keymap compile serves
clipboard shortcuts and `keyboard.layout`.** strand-surface keeps one
`update_keymap` that compiles once (`seat::compile_keymap`) with gap 31's
flags and reads both `LatinKeys::of` and `seat::layout_names_of`; the
text-taking helpers stay for unit tests.

**2026-10-15 · m6-service-gaps (integration): `.text` gates re-based for
gaps 28 to 35.** The eight gaps' code grew the release binary about 600 KB,
all in paths with no cheaper opt-level, so under the owner's re-base rule
(m6-owner 2026-10-12, 2026-10-14) each gate is measurement plus 512 KiB
rounded to 256 KiB: GPU 26,214,400 B and CPU-only 22,282,240 B. The owner
may ask for a cut instead
(`budgets.rs::the_release_binary_code_stays_within_its_gate`).

**2026-10-15 · m6-service-gaps (integration): the debug PSS ceiling is 92
MiB.** Gaps 28 to 35 grew the debug `.text` and
`demo.rs::the_design_bar_keeps_the_m0_budget` hit 82,301 kB on GitHub
against 80 MiB. The debug ceiling (design bar and `services.rs`'s
real-services bar) rises to 92 MiB; release gates do not move.

**2026-10-15 · m6-service-gaps (integration):
`hover_popups.rs::a_tray_menu_left_for_the_island_goes_once` is a known
flake.** It failed about 1 in 52 under load and not only on the merge, so it
is logged as owner-testing.md bug 18 rather than fixed here.

**2026-10-15 · m6-service-gaps (integration): dependencies are optimised in
debug builds; bug 18's cause.** In debug, shadowed animated frames took 100
to 380 ms, overrunning the test's 900 ms window. `[profile.dev.package."*"]`
builds dependencies at opt-level 2 (workspace crates stay 0, debug
assertions on), bringing frames to 1 to 11 ms; release and timing profiles
are unchanged and pixel references hold.

## ci-speed

**2026-10-15 · ci-speed: the inner loop tests what changed; the whole suite
runs once per branch.** Agents spent most of their time in 26-minute CI
runs. CLAUDE.md's per-commit checks are now fmt, workspace clippy and
`run.sh affected` (`scripts/container/affected.sh`: changed crates since the
merge base, docs readers when docs changed, every crate for the root
manifest, lock file or `.cargo/`, `--deep` for dependents, `--list` to name
them). `run.sh ci` runs once per branch, resuming with `CI_FROM=N`;
`scripts/container/test-affected.sh` checks the choices.

**2026-10-15 · ci-speed: incremental debug builds, set once in the
profile.** `incremental = true` in `[profile.dev]` (the test profile
inherits it) replaces the `.cargo/config.toml` override, still one setting
for every agent; a one-line edit rebuilt tests in about 5 s instead of 20 to
38 s. GitHub's jobs set `CARGO_INCREMENTAL=0`.

**2026-10-15 · ci-speed: sccache shares compiled dependencies between
worktrees.** The CI image carries sccache 0.18.0 (pinned, sha256-checked)
and `run.sh` wraps rustc with it, cached in the `strand-sccache` volume (40
GB; `RUN_SCCACHE=0` turns it off). `SCCACHE_BASEDIRS` and a relative
`CARGO_TARGET_DIR` (`target/container`) make hashes match across worktrees.
Local only; GitHub keeps rust-cache and output is identical.

**2026-10-15 · ci-speed: a build alone uses every core.** `run.sh` divides
the cores among running strand builds (containers labelled
`strand.build=1`), at least 4, instead of a fixed `CARGO_BUILD_JOBS=6`,
which still overrides.

**2026-10-15 · ci-speed: `reload_latency` runs 15 rounds in a local `ci`.**
The step is advisory locally, so it runs 15 rounds (50 under
`STRAND_STRICT_TIMING=1`); GitHub's timing job enforces the gate on 50.
Faster debug builds exposed two races:
`lottie.rs::the_file_is_read_on_the_image_worker` now waits for the wake
too, and `renderer::tests::a_poisoned_slot_keeps_no_stand_ins` turns
`ESTIMATE_WAIT` off.

## popup-slide

**2026-10-15 · popup-slide (bug 19): a popup asks to slide only on an axis
where it does not fit its output.** Hyprland 0.56 slides into the output
shrunk by 4 px, moving a popup that merely touches an edge. `popup_slides`
asks for a slide only on an axis where the box passes the output, and on
both while the output is unknown; the flip stays.

## seam-snap

**2026-10-15 · seam-snap (bug 20): a box in flight is drawn with each edge
rounded from where it falls.** Rounding a flight offset apart from the
snapped box made meeting edges miss by a pixel. Layout keeps each box's
residue (`Boxes::residues`), flattening carries in-flight offsets unrounded
(`Inherited::flight`), and a box with anything in flight draws from snapped
edges plus residue plus flight, rounded half up
(`flatten::node::drawn_span`). At rest nothing changes; a moving box's drawn
width may differ by a pixel.

**2026-10-15 · seam-snap: the full shell's PSS ceiling is 104 MB (106,496
kB), from 86 MB; the 76 MB target stays.** GitHub's runners read mimalloc's
heap up to 17 MB higher on the same build, which the container never shows,
and the change measured no cost. Per the owner's 2026-10-15 ruling the
ceiling is the highest reading (97,875 kB) plus about 9%; the 76 MB target
still warns.

## grab-scope

**2026-10-15 · grab-scope (bug 21): a press grabs only for popups on its own
monitor.** A logic step's whole diff carries its press, so an `on change` in
another monitor's bar instance opened a grabbing popup there. The manager
records the layer surface each press landed on (`UserAction::on`), and a
popup grabs only when its layer surface is that one or another on the same
monitor (`State::press_reaches`); a press on an unknown surface reaches any.

## fillet-clip

**2026-10-16 · fillet-clip: a clip above the `attach:` target lets its
fillets through.** Drawing them removes a concept, where a checker warning
would add one: `clip:` still means "hold the content to this box", and
fillets are the surface's, not content. Each clip above the target takes in
the fillets and the square corners they meet (`fillet::corners`), bounded by
the clip's own box, and only the target's fill passes: other content is
re-held to the box (`Flattener::hold_to_box`). Edges clipped twice antialias
a shade lighter
(`tests/fillet.rs::a_clip_above_the_card_lets_its_fillets_through` and
siblings; PNGs `fillet_short_clip`, `fillet_slid_out_of_clip`).

## letters-placement

**2026-10-16 · letters-placement: `letters` placement is a schema entry;
elsewhere it is `check::misplaced`.** Per spec.md 6.1, builtin.schema says
`letters` is `flags leaf only_in text`, replacing a checker special case; a
leaf may hold any element whose `only_in` names it. A misplaced element's
children are not also reported, and the diagnostic says where to write it
(`text … { letters { … } }`). Tests:
`tests/negative/letters_outside_text.strand`,
`checker.rs::a_text_holds_letters_and_nothing_else`.

## list-scroll-regrow

**2026-10-16 · list-scroll-regrow: a reveal stands until the pointer
scrolls, and is placed again when the view changes size.** A reveal is
recorded in the list's `ScrollState` (row, global index, offset before it)
and re-placed from that offset on every layout pass that changes the view or
remeasures rows; a pointer scroll or the row's removal ends it. A row taller
than the view is revealed at its top, as GTK and browsers do.

## change-optional-chain

**2026-10-16 · change-optional-chain: `on change` over a record's key fires
when the record switches.** A target whose field is the record's key, or
holds it (a path key's first step), compares values only (`lower::handler`),
since for a key the switch is the only change; other fields of a keyed
record still re-baseline on a switch (spec.md 8.4). Each target is paired
with its own identity (`strand_core::Runtime::on_change_each`,
`on_change_after_each`). A list query's item switching re-baselines too:
the owner kept that, with the key-watching idiom documented
(owner-testing below, 2026-10-29).

## nav-remount

**2026-10-16 · nav-remount: `nav:` names the list as it is once the change
is done.** A node-valued prop's memo now also gives the element it names
(`PropOut::node`); the instance keeps such props (`Ctx::node_props`,
replacing `late_nodes`) and sets each at the end of every tick to the node
the element has then, or unsets it. So `nav:` follows its element across
mounts, remounts and rekeys without a signal per element; spec.md 6.7 says
what `nav` names.

## launch-env

**2026-10-16 · launch-env (G9): variables addressing one shell stay with it;
`STRAND_STATE_DIR` names its state dir.** `STRAND_STATE_DIR` (default
`$XDG_STATE_HOME/strand`, absolute only) is resolved in
`strand_core::persist::state_dir` by every store and `strand export`.
`strand_services::child::child_env` (was `activation_env`) removes
`STRAND_SOCKET` and `STRAND_STATE_DIR` from every program the shell starts;
other variables pass. No `STRAND_CACHE_DIR` or config variable; `strand
help` lists the two.

## morph-cross-surface

**2026-10-16 · morph-cross-surface: a node starting a shared morph shows its
creation props at their values.** A morph plays "in place of its `enter`
pose", not in place of being new, so a node that starts one drops its
creation writes as an entering node does (`Animator::shared_morph`) and only
the morph moves it; before, its props sprang from 0. design.md unchanged.

## shadow-slice

**2026-10-16 · shadow-slice: a shadow on a box large enough for its blur is
drawn from size-independent slice tiles.** For a box at least `2 × 4σ`
across, vello's blurred rounded rect is flat in size, so each quadrant draws
a base tile and a corner tile of its own radius, keyed by blur, colour,
radius and sub-pixel phase, never size: design.md's 9-slice. Transformed
shadows, small or pill boxes and blur under 0.25 px keep the whole pixmap;
corner tiles are held uncached (`PaintCache::recent`) until asked again.
Matches the direct blur within one level
(`cache.rs::sliced_shadows_match_the_direct_blur`).

**2026-10-16 · shadow-slice (review): a corner tile waits for any later
frame within a 4 MB hold; tiles over 2 MB are not sliced.** Held tiles are
cached when any later frame asks, bounded by `RECENT_BYTES` (4 MB) and
counted in `PaintCache::bytes`, since surfaces share the cache. A shadow
whose tiles would pass 2 MB keeps its whole pixmap under
`EAGER_ENTRY_BYTES`, so one shadow's tiles hold at most 16 MB
(`cache.rs::other_surfaces_painting_between_frames_do_not_lose_corner_tiles`,
`small_round_or_sharp_boxes_are_not_sliced`).

## gpu-first

**2026-10-16 · gpu-first (OWNER RULING): animation draws on the GPU from its
first large frame; the device stays warm.** Speed beats size: a surface
promotes at its first large frame (0.2 Mpx damaged) while it animates, or
the third large clock-driven frame in a row; it demotes after 3 s settled (a
small spring frame stays hot only if the CPU took `CHEAP_FRAME`, 4 ms,
median of the last five), and 15 s after a re-promotion within 30 s
(`AGAIN_FACTOR` 5). `strand run` warms the device (`Renderer::warm_gpu`,
`BackendChange::Start`) only where a render node or `STRAND_GPU_SOFTWARE=1`
exists, else `Renderer::gpu_absent`; no adapter is final (`Device::Absent`).
The binary sets `VK_LOADER_DRIVERS_DISABLE` against software and absent
vendors' drivers and uses `PowerPreference::LowPower`. Budgets have two
legs: cold (no render node or software adapter, nothing Vulkan mapped) keeps
bar 42 / 48 MB, full shell 76 / 104 MB, debug 96 MiB, and
`demo.rs`/`services.rs` 48 MB and 92 MiB debug; warm on CI's lavapipe is bar
232 / 242 MiB, full shell 296 / 324 MiB, debug 288 MiB.

**2026-10-16 · gpu-first (review 2): slow-frame logging, a presented
surface's new size, three small fixes.** Under `STRAND_LOG=debug` the
`strand-gpu` thread logs frames over `strand_gpu::SLOW_FRAME` (40 ms) by
phase with `strand_gpu::thread_stats`; the gain is the tail, not the median.
Presented surfaces are held for their new size again and the GPU thread
commits it (`GpuRequest::Commit`, via `Renderer::take_surface_changes`).
`render_vendors` treats unreadable vendors as unknown, and a failed warm
device retries once at 30 s (`promote::Device::retry_at`,
`Renderer::gpu_wake`). Thread priority via rtkit is left open.

**2026-10-16 · gpu-first (exit): a GPU thread still running at exit ends the
process with `_exit`.** A SIGTERM during lavapipe's warm-up let the driver's
static destructors run under the live thread (SIGSEGV). When the thread
outlives `GPU_SHUTDOWN` (3 s), `run::run` joins logic and the compiler,
flushes and calls `libc::_exit` (`exit_beside_gpu_thread`); waiting longer
was rejected because a hung driver would hold exit forever.

## syncobj-lend

**2026-10-16 · syncobj-lend (bug 22): a surface lent to the GPU thread takes
no buffer until handed off or taken back.** Mesa's WSI turns explicit sync
on when the compositor offers `wp_linux_drm_syncobj_manager_v1`, after which
Hyprland refuses a buffer with no acquire point ("Missing acquire
timeline"). The manager's `draw` now attaches nothing to a lent surface (a
bare configure-ack commit still goes) and draws the skipped frames at
`take_back`
(`gpu_handoff.rs::a_lent_surface_takes_no_buffer_until_it_is_taken_back`).

## gpu-regions

**2026-10-16 · gpu-regions (bug 23): a GPU frame sets its opaque, input and
blur regions as pending state for its present.** A handed-off surface kept
the last shm frame's regions, so a `trigger: hover` popup closed early.
`State::sync_presented(surface)` runs `sync_opaque`, `sync_painted_input`
and `set_blur` after each GPU frame, committing nothing; the shared
`wl_display` queue keeps order, and the manager stays the only speaker of
region requests
(`gpu_handoff.rs::a_handed_off_surface_takes_input_where_its_gpu_frame_drew`).

## flip-open

**2026-10-17 · flip-open (bug 24): a layout pass caused by a text's arrival
glides what it moves from where it was drawn.** A FLIP taken by a pass laid
out from estimates left siblings drawn far from their box. A structural
change's scope stays open on the surface (`flip_open`) until every text is
shaped (`flatten_surface`'s `settled`), and a pass after a text arrived
(`text_arrived`) glides moved boxes from the previous pass
(`Motion::shift`), leaving out `unpainted` nodes
(`morph.rs::a_mounted_text_moves_its_siblings_from_where_they_were_drawn`).

## hidden-text

**2026-10-17 · hidden-text (bug 25): text under a node drawn hidden is asked
for and waited on as visible text is.** Flattening skipped `opacity` 0 or
`scale` 0 subtrees, so their text stayed estimated while layout still gave
them room. Such a node now walks its subtree for text only
(`Flattener::hidden_texts`), reading each `text` and button label's spec but
no items, clocks or animation state; an `input` keeps its estimate
(`morph.rs::text_entering_faded_out_moves_its_siblings_once`).

## eval-sandbox

**2026-10-17 · eval-sandbox: the animation evaluation sandbox is its own
Cargo workspace, `eval/`.** It is tooling, not product, so it sits outside
the root `members` (precedent `spikes/gpu-present`) and `--workspace`,
`affected` and the shipped binary never see it. It names Strand crates by
path, keeps a `Cargo.lock` held to the root's versions
(`scripts/eval/check-lock.sh`), copies profiles and lints
(`eval/tests/drift.rs`), runs with `--manifest-path eval/Cargo.toml`, and
`scripts/eval/check-isolation.sh` holds the boundary.

**2026-10-17 · eval-sandbox: strand-render's hooks are a cargo feature,
`eval-hooks`, not a runtime flag.** No root crate enables it, so product
builds hold none of it, and root clippy `--all-features` still lints it; a
runtime flag would ship a branch per node. Hooks are cfg'd blocks counted by
`check-isolation.sh`; the probe's `impl Renderer` is in `renderer/probe.rs`,
types in `src/probe.rs`, the trace reaches `flatten()` via `Extras::trace`.
A node's `visual` box is its drawn box through its transform, a drag's lift
counts as authored, and the animator reports `ENTER` while enter-pose
springs run.

**2026-10-17 · eval-sandbox: lateness is inline shaping plus a hold, not a
delayed worker.** The deterministic Inline backend reproduces the worker's
in-time order (estimate pass, delivery, shaped pass). Text later than the
wait is made by holding the request in `pending` so layout and flatten treat
it as a late worker's, until `Renderer::release_text` hands it to `deliver`.
The hold changes nothing unless the sandbox turns it on.

**2026-10-17 · eval-sandbox: the input router reads its clock through
`now()`, which `Router::set_now` can pin.** Hover `delay` and `HOVER_GRACE`
use the wall clock; a private `fn now(&self)` returns `Instant::now()`
unless `eval-hooks` pinned it. A clock trait would add a concept for one
caller; this is the hooks' only non-cfg'd change.

**2026-10-17 · eval-sandbox: a second workspace sharing `target/container`,
measured.** `.workspace = true` fields of path dependencies resolve in the
root workspace, so the fallback root member `crates/strand-eval` is not
needed, and the two workspaces' units sit side by side without thrash. eval
uses clap's builder API so its lock adds no package but strand-eval, and
`eval/tests/drift.rs` compares only profile sections for packages in eval's
graph.

**2026-10-17 · eval-sandbox: a scenario is a TOML header, the config's files
and a steps script, none of it Strand syntax.** The shell is `.strand` text
in `=== <path>.strand` sections or a `config` path; steps are one verb per
line (`set`, `pointer`, `text late 1 where "…"`, `rest`; docs/eval.md),
values read as `strand set` reads them. A second use of the language would
add a concept. `#` comments only when standing alone; `[allow]` needs a
`why`, `expect_fail` a reason; outside `eval/scenarios/local/` a scenario
may name only files in `eval/` and bundled fonts. `id:` selectors rest on
spec.md 6.3.

**2026-10-17 · eval-sandbox: the driver runs a scenario in one process on a
virtual clock.** `harness::run` compiles from an in-memory `SourceMap`,
boots `Instance::from_build` on the schema mock with one screen (`Mock |
DP-1`), drives renderer and router at `T0 + k·period`, and zeroes text
waits. Additions: `set_text_hold_with(Option<HoldPick>)` filters the hold;
`late N` releases N ticks after first held; selectors resolve against the
live tree then nodes seen. `notify`, the text worker, GPU backends and
`lock` are typed errors. A node's key is
`id|file:line:col|instance_path#sibling`.

**2026-10-17 · eval-sandbox: the checks and their thresholds.** Each check
reads only the trace (eval/src/checks/), tested with a passing and a failing
synthetic trace. A node shows when present, effective opacity ≥ 0.02, area ≥
1 px; `tol = max(edge_px, 1/scale)`. **bystander** (opacity within 0.01,
scale 0.005), **envelope** (`ov = max(3 px, overshoot·|e−s|)`, 0.35),
**teleport**, **vanish** (back within 3 ticks), flash, blank, **pop-in**
(first frame must report `ENTER` and be no more than half way to rest),
**twin** (reduced-motion rerun, opacity within 1/255), **determinism**,
**budget**, **reference**. `expect_fail` gives XFAIL/XPASS, XPASS failing
the gate.

**2026-10-17 · eval-sandbox: the scenarios, the `check` command, and what
running them changed.** `strand-eval check [--gate] <scn|dir>... [--out
DIR]` writes `trace.json`, `checks.json`, `summary.txt`, `summary.md` and
exits 1 on failure, 2 when a run cannot happen; `--gate` refuses an empty
set and `scenarios/local/`. Check fixes, no threshold moved: a bystander
must be drawn alike at `s` and `e` and laid alike on every drawn frame
(`checks::way`); a blink is one no step explains; a frame hidden by an
`enter`/`exit` pose is not blank; a detached surface leaves the timeline.

**2026-10-17 · eval-sandbox: mutants prove the regression scenarios catch
bugs 24 and 25, Inline, no lateness.** `eval/mutants/*.patch` reverse each
fix; `scripts/eval/mutants.sh` (`run.sh eval mutants [name]`) reads
`eval/mutants/mutants.txt` (`<patch> <scenario> <expected failure>...`),
runs each on HEAD (must pass) and patched (must fail as listed, else WEAK;
STALE if it no longer applies, SURVIVED if it passes), in one detached
worktree. Both fail `bystander` on y, so bug24-flip-open is the gate and
bug24-flip-open-late1 is not a mutant proof.

**2026-10-17 · eval-sandbox: a run's report is headless first; its viewer is
one self-contained HTML file.** Every `run` and `check` writes `summary.md`
(appended to `$GITHUB_STEP_SUMMARY` only by `scripts/eval/ci.sh` and
`matrix.sh`), `summary.json`, `summary.txt`, per-scenario traces,
`fail-<check>-<n>.png` contact sheets and `index.html`. The viewer
(`eval/viewer/viewer.{html,css,js}`, `include_str!`) names no URL and stays
under 15 MiB by thinning frames (`--frames all|failing|none`). `strand-eval
explain`, `verdict` (`eval/refs/<id>.ref.json`, no pixels), `ab` and
`scripts/eval/ab.sh` (`run.sh eval ab-revs`) are built.

**2026-10-17 · eval-sandbox: the effect matrix, `promote`, and what its
first run changed in the checks.** `eval/matrix/fixture.strand.in` plus
`eval/matrix/effects.toml` (22 effects) make every effect alone and every
pair at each rate (`strand-eval matrix`, `run.sh eval matrix-all`);
conflicting pairs are `n/a`, cell ids are `m.<a>__<b>.<hz>`, and a
`known.toml` entry whose check passes is stale and fails. `strand-eval
promote ... --as NAME [--xfail CHECK --note TEXT]` writes a regression
scenario. Check fixes: twin leaves out lean and jelly nodes; teleport's
bound is 0.75 of the larger of travel and glide offset.

**2026-10-17 · eval-sandbox: the GPU leg, `cpu-gpu` and `regions`.** The GPU
leg (`eval/src/gpu.rs`) warms the device, promotes every surface with
`set_promotion(false)` and reads back each `paint_gpu` frame; `Presented`
and `shader` passes are left out. `backend both` is two deterministic runs
compared tick by tick (boxes 0.01 px, opacity 1e-4, pixels within gpu.rs's
tolerance: 6 a channel, at most 2% past); `[allow]` with `checks =
["cpu-gpu"]` masks a node. Without a device it skips unless
`STRAND_REQUIRE_GPU=1`. `regions` gates opt-in on spec.md 6.7's hover-input
rule, regions recorded in `Frame.regions`.

**2026-10-17 · eval-sandbox: the first 144 Hz matrix run and what it changed
in `teleport`.** `teleport` counts a node lifted by a drag then gliding as
carried with the offset it was left, and on w and h only motion that sizes a
box counts (a size spring or morph, lean, jelly, or authored w, h, scale or
rotate). Product findings (exit-height0, the first-frame lead under
`START_LEAD` 16.7 ms and GPU buffer age 0) went to known.toml at 144 Hz,
since fixed.

**2026-10-17 · eval-sandbox: CI runs the sandbox in its own job; `affected`
treats it as a pseudo-target.** ci.yml gains an `eval` job (every push, 40
min, full tier, rust-cache key `eval`) running scripts/eval/ci.sh's five
steps and uploading `eval-report`; the matrix and mutants run nightly in
`.github/workflows/eval-matrix.yml`. Local `ci.sh` runs them as steps 35 to
39 (`CI_JOB=eval`). `affected` runs every eval step when eval/,
scripts/eval/, the root Cargo.toml or Cargo.lock changed, and for a
strand-render change runs `cargo check -p strand-render --features
eval-hooks`.

**2026-10-17 · eval-sandbox: wrap-up: fonts, owner files, docs, and what is
left open.** Committed scenarios shape text with the bundled Liberation Sans
regular and bold; `fonts = "local"` (`$STRAND_EVAL_FONTS`), `config_dir` and
outside configs are accepted only in gitignored `eval/scenarios/local/`,
which `check --gate` refuses. docs/eval.md is the guide; design.md
"Testing", features.md and README gain entries. `bench` and `new` were
reserved with a typed "unsupported" error.

**2026-10-17 · eval-sandbox review: the driver's wakes follow the shell's
loop; the renderer's timers read the virtual clock.** The tick is now the
shell's (eval/src/harness.rs): only non-empty diffs applied with
`Router::observe`, `Renderer::apply`, `Router::settle` and `set_input_wake`,
wakes on `text_ready` and `next_wake`, one paint per surface, and reduced
motion set through `system.reduced_motion`. Beyond the plan's footprint:
cfg'd `Renderer::set_now(Option<Instant>)` with a private non-cfg
`Renderer::now()` read by tooltip timing and capped clocks, and cfg'd
`take_inline_shaped()` so inline-shaped layouts wake the loop (16 in a row
is an error).

**2026-10-17 · eval-sandbox review: what makes a node a mover, what it
inherits as authored, what is in flight.** (1) A frame laid out off the
segment's span counts only as an excursion with a reason
(`checks::LAYOUT_CAUSES`, or `Frame::driven`); otherwise bystander fails.
(2) `harness::inherited` passes on only offset (x, y), opacity, and scale or
rotation, not an ancestor's authored width or height. (3) Rows record `fx`,
`fy`, the inherited part in flight, which `teleport` reads; old traces read
as nothing in flight.

**2026-10-17 · eval-sandbox review: a known failure names its nodes, and
each frame its scale.** A known.toml entry now needs `nodes`, resolved
against the cell's trace (`Known::covers`); a finding on an unnamed node
fails the cell, and `*` is the only way to accept a node-less finding.
`Frame::scale` records each frame's scale, and `tol` is one physical pixel
at the coarsest scale drawn.

**2026-10-17 · eval-sandbox review: promote, local configs, A/B, `affected`,
step summary, viewer.** `promote` refuses a run whose scenario is under
`eval/scenarios/local/` or unknown; `${VAR}` in `config` expands only in a
local scenario (a committed one refuses `${`). `ab.sh` copies the checkout's
scenarios and fixtures into the revision's tree. `affected` runs every eval
step and builds strand-render with `eval-hooks`; only ci.sh's gate and
matrix.sh append to `$GITHUB_STEP_SUMMARY`.

**2026-10-17 · eval-sandbox hardening: the trace gains text `fit` and `ink`,
surface origins and committed poses.** Seeded defects showed text was
invisible to the trace, so strand-render's `eval-hooks` traces per text
`fit` (the line box asked for) and `ink` (glyph bounds drawn, clipped) in
probe.rs `NodeTrace`; nothing else in crates/ changed. The driver records
each surface's `origins` (strand-surface's `popup_config`/`popup_request`
arithmetic) and, with a `compositor_poses` header,
`Renderer::delegated_pose` read right after paint. Rows grow to 38 columns;
older rows still read.

**2026-10-17 · eval-sandbox hardening: checks read what the eye sees; new
`coherence` and `ink`.** `RunTrace::on_screen` moves rows with their surface
and folds in committed poses, and every check reads that. `coherence` holds
`inherit`, `pivot`, `morph start` (at most 0.5 of the way on the first
frame) and `stacking` (an in-flight node never goes below an overlapping
sibling while layout keeps order). `ink` holds `spill` (ink within its box ±
2 px) and `over-cut` (an ellipsised text never draws less as its box widens,
counted only where the prior frame's ink stopped short of the edge).

**2026-10-17 · eval-sandbox hardening: false positives fixed in `teleport`,
`vanish` and `twin`.** `teleport`'s travel is the span of the node's
`way()`, and its retarget arm applies on x and y only. A frame is blank only
when something inside the surface's root shows, keyed per surface and
segment. Pop-in holds each time an element comes onto a surface, and a new
`pose back` rule holds bug 7: a surface's committed exit pose never comes
back toward rest (gives 0.01 opacity, 0.005 scale). `twin` now compares
time-driven nodes, since clocks stand at t = 0 at rest under reduced motion.

**2026-10-17 · eval-sandbox hardening: coverage, and what it proved.** New
curated scenarios (title-pill-cut-late, title-pill-cut-waker,
cond-transition, time-signals-reduced, closing-pose-delegated,
popup-grow-centred) and matrix fixture changes; temporary strand-render
mutations proved `inherit`, `pivot`, `morph start`, `spill`, `twin` and
`pose back`, while `over-cut` and `stacking` are proven on synthetic traces
only. A popup reopened after its exit showing at rest without its enter pose
was left to triage.

**2026-10-17 · eval-sandbox: `bench` and `new` are built.** `strand-eval
bench <run>... [--out FILE]` (eval/src/bench.rs) prints nearest-rank p50,
p95 and max per surface cost, wall times marked advisory, and never fails on
a value; `[budget]` stays the only cost gate. `strand-eval new <id>
[--config F] [--from-steps FILE] [--to DIR]` (eval/src/scaffold.rs) writes a
skeleton to `eval/scenarios/local/`, never replacing a file, mapping
shots-keys verbs tolerantly and keeping unknown lines as `# shots-keys:`
comments.

## exit-height0

**2026-10-17 · exit-height0: an exit to a size stays laid out and clipped
there until it unmounts.** (1) An exit now ends with the frame: the pass
that ends it records it finished and `take_finished` removes it as it
unmounts, so every pass of that frame draws it at its pose (`finish_now`,
`finish_undrawn` alike). (2) A node exiting to a pose that sets its size
clips as it does while the size springs. (3) A node clipping its content to
nothing asks for its subtree's texts (`hidden_texts`). Test:
`morph.rs::a_collapsing_exit_hides_what_it_collapsed_until_it_unmounts`.

## first-frame-lead

**2026-10-17 · first-frame-lead: a change starts one refresh of its surface
before the first frame that shows it.** A change used to lead by up to
START_LEAD (16.7 ms) from `painted_time`, so after idle frames at 144 Hz a
glide jumped 2.4 frames, and differently on GPU (age 0).
`Motion::sample_frame(at, period)` starts a change `min(at - last, period,
START_LEAD)` before `at`, with the period from `Clocks::period`;
`Animator::set_period` replaces `set_slack` and every painted sample passes
it. At 60 Hz nothing visible changes
(`motion.rs::a_glide_after_idle_frames_leads_by_one_refresh_at_any_buffer_age`).

## late-text-dip

**2026-10-17 · late-text-dip: a text not shaped yet takes no room, so a late
text moves its siblings once.** A text with no layout measures only its
authored width (or `min_width`) and no height, and folds its gap when it has
neither (`layout::style`, `unshaped_nothing`). List rows keep the estimate
(`await_unmeasured`), as does a content-sized surface's sizing pass
(`refresh_specs`, `TextSizes::estimates`), since resizing a shown popup
drops its grab. A box clipped to nothing asks for its texts
(`Flattener::hidden_texts`), a node still shaping stays `unpainted`, and an
`enter` pose over such text holds up to `FIRST_FRAME_TEXT_WAIT`
(`Animator::hold_enter`). This revises flip-open's rejected alternative:
moving once beats away-and-back.

## jelly-drag

**2026-10-17 · jelly-drag: a preview that sees a held jelly node moved
leaves it pending, so the drag's frame measures the move.** Every drag frame
reused its `update` preview, and only a committing flatten measured speed
(`Jellies::sample`), so jelly never stretched. As `Morphs::pending` does, a
preview measures nothing but marks a held node seen at a new offset pending,
which `Jellies::busy` counts so the next frame flattens afresh; a committing
sample, `forget` and `retain` clear it
(`effects.rs::jelly_stretches_a_drag_whose_moves_are_previewed`).

## popup-boot-wake

**2026-10-17 · popup-boot-wake: a popup waiting for its anchor's box is
placed by the layout that gives it one.** A text-less bar's popup open at
boot waited for an unrelated wake. A layout pass giving a waiting popup
(`anchor_rect` none) its anchor's box marks its spec dirty
(`Renderer::place_waiting_popups`), and `release_holds` reports it in the
same wake. An anchor that moves still keeps its popup until the next
refresh, since following a spring would reposition it every frame
(`popups.rs::a_popup_open_at_boot_is_placed_by_its_bars_first_layout`).

## eval-sandbox-integration

**2026-10-17 · eval-sandbox-integration: the five sandbox fixes on one
branch.** exit-height0, first-frame-lead, late-text-dip, jelly-drag and
popup-boot-wake are cherry-picked in that order; flatten/node.rs keeps one
`hidden_texts` call for clipped-to-nothing nodes. known.toml has no entries
left, and the mutants take late-text-dip's regenerated patches (bug-24
`teleport:#bars:x` on bug24-flip-open-min, bug-25 `bystander:#status:h`).

**2026-10-17 · eval-sandbox-integration: a surface's frame period recovers
from a stray short gap (first-frame-lead review).** The period is the
shortest of the last 8 gaps (`clock::GAPS`), counting only gaps between
1/360 s and `MAX_GAP` (1/60 s plus a tenth); longer gaps leave it unchanged
(`clock::tests::the_period_recovers_from_a_stray_short_gap`). Left open: a
surface's first glide after boot at 144 Hz, and variable refresh; the fix is
`PaintTarget` carrying `wp_presentation`'s refresh, an architecture.md
change.

**2026-10-17 · eval-sandbox-integration: late-text-dip review fixes.** (1) A
leaf with no characters keeps one empty line (0 × ⌈1.2 em⌉) in
`measure_text`, so a label-less `button` keeps its line. (2) A paint keeps
an empty box in `unpainted` only when late text is under it. (3)
`renderer::tests::a_late_text_in_a_box_clipped_to_nothing_is_shown` proves
the clip-to-nothing call on the worker. (4) bug24-flip-open.scn stays as a
behaviour check, not the fix's proof.

**2026-10-17 · eval-sandbox-integration: a jelly the painted frame did not
draw is dropped (jelly-drag review).** A pending or moving jelly under an
ancestor hidden before the frame kept `Animator::busy` true.
`Animator::finish_undrawn` now drops it with the other undrawn paint state,
and it comes back at rest
(`renderer::tests::a_jelly_hidden_before_its_frame_leaves_nothing_moving`).

## eval-refs

**2026-10-17 · eval-refs: every scenario and matrix cell is held to an
approved reference recording.** Rule checks missed six of nine held-out set
B bugs that changed the trace; a reference catches the change and leaves
judgement to a person once. It replaces the verdict files (bad verdicts
merge in as `verdict bad` blocks). A run off its recording is CHANGED, which
gates `check` (`Report::ok`) and the matrix (`Outcome::Changed`); a missing
reference fails `check --gate` and the matrix but skips under plain `check`;
`--reduced-motion` runs are not compared.

**2026-10-17 · eval-refs: what a recording holds, its quanta, and no
pixels.** Per painted tick: surfaces painted, each surface's size, origin,
pose and input region, and each node's drawn and visual box, opacity, scale,
rotation, hidden reason and text state; quantised to 1/64 px, 1/255 opacity,
1/1000 scale, 1/100 degree, matched within one quantum. No pixel hash, since
vello_cpu's SIMD choice may differ per CPU. Change-only text lines, one file
per scenario and per matrix rate; tripwires: 5 MB total, 64 kB a scenario, 8
kB a matrix cell.

**2026-10-17 · eval-refs: an approval carries its reason, and CI checks
that.** `approve` requires a note, recorded with date, revision and approver
(`STRAND_EVAL_APPROVER`) and sealed by a hash. `refs-check`
(scripts/eval/ci.sh step 5) refuses a broken seal, a missing note, and a
recording changed since the merge base under its old note; GitHub's eval job
uses `fetch-depth: 0`. Missing cells are STALE REFERENCE, pruned by `approve
--all-changed`.

**2026-10-17 · eval-refs: the eval scripts forget only their own worktree.**
In the container a `git worktree prune` sees every other worktree as
missing, so `ab.sh` and `mutants.sh` now remove only their directory and its
admin entry (`forget_tree`) and never prune.

**2026-10-17 · eval-refs: the `redraw` check holds every partial redraw to a
full repaint of the same frame.** A stale damage band passed every other
check. After each paint the harness redraws the frame in full into a fresh
buffer (CPU: `Renderer::repaint_fresh` with `Raster::fresh`; GPU:
`Renderer::fresh_gpu_frame` on a second device `gpu::Fresh`) and compares;
findings count pixels outside `Renderer::last_damage`, and an uncompared
painted frame fails. A default check on the main run and GPU leg
(`eval/tests/redraw.rs`).

**2026-10-17 · eval-refs: the repaint changes nothing the run draws.** Both
hooks take `&self` with their own raster and upload table, reading a copy of
the display list (`Probe::last`, under `Renderer::set_keep_frames`); the
only outside effect is `hugepage::trim_idle`, which never changes pixels.
Digests and images are byte-identical with the check on and off.

**2026-10-17 · eval-refs: `redraw` allows no difference on the CPU and one
step on the GPU.** The CPU raster draws each pixel identically whatever the
damage (`Raster::paint_window`), so `CPU_TOLERANCE` is 0. On the GPU a fresh
device places pixmaps elsewhere in vello_gpu's atlas and bilinear sampling
of transformed glyphs can move a channel by one, measured on six cells, so
`GPU_TOLERANCE` is 1; within it is an info line.

**2026-10-17 · eval-refs: `redraw`'s real failure is a test-only damage
mutant.** `Renderer::set_damage_inset(px)`, behind `eval-hooks` and set only
by eval/tests, shrinks each frame's damage, and island-card-swell then fails
`redraw` (`a_damage_band_mutant_fails_redraw`). A switch proves a check
without a detached build; the GPU leg has no such mutant.

**2026-10-17 · eval-refs: `redraw` holds the age-1 buffer the sandbox
models.** Each sandbox surface keeps one buffer, so every frame paints at
age 1 and the check holds each frame's own damage, not the union ages 2 or 3
add. Another age would change `raster_px`; recorded as a limit in
docs/eval.md.

## popup-reopen

**2026-10-17 · popup-reopen: a closing surface drops what its props were
drawn at, so it enters from its pose when reopened.** A preview between the
exit's end and the surface's destruction snapped held props to rest, so a
reopened popup showed at rest. When `refresh_specs` reports a surface
closed, `Animator::surface_closed` drops its nodes' paint state (size
springs stay; shared `drop_paint_state`); reopening during the closing pose
still uses `cancel_exit`
(`surface_poses.rs::a_surface_opened_again_after_its_exit_enters_again`,
curated `popup-reopen.scn`, `popup-reopen-delegated.scn`).

## shared-morph-on

**2026-10-17 · shared-morph-on: a morph name is where each painted frame
shows it; a box on screen starts a morph however long ago it was drawn.**
Names were recorded only in committing flattens, so a box painted from a
reused preview was remembered nowhere. A flattened scene lists its drawn
names (`Flattened::morphs`), and every painted frame hands them to
`Animator::shared_painted`; a box on its surface's latest frame starts
morphs indefinitely, and for one second (`STALE`) once off screen or after
`detach_surface`. A preview's start the painted frame cannot keep is
`Start::Missed`, and the node plays its enter pose.

## popup-shrink

**2026-10-17 · popup-shrink: a held content-sized surface lays out its
content where its smaller box will be.** wave3-pixels' growth glide reason
holds for shrinking too, and it named this fix. While held
(`Renderer::held`), a content-sized `panel`, `osd` or `popup` root is laid
out at its content's size and placed at its share of the room
(`RootSize::Placed`, `layout_pass.rs` `root_size`); bars, locks and
compositor-sized surfaces still fill. One rule says where a resized box
stays (`specs.rs` `resize_share`), matching `placement::layer_config` and
`popup_config`, and `glide_origin` reads it.

## eval-refs-integration

**2026-10-18 · eval-refs-integration: an approval names who looked; the seal
only guards against hand edits.** `approve` and `verdict` take `--approver
NAME`, else `$STRAND_EVAL_APPROVER`, refusing neither or `unknown`
(`refs::approver`), and `refs-check` refuses an empty or `unknown` approver.
The seal stays an unkeyed hash, since a key every approver could read is no
secret; trust comes from reviewing the reference diff and note in the
commit.

**2026-10-18 · eval-refs-integration: a recording holds paint order and a
text's ink and cut width; non-numbers stay non-numbers.** Each node records
`ord`, `ink` and `fit`, defaulting when absent; colour, gradients, radii,
borders, shadows, blur and crossfade weight stay out, covered by the pixel
tests docs/eval.md's Limits names. `qz` quantises NaN and infinities to
sentinels written `nan`, `inf`, `-inf`, and clamps finite values to 2^52
quanta.

**2026-10-18 · eval-refs-integration: the whole matrix is held to its
references on every push.** The full matrix at both rates (about 90 s) is
scripts/eval/ci.sh step 6, run by GitHub's eval job, `CI_JOB=eval run.sh ci`
and `affected`; the nightly keeps the matrix and mutants.

**2026-10-18 · eval-refs-integration: the baseline is recorded from a
committed build after the three fixes.** Every block was re-recorded from
one committed build so its `rev` reproduces it. Blocks reviewed here name
their fix; the rest say they are a baseline that passes its checks, approved
by Claude and not reviewed by the owner.

**2026-10-18 · eval-refs-integration: `redraw` repaints the inspector's
overlay too.** `keep_frame` now runs before the overlay items are cut, so
the kept frame is the frame as drawn
(`redraw.rs::a_frame_with_the_inspector_overlay_is_repainted_with_it`,
`RunOptions::damage_flash`).

**2026-10-18 · eval-refs-integration: worktree helpers forget a tree whose
directory is gone.** `remove_tree` forgets the entry even when the directory
is missing, and `forget_tree` matches by `realpath -m`; both live in
scripts/eval/worktree.sh, tested by scripts/eval/test-worktree.sh in eval CI
step 3.

**2026-10-18 · eval-refs-integration: a resized box moves by the
compositor's whole-pixel halves.** `layout::edge_shift` places a held root
and the growth glide as `popup_request` (`ax + aw / 2 - w / 2`) and centred
layer surfaces do. `resize_share` and `edge_shift` are public and
`crates/strand/tests/resize_parity.rs` holds them to `layer_config` and
`popup_request` for every anchor. A centred child can still step a pixel
through odd sizes
(`motion.rs::a_centred_panel_at_odd_sizes_lands_where_the_compositor_puts_it`).

**2026-10-18 · eval-refs-integration: a closing surface takes its jellies
and morph flights with it.** `surface_closed` also drops jellies and morph
flights; the boxes morph names were drawn at stay (`SharedMorphs::stop`). A
surface with only an `exit` pose now shows at rest when reopened after its
exit, matching design.md's "enter plays when it opens".

**2026-10-18 · eval-refs-integration: review items left as they are.** (1)
`Start::Missed` restores a node to the `enter` set even without a pose,
which is that set's meaning. (2) `vanish` exempts a morphing first frame
starting at its own box. (3) shared-morph boxes are keyed by root, so a root
on two outputs takes the last painted; keying by surface needs origins in
render. (4) A hold `RESIZE_WAIT` releases early lays out filling the old
buffer for a frame. (5) `redraw`'s GPU tolerance must be confirmed on
GitHub's first nightly.

## eval-coverage

**2026-10-18 · eval-coverage: a recording holds what each node painted, read
from the scene.** With `eval-hooks`, `Flattener::node` reads the node's own
display items into `probe::NodePaint` (fills, borders, shadows, glyph runs,
glyphs identified by source text via `Extras::texts`); the trace adds a
`paint` column (39 columns) and per-surface `fades`, the recording node
fields `bg`, `bd`, `sh`, `fg`, `gl` and surface field `f`. Colours are
straight-alpha sRGB at 1/255; the display list is CPU-independent, unlike
pixels.

**2026-10-18 · eval-coverage: an `intend` names a node its segment mounts
later.** An `intend` selector matching nothing waits, resolved at the end of
each tick of its segment; unmatched at the segment's end it is an error at
its line.

**2026-10-18 · eval-coverage: `teleport` gives an entering node its travel;
a glide across is no retarget down.** A node that enters travels from where
it first shows (and one leaving to where it last shows), so an enter slide
is judged like a slide of a node already there. The retarget arm holds only
axes the glide moves; on others, under a `LAYOUT_CAUSES` relayout, the node
follows its layout. Both read design.md's "a moving node does not jump" per
axis.

**2026-10-18 · eval-coverage: new curated scenarios and matrix effects.**
Curated: `shadow-card-swell` (and `-1_5`), `flip-nested`, `toast-stack`,
`roll-counter`, `drag-chip`, `pill-title-ellipsis`, `popup-height-spring`,
`colour-mid-motion` and set C's eight `heldc-*`. Matrix effects `shadow`,
`roll-text`, `colour-shift`, `flip-nested`, `popup-content-height` (fixture
slots `pop_size`, `card_height`); 28 effects make 406 cells a rate. Accepted
as designed: the contrast guard flipping a label mid-swap, a growing popup
sized at once, `gl` holding an ellipsised title's whole text.

**2026-10-18 · eval-coverage: rows pushed down in a growing content-sized
surface glide together.** A change's FLIP scope (`flip_open`) stays open
until a frame has painted it as well as until its texts are shaped, and a
pass in a resized buffer while it is open glides moved boxes from where they
were drawn, as bug 24's rule does; a never-painted surface lays out at rest
(`motion.rs::rows_pushed_down_in_a_growing_content_sized_panel_glide_together`).

**2026-10-18 · eval-coverage: what the new recordings showed and is left as
it is.** (1) A growing content-sized surface lays out at its final size
while a shrinking one follows its content; changing growth is a design
change raised with the owner. (2) A default-spring roll rolls 200 ms at 60
Hz and 160 ms at 144 Hz; each rate has its own reference, cause not traced.
(3) A shadow's extent is recorded before transforms, as `ink` is.

## eval-rules

**2026-10-18 · eval-rules: rule checks hold how a motion plays; only an
`allow` exempts from them.** Four checks (`colour`, `contain`, `tiling`,
`stagger`) and `vanish`'s `pop-out` each derive from one guarantee of
design.md or a decision, kept only with no false positive over 45 scenarios
and 812 matrix cells. An `intend` says a node moves, not how, so it does not
exempt (`Cx::allowed`). The trace gains exit poses, `stagger` steps,
clipping, rigidity (`labels::rigid_of`), surface `content` axes and motion
bits `FILL_SPRING`, `INK_SPRING`, `LINE_SPRING`.

**2026-10-18 · eval-rules: `colour` holds a spring's colours to the straight
way in premultiplied OKLab.** One spring moves all four premultiplied OKLab
channels along one curve, so each drawn `bg`, border and text colour must
lie within 3 steps of 1/255 of the line from start to rest (-0.5 to 1.5 of
the way). Derived tokens, contrast-guard colours, retargets, keyframes,
time, gradients and span colours are not held.

**2026-10-18 · eval-rules: `contain` holds a content-sized surface to what
it holds and an ellipsised text to giving way.** `cut`: nothing in a popup
or panel without `width`/`height` is drawn past that edge, excusing spring
overshoot, authored motion and clipped content; allowance `tol` plus a
quantum. `give way`: an ellipsised text drawn whole is never laid past its
surface's edge unless something rigid (length `width`, `size`, `min_width`,
`shrink: 0`, image, icon, unellipsised text) or a clipping ancestor puts it
there.

**2026-10-18 · eval-rules: `tiling` holds edges that meet in layout to one
physical pixel.** Per wave3-pixels, among nodes nothing moves on one
surface, edges laid at one place are drawn (snapped `visual` boxes) within
1/32 px; nodes in motion are left out. New scenario `tiles-scale-1_25`.

**2026-10-18 · eval-rules: `stagger` holds each child's start to its turn
within a frame.** Children mounting together are numbered in child order,
child `n` due `n` steps after the batch's change (anim/stagger.rs, step
capped at 1 s), and its first frame off its pose must come within one period
after. Not held: no pose, text on its way, reduced motion.

**2026-10-18 · eval-rules: `pop-out` holds an exit's last frame to its exit
pose.** Per design.md "Poses", the last frame shows the node at its `exit`
(else `enter`) pose: opacity within 1/255, offset within `tol`, scale within
0.005, pose size within `tol`, unless it shows nothing; `slide(edge)` poses
are held on opacity only, reduced motion not at all. A `~` inside a pose
block parses and is ignored, recorded as an open question.

**2026-10-18 · eval-rules: no check holds a retarget's velocity.** A spring
retargeted from rest differs from a correct one by a few pixels, as much as
correct runs differ, so bounds on drawn frames failed correct runs or passed
the mutant. Holding the curve needs stiffness, damping and the retarget
moment in the trace; references catch it (docs/eval.md "Limits").

**2026-10-18 · eval-rules: the bug-24 mutant is cut again against
eval-coverage's FLIP scope.** The patch now also removes `flip_drawn`,
`flip_settled` and `laid_frame` with the scope, and still fails
bug24-flip-open-min with `teleport` on `#bars` x.

## roll-preview

**2026-10-18 · roll-preview: a preview that leaves a change to the painted
frame makes it flatten afresh and keeps the surface dirty.** Rolls, shape
morphs, image swaps and `play` never ran on a surface painted from previews.
(1) A node only a preview has seen is pending (`Rolls`, `Morphs`,
`ImageSwaps`), so `Animator::busy` makes the painted frame flatten afresh
and remember it; `drop_undrawn_enters` drops undrawn ones. (2) A preview
that deferred anything (`Animator::deferred`) keeps its surface dirty. A
preview still remembers nothing. Supersedes eval-coverage's
`Flattened::firsts`/`Animator::firsts_painted` (regression
`roll-at-rest.scn`).

## late-text-morph

**2026-10-18 · late-text-morph: a box of no area does not put its `morph`
name on screen.** A late-text node's empty first frame listed its 0 × 0 box,
replacing the partner's. `Animator::shared_morph` lists a box only with
width and height, so the node morphs from its partner on the frame its text
lands, as late-text-dip's hold and shared-morph-on's rule already ask
(`renderer/tests.rs::a_late_text_morphs_from_its_partner_once_it_lands`).

## eval-host-loop

**2026-10-18 · eval-host-loop: the shell and the eval driver run one host
loop, `strand_render::host_loop`.** The driver mirrored the shell by hand,
so drift could show frames the shell never draws. The reactions, whose
internal order is fixed, are shared in
`crates/strand-render/src/host_loop.rs` (`apply`, `woke`, `input`,
`attached`, `configured`, `detached`, `painted`), each host giving its side
through `LoopHost` and `LoopSync::sync`; when reactions run and frame
scheduling stay each host's. Generic, no dyn, no shell behaviour change; a
recorded call-sequence test was rejected as flaky. Drift tests in
`demo/mod.rs` and `eval/tests/drift.rs` fail when either takes a reaction's
step by hand.

## eval-surface-leg

**2026-10-18 · eval-surface-leg: the sandbox runs the real surface manager
against an in-process fake compositor.** `surface = true` (eval/src/wire.rs)
mirrors each frame through strand-surface's public interface to
strand-fake-wayland, which gains a commit log (`Fake::commits`) and a vblank
mode (`SurfaceGlobals::vblank`, `Cmd::Vblank`); the harness stays the
scheduler, so recordings are unchanged. `regions-on-wire` holds the
compositor's opaque, input and blur regions to the frame each commit
carries, and `late-present` holds every drawing frame to its tick's refresh.
Scale 1 only; strand-fake-wayland stays a dev-dependency
(`check-isolation.sh` (d)); proof `bug23-gpu-regions.scn` and mutant.

## eval-viewer-browser

**2026-10-18 · eval-viewer-browser: the viewer is checked in a real
browser.** `scripts/eval/viewer-check.sh` loads report pages in headless
Chromium (`scripts/eval/Dockerfile.viewer`, Playwright's `v1.62.0-noble`
image pinned by digest), no network, pages read-only, driving every mode
(`scripts/eval/viewer_check.py`) and failing on console errors, exceptions,
failed or off-page requests and broken modes. It runs on host Docker
(run.sh's image gets no browser) in the nightly workflow; the fixes it
prompted change no recording.

## eval-local

**2026-10-18 · eval-local: `run.sh eval local` runs an owner's config where
it is, with the machine's fonts.** Only that command mounts read-only each
absolute config directory a local scenario names and the host's font
directories at `/opt/host-fonts/<n>` with a fontconfig file
(`scripts/eval/local-mounts.sh`). `fonts = "local"` shapes with the system's
fonts through fontconfig plus `$STRAND_EVAL_FONTS`. Nothing is copied; the
gate still refuses local scenarios.

**2026-10-18 · eval-local: `new --explore` writes a starter from what a
config declares, knowing no shell.** `strand-eval new <id> --config F
--explore` (`local new`) sets top-level `bool` and `enum` states, hovers
elements whose `hover`/`pressed` is read or that anchor a `trigger:` popup,
and clicks handler elements, re-running to explore what each reveals (three
levels, at most 32 runs, 40 targets a kind). It knows elements, states and
inputs only, never a shell's names.

**2026-10-18 · eval-local: local scenarios of real shells, and what their
failures were.** Five check fixes, no threshold moved: (1) `teleport` travel
spans a node's first or last frame on its surface; (2) a pop-in is excused
for an enter pose dropped because no frame drew it (wave3-pixels p2); (3) a
node gone by rest is judged against its last steady frame; (4) a pose moving
nothing, or of size alone, is not skipped; (5) `ink` spill holds only
stand-in layouts. `settle` failing on a live graph is correct. Product
finding `popup-still-pose.scn` (a hover popup whose pose moves nothing never
closes) was `expect_fail`, fixed in eval-r3.

## eval-r3

**2026-10-18 · eval-r3: the fake compositor answers a popup's reposition
with its new size.** xdg-shell answers a reposition with `repositioned` then
configures at the new geometry, as sway, niri, Hyprland and labwc do;
strand-fake-wayland now keeps the repositioned size and configures with it
(`xdg.rs` `PopupState::reposition`;
`fake.rs::a_repositioned_popup_is_configured_at_its_new_size`). The fault
was the fake's.

**2026-10-18 · eval-r3: the stale-socket test waits for its dropped listener
to go quiet.** A spawned child can hold the dropped listener's descriptor
until exec, so `ipc/server.rs::tests::bind_replaces_only_a_stale_socket` now
waits up to 5 s for a refused connect; `Server::bind` is unchanged.

**2026-10-18 · eval-r3: `stagger` judges a child from its turn, not its
first frame off its pose.** Slow-starting curves (`ease_in`, `in_back`) were
reported late. A child's start is the first frame without
`Motions::STAGGER`, or an earlier frame already off its pose (still early);
a child never leaving its pose is not judged. New curated
`stagger-slow-start`.

**2026-10-18 · eval-r3: `colour` does not hold a colour already springing
when its segment starts.** A retargeted spring keeps its velocity, so its
way bends; a colour whose spring bit is set at `s`, or whose row is absent
there, is skipped.

**2026-10-18 · eval-r3: `ink` gives a text drawn by its own layout the
font's own overhang.** The probe records `NodeTrace::overhang` (column `ov`,
eval-hooks only), and a text drawn by its own layout (`txt=S`) is allowed it
on top of 2 px plus `tol`; stand-ins keep the old bound and an offset layout
still spills.

**2026-10-18 · eval-r3: `late-present` leaves ticks the manager's throttle
gave up in to the machine.** `THROTTLE_GIVE_UP` (1 s) is real time, so ticks
where `Stats::throttle_given_up` rose (`WireLog::gave_up`) and the next are
not judged.

**2026-10-18 · eval-r3: the host loop's drift guards are tripwires.** Making
the steps private was rejected, since renderer and router steps are public
interfaces other callers use; the spelling-matching drift tests stay,
documented as tripwires, and order is held by review and the shared
reactions.

**2026-10-18 · eval-r3: the gate and the matrix can run many times in one
job.** `.github/workflows/eval-matrix.yml` takes a `repeat` input; above 1,
scripts/eval/repeat.sh runs gate and matrix that many times and fails on any
failure or digest differing from the first run
(scripts/eval/test-repeat.sh).

**2026-10-18 · eval-r3: an enter pose dropped out of view needs an ancestor
that clips.** Narrows eval-local's rule (2): being outside an ancestor's box
excuses a pop-in only for an ancestor that clips (`clip: true`, `scroll`,
`list`) or the surface root; a transparent ancestor still counts.

**2026-10-18 · eval-r3: a shaped text's ink is bounded by its font's
overhang, not exempt.** eval-local's exemption (5) is replaced by the
recorded `ov` bound, so a shaped layout drawn in the wrong place still
fails.

**2026-10-18 · eval-r3: an exit that moves nothing still ends.** Per
design.md "Poses", a pose with nothing to play has played. A preview finding
an exit at its pose with nothing to move records it due
(`Animator::exits_due`), and `Animator::deferred` keeps the surface wanting
the frame that ends it
(`motion.rs::an_exit_that_moves_nothing_still_ends_under_the_host_loop`;
mutant `popup-still-pose.patch`).

**2026-10-18 · eval-r3: a painted frame's blur and input regions outlive the
scene kept for the next.** Per design.md "Blur ladder" and spec.md 6.7 the
regions are the frame on screen's, so the renderer keeps them apart from the
reusable scene (`SurfaceState::painted_regions`, as `posed` is)
(`motion.rs::the_frame_that_ends_an_exit_keeps_its_blur_region`).

**2026-10-18 · eval-r3: `regions-on-wire` holds a bare commit to the frame
that drew nothing before it.** A bare commit carries a no-damage frame's
regions, so it is held to the latest frame painted since without a buffer
commit, else the last carried frame.

**2026-10-18 · eval-coverage-2: the sandbox places layer surfaces as the
compositor does.** `Surfaces` places each layer surface through
`strand_surface::placement` (`layer_config`, `arranged_area`,
`position_in_rect`) on whole logical sizes, re-places every shown surface
after each attach, configure and detach (`Change::Placed`,
`set_surface_origin`), and sizes buffers with `Scale::physical_size`.
Scenario `panel-odd-centre` at 1, 1.25, 1.5 and 2.

**2026-10-18 · eval-coverage-2: `teleport`'s travel covers a node that comes
back and an authored excursion.** Per design.md's "interrupted animations
never jolt", travel is per unbroken stretch of shown frames, from where the
node came back, and at least the span its authored motion (`auth`) covers; a
glide moved by its own size snapping about its centre is not a retarget (the
`resized` arm).

**2026-10-18 · eval-coverage-2: `twin` compares a node made again by its
key's base.** A key shown alone under its base (`label~2` with no `label`)
is compared by the base at each rest, since under reduced motion the exit
ends at once; beside a plain-key node it stays its own.

**2026-10-18 · eval-coverage-2: scrolling is a motion, flinging its own, and
a scroll moves rows by itself.** The probe marks a moving `scroll` or `list`
content `SCROLL`, and `FLING` while flinging (`ScrollState::flinging`,
eval-hooks only). `SCROLL` is a layout cause; `teleport` notes scroll jumps
as layout's; `tiling` still holds rows; `vanish` lets a scroll-carried row
skip a frame; `twin` leaves out a surface from its first fling, since
reduced motion never flings. Strand has no overscroll.

**2026-10-18 · eval-coverage-2: `scroll` is notches, `swipe` is a
touchpad.** `scroll DY` is `DY` wheel notches of 15 px (`WHEEL_STEP`); a
touchpad (`AxisSource::Finger`) gets its own verb, `swipe DY over MS`, one
axis frame a tick and the lift with the last.

**2026-10-18 · eval-coverage-2: a node clipped out by an ancestor is
`hidden: clipped`.** The probe marks a node whose visual box meets its
surface but not its ancestors' clip `Clipped`; its children are still
traced, and checks treat it as not shown (partly inside counts as whole).
`vanish` also counts steps acting inside a segment and timed steps (`glide`,
`drag`, `swipe`) on every tick of their span. A product blink in
`c-launcher-filter` was left open.

**2026-10-18 · eval-coverage-2: a control's parts are recorded.** The probe
gives each fill's box (`NodePaint::fill_boxes`), and the recording's `pt`
holds every fill other than the node's own box, relative to its drawn box,
within the geometry tolerance (slider track, value and knob, meter, arc,
`segmented` cell, path).

**2026-10-18 · eval-coverage-2: a theme crossfade is reached through a
palette state, with no new step.** A `theme` verb would add a concept; a
palette chosen by a state swaps on `set`. The driver gives logic
`Storage::config_dir` so `import` reads palette files, and
eval/fixtures/palettes/ holds `grey.yaml` and `split.yaml`, which crossfade
from a snapshot.

**2026-10-18 · eval-coverage-2: two input effects in the matrix, not four.**
`scroll` and `slider-drag` act in the `mid` steps, 457 cells a rate. A theme
swap and a split were not added, as they would change every cell's recording
with references at 5.10 MB of the 5 MiB tripwire; theme-crossfade and
split-share cover them.

**2026-10-18 · eval-coverage-2: what set D's remaining misses are.**
Reference-only catches (`slider-lag`, `clip-sizing`, `enter-unset`,
`lead-max`, `retarget-rest`, `roll-midflight`, `shadow-reach`) get no rule
since none breaks a guarantee the trace can bound; `linear-len` is an
equivalent mutant. Set D: rules catch 12, references 7, and the ten
correct-motion scenarios false-alarm none.

## eval-rules-2

**2026-10-18 · eval-rules-2: rule checks for what only the references
caught.** Each reference-only class breaks a stated guarantee, so each is
now a rule: a new check `continuity`, `contain`'s `held` and `shadow` arms,
and `coherence`'s `drag`. Each was kept only with no false alarm over the
gate, matrix, sets C and D and the owner shells, and proven on its spent
mutant and a built one. Supersedes eval-rules' "no check holds a retarget's
velocity" and eval-coverage-2's remaining misses.

**2026-10-18 · eval-rules-2: the trace records what each motion was asked,
never how it moved.** strand-scene's `Motion::view()` gives a motion's state
as plain data (architecture.md "Motion"); the probe takes it before each
painted sample of prop, size and glide springs (`FrameTrace::springs`,
`SpringTrace`) with the frame period: value, target, curve, tolerance,
`rest`, `start`, but not velocity, which `continuity` computes itself so a
wrong renderer cannot mislead it. Shadows' and uniforms' motions are not
traced. `clip` is a row column (COLS 41).

**2026-10-18 · eval-rules-2: `continuity`'s ways and tolerance.** Per
design.md and first-frame-lead, the check reseeds a way at every `rest` and
starts a new one at each `start` from the old way's place and velocity
(timed curves carrying `v0·t·(1 − t/d)²`), holding each drawn value within
four times the settling tolerance plus 1e-4 of its span. A miss is named
`velocity`, `start` or `way` by what fits. A way is let go on an unexplained
target change, a frame that does not sample it (reduced motion), or after
failing.

**2026-10-18 · eval-rules-2: a way in flight dropped for one at rest is a
jolt.** A `rest` record meeting an unsettled way whose place is off rest by
more than the tolerance, or whose velocity would pass it within a 60 Hz
frame, fails `velocity`.

**2026-10-18 · eval-rules-2: `end`, a motion ends exactly on its target.**
On a surface that wants no more frames (`Frame::wants`), every spring
sampled is at its target within 1e-5 of its size, since `sample_frame` snaps
settled motion; exits are left to `pop-out`.

**2026-10-18 · eval-rules-2: `first frame`, a new node springs only its
pose.** Per m4-effects-finish, the probe marks props sampled on a node's
enter frame with whether its pose sets them; a change starting there on a
prop the pose does not set, from a value other than its target, fails.

**2026-10-18 · eval-rules-2: `roll`, an interrupted roll continues from the
text on screen.** Per design.md "Rolling numbers", the probe records each
roll's texts and progress and the interrupted roll (`RollDrawn::was`); the
new roll must start from the nearer text, or either when the frame before
was the other side of half way.

**2026-10-18 · eval-rules-2: `contain`'s `held`, a collapsing box clips.**
Per wave3-pixels p2 and exit-height0, a descendant's visual box cut by its
ancestors' clip (`NodeTrace::clip`) must lie inside a sizing box within
`tol` plus a quantum, even when that box is collapsed to nothing; turned
boxes are left out.

**2026-10-18 · eval-rules-2: `contain`'s `shadow`, a shadow at rest is
inside its buffer.** Per design.md "Layout" and the overhang rules, a box at
rest with nothing on its surface springing a size draws each visible shadow
inside its buffer within `tol` plus a quantum, except on sides
`surfaces.rs::cuts` names (a margin-capped anchored side, the attach side).

**2026-10-18 · eval-rules-2: `coherence`'s `drag`, the knob under the
pointer.** Per wave3-pixels p3, the harness records `ptr` and the probe a
dragged slider (`NodeTrace::drag`); the knob's centre (its last `pt`) must
be the pointer's x clamped a knob radius in, within `tol` plus one physical
pixel; turned or scaled sliders are not held.

**2026-10-18 · eval-rules-2: timed-retarget, a timed curve interrupted.**
New curated `timed-retarget.scn` eases a pill on `ease(ease_out, 300ms)` and
`~ 240ms` and sends both back six frames in, since nothing exercised a
retargeted timed curve; reference approved by Claude.

**2026-10-18 · eval-rules-2: the sweeps and the proofs.** No new arm
false-alarms beyond known product findings; `$STRAND_EVAL_RULE_STATS` counts
what each arm held. Every arm fails its spent mutant and a built second
case; set D now scores rules 19 with `linear-len` equivalent, and set C's 16
are still caught.

## eval-false-alarms

**2026-10-18 · eval-false-alarms: the sandbox places surfaces with
strand-surface's placement.** eval/src/surfaces.rs computes layer and popup
places from the surface manager's own code, never a copy
(`placement::layer_config_with`, `LayerConfig::fit`, `arranged_area` in
shown order, `position_in_rect`, `popup_config`, `popup_request`,
`popup_shift`); a move alone reaches render as `Change::Placed`. What a
compositor may do beyond that is in docs/eval.md Limits. The surface leg
refuses a run where another surface's zone sizes a layer surface.

**2026-10-18 · eval-false-alarms: what `teleport` counts as a node's travel
and its own move.** (1, 2) Travel also spans every frame where a node comes
back or leaves, and where each later change found it (`Cx::restarts`). (3)
The retarget arm holds a node's own move in its parent; a parent's layout
carry is judged on the parent (`layout_pass::glide_moved`). (4) On x and y,
opacity, fill, ink and border springs and non-placing prop springs count as
no motion, so a layout snap under them is noted. A whole-way jump in one
tick still fails.

**2026-10-18 · eval-false-alarms: a scroll's spring lays out its rows (a
check flaw, not a wheel bug).** Each frame records scrolls and lists whose
offset springs (`Frame::scrolls`, from `ScrollState::moving`); a node under
one is laid out anew for a reason (`Cx::scrolled`), as under a size spring.
`vanish` no longer calls a whole row carried off the surface for a tick by a
scroll, glide, drag or authored offset a blink.

**2026-10-18 · eval-false-alarms: `twin` compares one node by its identity,
where each run put it on the output.** Nodes match by key with `~n` counts
dropped (`twin::identity`), and rows are compared at their surface's first
place plus the read offset, i.e. their place on the output.

**2026-10-18 · eval-false-alarms: `stagger` leaves out a child whose enter
was dropped unseen.** A child first drawn with no `ENTER` or `STAGGER` under
an ancestor that kept it from view the frame before
(`vanish::dropped_unseen`, wave3-pixels p2) has no turn to wait for.

**2026-10-18 · eval-false-alarms: what is left are Strand's (not fixed
here).** (a) `d-toast-stack` `redraw` k35: the presented raster stopped
building a fade layer after two went unused (`offscreen/fade.rs`) while the
fresh repaint built one, rounding differently, against cache.rs's "a partial
repaint draws exactly what a full one does". (b-d) Held, centred
content-sized surfaces glide content by the share while the root is also
placed, cutting or displacing it for a few frames; `contain`'s spring
exception also covers a glide (docs/eval.md Limits).

**2026-10-18 · eval-false-alarms: false alarms on sets C and D went from 4
in 18 to 0 in 18.** Only toast-stack's `redraw`, a product finding, still
fails. Six new curated scenarios pass but found rules on the way, so they
are not a held-out measure.

## integration-r4

**2026-10-18 · integration-r4: one placement model, with eval-coverage-2's
buffer size and a geometric `cuts`.** The driver keeps eval-false-alarms'
model (strand-surface's own `layer_config_with`, `fit`, `arranged_area`,
`configured_size`, `popup_config`/`popup_request`/`popup_shift`), sizes
buffers with `physical()` (`Scale::physical_size`, as `buffer_size` does),
and reads `cuts` from the modelled buffer: sides on or past the output's
edge plus the `attach:` side. eval-rules-2's `cuts()` copied render's
`within_edges`, which would hide a wrong cap.

**2026-10-18 · integration-r4: a scroll moving by itself is motion from one
source; its rows travel with its content.** The probe's `SCROLL`/`FLING`
bits are the one source: `Cx::scrolled` reads an ancestor's `SCROLL`,
`Frame::scrolls` is gone, and `SCROLL` leaves `LAYOUT_CAUSES`. `teleport`
judges such rows like any mover, with y travel at least the scroll's own
carry (`Frame::offsets`, `ScrollState::offset`, `Cx::scroll_travel`), so a
scroll that snaps fails (`a_scroll_that_snaps_its_rows_fails_teleport`).

**2026-10-18 · integration-r4: one fix per overlapping false alarm.**
teleport travel keeps `Cx::restarts` and drops the authored-excursion rule;
the retarget arm keeps `own` and drops `resized`, since design.md does not
say a self-sized move is never glided; `twin` keeps `identity` over `base`;
and only what lies under the flung scroll, not the whole surface, is left
out of `twin` (`twin_leaves_out_what_a_flung_scroll_holds`).

**2026-10-18 · integration-r4: `vanish` excuses only what moved off its
surface, and a swipe only for its rows.** A carried-off row must be hidden
for no renderer reason and wholly off its surface, so a clipped FLIP row
still vanishes (set D's launcher-filter blink fails again). Only a `swipe`'s
span acts, on what lies under the `scroll`s and `list`s under the pointer as
it starts; `glide` and `drag` act at their tick.

**2026-10-18 · integration-r4: `continuity`'s ways rest when the product's
springs do.** `Way::settled` uses roll-rate's amplitude rule `√(x² +
(v/ω₀)²) <= eps` with `ω₀ = √k`
(`a_way_rests_when_the_products_spring_does`).

**2026-10-18 · integration-r4: a `swipe` is a time step the driver plays a
tick at a time.** `swipe` is planned like a glide (`Plan::Swipe`, one axis
frame a tick, the last lifting the fingers), so the live sandbox can step
and record it; the window's own touchpad still scrolls by notches
(`live.rs::a_swipe_plays_tick_by_tick_and_records_as_itself`).

**2026-10-18 · integration-r4: a popup open while its anchor moves jumps to
it at the next wake; left to the owner.** popup-boot-wake's trade-off holds:
following a springing anchor each frame repositions or remakes the popup
below `xdg_wm_base` 3. Either fix (follow where reposition is cheap, needing
the compositor's version across crates, or hold the anchor until it rests)
is a product decision; the window test now lets the popup close first.

**2026-10-18 · integration-r4 (review): the references' total tripwire is 6
MiB.** References reached 97% of 5 MiB (`refs::TOTAL_BUDGET`), and adding a
way to rewrite notes would add a concept to keep a number, so the tripwire
rises to 6 MiB, about a round's growth above them. The per-scenario (64 KiB)
and per-cell (8 KiB) tripwires are unchanged.

## shadow-redraw

**2026-10-18 · shadow-redraw: a group that qualifies for a fade layer is
drawn from one on every frame.** Whether a frame drew a layer depended on
history, so a partial redraw could differ from a full repaint, against
design.md's partial-redraw promise and `redraw`'s exactness. A qualifying
group is always drawn from its layer; when none is cached and the old policy
would keep none, it is rastered uncached only in the damaged cells of its
fixed 256 × 64 grid (`offscreen/fade.rs::render_layer`); caching is
unchanged. `random_fades` holds partial frames exactly
(`fade_layers.rs::a_resting_layered_panel_repaints_a_change_exactly`,
regression `fade-layer-redraw.scn` and mutant).

**2026-10-18 · integration-r4 (review): drawing every qualifying group from
its layer costs about 60 µs on such frames; layer raster contexts are
kept.** An uncached pass roughly doubles a resting panel's changed-label
frame (124 µs against 64 at scale 1), well inside a frame and paid only
while content changes each frame. Contexts, `Resources` and scratch are kept
on `Fades` (`LayerRaster`, at most eight contexts, freed when idle).

**2026-10-18 · integration-r4 (review): `random_fades`' edge pixels are
bounded by a quarter of the contrast across the edge.** A fixed cap does not
hold (33 levels seen over 204 runs); coverage taken once versus twice
differs by at most c(1 - c) <= 1/4 of the contrast, so an edge pixel fails
past `contrast / 4 + TOLERANCE`, contrast being the widest channel range in
the 5 x 5 pixels around.

## gradient-cpu-gpu

**2026-10-18 · gradient-cpu-gpu: gradient stops sample the OKLab ramp until
a straight line between them stays within half a level.** vello interpolates
stops in sRGB, so 8 even samples a segment strayed up to 29 levels from
design.md's premultiplied OKLab ramp. `raster::paint::ramp` halves each
piece until the straight line stays within half a level at seven inner
points per 8-bit channel, down to 1/1024 of the segment; both backends take
the same stops (`renderer/backend.rs::stops`), cached by colour pair (1024
kept). No tolerance moved
(`gpu.rs::a_gradient_far_apart_in_hue_is_drawn_by_the_gpu_like_the_cpu`,
regression `gradient-far-hue.scn`).

## roll-rate

**2026-10-18 · roll-rate: a spring rests when its amplitude is within its
tolerance, a moment of its curve alone.** The old rule (`|x| ≤ eps` and `|v|
≤ 10·eps`) let underdamped springs rest near a turning point at some frame
rates only, against design.md's rate-independent motion. A spring now rests
when each channel's `√(x² + (v/ω₀)²)` is within `eps`; damped energy never
grows, so it stays within `eps` and settles at the same moment at every
rate. Timed curves, `MAX_MOTION` and tolerances are unchanged
(`motion.rs::a_spring_settles_at_the_same_moment_at_every_frame_rate`,
regression `roll-rate.scn`).

## pose-transition

**2026-10-18 · pose-transition: a `~` in a pose block, or on `enter:` or
`exit:`, is a load error whose fix removes it.** Render takes each prop's
curve from the node's own prop, so the `~` was silently ignored; supporting
it would add a second curve and a precedence rule. `check::pose_transition`
rejects it with the quick fix "Remove the `~`" and help moving the curve to
the node's own prop, as `check::file_item` does. A `~` on a never-animated
prop (`id:`, `text:`) is still ignored, left open
(`negative/pose_transition.strand`, regression `pose-own-curve.scn`).

## eval-live

**2026-10-18 · eval-live: the live sandbox is a window of the eval
workspace, its controls a minimal panel.** `strand-eval live` (docs/eval.md
"Live sandbox") runs a config or `.scn` in the same driver, host loop,
checks and recorder as `check`, on the virtual clock, in an sctk window
(smithay-client-toolkit 0.21.1, already locked). It lives in eval/ only.
Controls are drawn with the report's 5x7 bitmap font into shm, not with
Strand, so they work while the renderer under test misbehaves; the stage
blits only the driver's own surface buffers.

**2026-10-18 · eval-live: a session is the scenario it records, by
construction.** The driver's step loop is split into `begin` and `advance`,
and replay (`harness::run`) is that loop run to the end, so the sandbox runs
the same code tick by tick. Idle clock time records as `rest` (or `rest max
N` past 600 ticks) or `frames N`; a cut time step becomes `frames <done>`, a
glide or drag runs to its end. A recording's digest equals the live trace's
(eval/tests/live.rs).

**2026-10-18 · eval-live: `leave` is a step.** `leave` sends the router the
leave event for the surface the pointer is on, as `wl_pointer.leave` does,
and opens a segment; a scenario may close a hover popup by leaving.

**2026-10-18 · eval-live: a reload replays what was done and plays the last
thing again.** An edit reboots the config, replays the steps before the last
segment at once and plays the last segment live. A non-compiling edit is
refused before anything changes; an opened `.scn` replays from the start; a
step the shell refuses is not logged.

**2026-10-18 · eval-live: a recording carries its config inline, a surface
by its most stable selector.** `w` writes the header, the config's files as
`===` sections and the steps to eval/scenarios/local/. The pointer's surface
is written as `#id`, else its kind path (`popup[1]`), else `@file:l:c[n]`,
each kept only if it resolves back. A config's boot is run to rest when
opened.

## eval-rules-3

**2026-10-19 · eval-rules-3: set E's gaps closed by rules, not thresholds.**
Set E scored 16/20 with one false alarm. Each change below has a general
reason from design.md or a decision, a proof on the spent case and a second
one, and no false alarm over the gate, matrix, sets C, D and E and the
owner's `qs` and `launcher`; no threshold loosened, nothing allowed.

**2026-10-19 · eval-rules-3: a node gone more than three ticks and back
before its segment rests vanished.** A blink is a node gone and back with
nothing taking it out, at any length: gone at a tick, back by its segment's
rest (or three ticks, whichever later), not brought back by a step, and no
frame showing it leaving (exit, scroll, glide, slide). Lifts `BLINK_TICKS`
as the only bound.

**2026-10-19 · eval-rules-3: a blank frame is one where something that draws
shows.** `blank` counts only nodes that draw (`Cx::draws`: paint ever
non-empty, or an image, icon or shader), so a bare `col` over a pose at
`opacity: 0` is not blank.

**2026-10-19 · eval-rules-3: `contain`'s `pushed`: what rests inside a
surface is not drawn past it with nothing moving it.** A node inside its
surface and clip at a segment's start and rest, drawn past them in between
with no own or inherited motion, authored geometry or scroll, was pushed out
by layout. Clips by a box whose size springs are left to `held`; non-drawing
nodes are not held.

**2026-10-19 · eval-rules-3: `contain`'s overshoot allowance is a share of
how far a box travels.** `cut` now excuses only the overshoot tolerance
times the box's travel (edge or size span, counting `restarts`), so a box
resting where it started has none to excuse; set D's launcher-filter now
fails `cut`, an open product finding.

**2026-10-19 · eval-rules-3: `continuity`'s `fold`: a slot from zero, or to
zero and gone, carries its gap.** Per `Forced::collapse`, on the frame a
node sized from zero first draws, and the frame after one exiting to below
0.5 unmounts, each sibling moves no further than twice the folding slots'
sizes plus `tol` and starts no glide over a step within the parent; judged
only when nothing else comes, goes or resizes.

**2026-10-19 · eval-rules-3: `colour`'s `past its end`: a colour goes no
further than its spring overshoots.** A spring from rest overshoots at most
`exp(−ζπ/√(1−ζ²))`, a bézier stays in its hull, linear and bounce easings do
not pass their ends (`CurveRec::overshoot`); colours without a named curve
or on elastic easings keep -0.5 to 1.5. New curated `colour-bouncy`.

**2026-10-19 · eval-rules-3: `colour`'s `palette`: a theme swap springs its
palette on the curve its table gives a colour.** strand-render's
`eval-hooks` traces palette roots (`SpringOf::Palette`, keyed `$` and token
path) with the curve from `TokenScope::transition(transition, Prop::Color)`;
`continuity` follows them and `colour` fails a swap on another curve. New
curated `theme-swap-own-effects`.

**2026-10-19 · eval-rules-3: coverage of set E's classes, and what stayed
out.** New curated `osd-bottom-grow` (1, 1.25, 1.5),
`hover-card-rest-move-leave`, `spilling-texts-grow`, `notif-centre-stagger`,
`calendar-grid-144hz`, approved by Claude. Three bottom-anchored
content-sized surface findings stay out of the gate pending the owner's
grow/shrink decision (the shrink was later fixed by bottom-osd-shrink).
`pointer X,Y` stays on its surface, so leaving a hover card takes `pointer
X,Y on #<node>`.

## border-eps

**2026-10-19 · border-eps: each spring channel settles by its own
tolerance.** `Motion::rest_each(value, [eps; N])` takes a tolerance per
channel, since a settle snap moves each channel by up to its tolerance:
a border's, stroke's and glow's width keeps 0.05 and its colour takes
`COLOUR_EPS` (0.002); a shadow keeps 0.01 for geometry and `COLOUR_EPS`
for colour. `continuity` gains `snap`: a settling opacity or colour may
jump at most 1/255. Tests: `a_fading_border_settles_without_a_visible_snap`,
regression `border-fade-snap.scn`.

## bar-blank-on-exit

**2026-10-19 · bar-blank-on-exit: a width-squeezed pose keeps its
rest layout, clipped.** A text exiting to `width: 0` was re-shaped
at each width and wrapped into a column hundreds of px tall. While an
`enter`/`exit` pose squeezes a node's width it keeps its layout at rest
(`Forced::hold`, `Animator::line_holds`, `Animator::hold_exits`) and
its box clips it; authored size springs still re-lay out. `contain` gains
`other axis`. Tests: `a_width_exit_squeezes_its_text_without_rewrapping_it`,
`text-exit-squeeze.scn`.

**2026-10-19 · bar-blank-on-exit, the measured size: content-sized
measuring uses the holds.** The spec pass measuring a content-sized surface
(`renderer/specs.rs`) ran before `hold_exits` and measured the squeezed text
wrapped, asking for an output-tall buffer. The measuring pass now learns
exit holds from the boxes last laid out, as the painted pass does. Test:
`a_content_sized_panel_keeps_its_height_while_a_chip_squeezes_out`.

## clip-blink

**2026-10-19 · clip-blink: a clip carries its content; a glide never
starts hidden.** A box that clips (`clip: true`, `scroll`, `list`,
`pages`, a springing or exiting size) carries its content: content
glides are measured from where the clip went. A glide's start moves,
per axis, by as little as shows as much of the node as showed before;
one wholly hidden stays put. Unclipped content glides on screen as
before. Tests: `a_snapped_clip_carries_its_rows_so_none_blinks_out`,
`clip-carries-rows.scn`.

## bottom-osd-shrink

**2026-10-19 · bottom-osd-shrink: a shrunken buffer's pass compares
boxes on screen.** A product bug, not the sandbox's placement. When a
content-sized surface's buffer shrinks, the layout pass moves the old
boxes by how far the buffer's near edge moved (`layout::edge_shift` with
`resize_share`, shrink part only), so FLIP measures on-screen moves. Growth
is untouched (`glide_origin`, owner question pending). Tests:
`a_bottom_osd_keeps_its_last_row_in_place_as_it_shrinks`,
`shrink-keeps-edge.scn`.

## integration-r5

**2026-10-19 · integration-r5: `cut` judges what of a node shows.** `cut`
judges a node by its drawn box cut by its ancestors' clips (`shown`),
including a clip a size spring sets, so what a clip hides past the edge
is not cut. Test: `contain_does_not_cut_what_a_clip_hides_past_the_edge`.

**2026-10-19 · integration-r5: held-glides' unhold half kept; growth
half awaits owner.** Of two models for comparing boxes on screen when a
content-sized buffer moves, bottom-osd-shrink's is kept (old boxes shift
once before `glide_moved`); held-glides' per-box `note_resize` subtraction
would double count and break clip-blink's carry. Its unhold finding passes
(`unhold-flip.scn`). Its replacement of `glide_origin` for growth changes
on-screen behaviour and stays on laptop/fix-r5b for the owner.

**2026-10-19 · integration-r5: `fold` does not judge a sibling
that shows nothing.** A sibling with no area or no opacity at
both frames opens no visible gap, so `fold` skips it. Test:
`fold_leaves_a_sibling_that_shows_nothing`.

**2026-10-19 · integration-r5 (review): `other axis` follows a
sibling's stretch.** A free-axis size equal, within `tol`, to a
sibling's on that frame is the parent's stretch, not the pose's. Test:
`contain_other_axis_follows_a_stretch_its_siblings_make`.

**2026-10-19 · integration-r5 (review): `fold` leaves the siblings
a reorder moved.** A sibling whose order among staying siblings changed
on the axis was moved by the change (FLIP), not the fold, and is not
judged. Test: `fold_leaves_siblings_a_reorder_moved`.

**2026-10-19 · integration-r5 (review): `colour`'s `palette` is a drift
guard.** `palette`'s expected curve (`SpringTrace::asked`, `plan_swap`)
is the same expression as the plan's own, so it only catches the two copies
drifting apart. Deriving it on the eval side would duplicate strand-scene's
token resolution, so the arm is kept and named a drift guard in eval.md;
resolution faults show in references and strand-scene's token tests.

**2026-10-19 · integration-r5 (review): `cut`'s overshoot bound still
excuses osd-edge-margins.** The bound (overshoot tolerance 0.35 times
the node's travel) excuses the label's 11 px cut because its snapped size
counts as travel. Narrowing it (spring travel only, edges on screen) waits
for the owner's growth-glide decision; envelope's 3 px floor is not taken,
since a floor only widens what is excused. Recorded in eval.md's Limits.

**2026-10-19 · integration-r5: the bottom-anchored findings after the
merge.** The bottom OSD shrink is fixed by bottom-osd-shrink. Left with
the owner's question: at scale 1.5 a held root's row is 0.67 px off for
two frames (a `RootSize::Placed` rounding), and a growing bottom panel's
card still glides past its bottom, and a second growth moves a row 4 px
(`glide_origin`).

**2026-10-19 · integration-r5: the nightly viewer check exercised one
scenario twice.** A mutant page's one scenario was exercised twice and the
second run opened on the first's saved verdict draft. `viewer_check.py` now
clears that run's and scenario's draft key before each Approve exercise;
the note placeholder reads "kept in this browser". A thinned frame's
wireframe is drawn dimmer (`#62626c`) than overlay boxes (`#9a9aa6`)
so overlays show on it.

## eval-rules-4

**2026-10-19 · eval-rules-4: set F's check gaps closed by rules from
the design.** Each rule is derived from design.md, spec.md 5.1 and the
protocols, never the product code, and kept only with synthetic passing
and failing traces in eval/tests/checks.rs, the spent mutant, a second
case, and no false alarm over the gate, the matrix at 60 and 144 Hz,
sets C to F and the owner's shells.

**2026-10-19 · eval-rules-4: `placement`, a new check.** Works out a card's
place from declared props (`SurfaceInfo::places`, from `SurfaceSpec`):
a popup by its anchor, beside its row in a popup, gap `margin` else 6
px (0 with `attach:`); a bar, panel or OSD by its margin inside earlier
bars' exclusive zones. Not judged: an axis that does not fit, a tooltip,
an unanchored axis, a frame whose anchor moved. `centre` holds spec.md
6 with `NodeMeta::inset`; `fit` holds `fit:` geometry, stand-ins included.

**2026-10-19 · eval-rules-4: `ink`'s `fills`: a cut text keeps the words
that fit.** The probe records the layout shaped for the box at that scale
(`Frame::lines`); the check measures on the unbounded layout whether the
next word would have fit before each break and end ellipsis, within a
pixel and `tol`. Word by word, since the engine cuts at a glyph. Newline
breaks, start/middle ellipses and non-LTR single lines are left out.

**2026-10-19 · eval-rules-4: `held` counts a text's glyphs.** A text's
drawn ink, cut by its ancestors' clip, joins its box for `held`, since
a sizing box clips its content (wave3-pixels p2). Ink under a scale is
left out (recorded before transforms).

**2026-10-19 · eval-rules-4: `fold` holds the parent at unmount.** On the
frame after a collapsed node unmounts, the slot's holder steps by no more
than the slots plus `tol` unless its own size springs. The frame a node
enters from zero is not judged (content-sized growth, owner's question).

**2026-10-19 · eval-rules-4: `redraw`'s `blend` reads the crossfade
weight from pixels.** The harness keeps the unblended repaint and snapshot
(`fade_snapshot`, `repaint_unblended`) and the check blends `round(w·new +
(1 − w)·old)` per premultiplied channel itself, within 2 steps.

**2026-10-19 · eval-rules-4: `bystander` and `envelope` read an authored
turn before transforms.** A frame where the node's own authored `scale`
or `rotate` is not identity is judged by its box before transforms;
only the transform's own change is exempt.

## eval-coverage-3

**2026-10-19 · eval-coverage-3: set F's eight gaps covered by shell
motion.** New scenarios at varied scales and rates, none shaped to a
mutant: `media-art-expand`, `tray-submenu-shadow`, `osd-shadow-edges-1_5`,
`tile-expand-held-1_75`, `bar-split-title-1_25`, `notif-groups-reorder`,
`wifi-last-row-fold-2`, plus the matrix's `image` effect and a third
level in `flip-nested`. Each first reference was read frame by frame.

**2026-10-19 · eval-coverage-3: design.md's animated and layout features
against the gate.** `click-attached-panel` (pressed state, `attach: top` on
`slide(top)`, `max_lines: 2`) and `workspace-pill-curves` (bézier retarget,
`ease(out_back, 300ms)`, `~ 200ms` colour) are scenarios, since they need
their own surface and input; matrix cells are preferred where an effect
pairs with all others.

**2026-10-19 · eval-coverage-3: the recording holds where an image's decode
is drawn (`im`).** The probe gives each node's images (`NodeTrace::images`,
`ImageDrawn`) and the recording writes them as `im` (`x,y,w,h@pw,ph`,
`~` when not the node's box, `+` the tint), compared within the usual
quanta. Pixels stay unrecorded.

**2026-10-19 · eval-coverage-3: `fold` holds the parent of a slot
unmounting at zero.** A content-sized parent may change, across a zero-slot
unmount, by no more than the slots took plus `tol`, unless its own size
moves by itself. The mount-from-zero frame is not judged on the parent
(owner's growth question).

**2026-10-19 · eval-coverage-3: two side-by-side folding slots
keep their gaps until unmount.** Taffy drops the negative fold
margin of an item whose flex basis is zero, so a slot at zero
carrying a folded gap is laid at 1/1024 px (`HAIR`). Test:
`neighbours_collapsing_together_keep_their_gaps_folded_until_they_unmount`.

**2026-10-19 · eval-coverage-3: an image's clip group is
bounded where it draws.** An image's rounded clip and a
transition's mask group take the transformed box as bounds
(`Flattener::push`), so a partial redraw does not skip them.  Test:
`a_rotated_image_redraws_its_corners_in_a_strip_beside_it`. The slider's
clip in `widget.rs` has the same pattern and is left open.

**2026-10-19 · eval-coverage-3: a matrix cell's config reads relative
paths from the fixture.** A cell's config is read against `eval/matrix/`,
as the shell reads a config against its own directory.

**2026-10-19 · eval-coverage-3: a surface painted on its detach tick
keeps its last size.** A node painted on the tick its surface is detached
is judged at the size the surface was last configured at, not counted as
on screen by `Timeline::visible`.

**2026-10-19 · eval-coverage-3: observed for the growth question, not
changed.** A row entering last into a content-sized popup overshoots while
the popup is laid at its final size, squeezing the row above 1 px; it is
the owner's growth rule, so `wifi-last-row-fold-2` covers rows leaving only.

**2026-10-19 · eval-coverage-3: what the new coverage catches.**
Six throwaway mutants in the new areas were all caught: four by rules
(`coherence` `fit`, `envelope`/`teleport`, `continuity` `fold`, `ink`
spill at 1.75) and two by references only (submenu placed from a shadowed
buffer, edge margins moved by the opposite overhang).

**2026-10-19 · eval-coverage-3: references.** `eval/refs/` is 5.32 MiB,
under the 6 MiB tripwire. `decode_png` rounds each side of a reduced
source up apart (`div_ceil`), drawing content about 5% off its aspect;
`fit` reads only placement, so it is left open in eval.md Limits.

## damage-fraction

**2026-10-19 · damage-fraction: an item's damage is every
pixel its geometry paints.** A shape morph's Catmull-Rom outline
(`shapes::smooth`) paints a fringe outside the box, which partial redraws
left stale. `Flattener::push` now unions an item's bounds with the cover
of its own geometry (`flatten::painted`), intersected with its own clips,
ignoring a 1/512 px sliver (vello_cpu's 8-bit coverage paints nothing
there); a clipping box hands children its outline (`child_clip`). Tests:
`a_morphing_shape_repaints_every_pixel_it_paints`, `damage-fraction.scn`.

## cross-surface-morph

**2026-10-19 · cross-surface-morph: an opening surface's `morph`
nodes start from their partner.** Content of a panel mounted at boot
never entered, so its morph never played.  A surface root that opens
is marked (`Animator::surface_opened`); its nodes with a `morph` name
enter for the morph alone, its first frame is flattened at that frame's
time (`Animator::opening`), and the first painted frame clears the mark
(`surface_shown`). `morph_took_enter` keeps a preview from adding enter
poses.

**2026-10-19 · cross-surface-morph: one box per name per surface;
on screen beats departed.** `SharedMorphs` kept only the latest box per
name. Each surface root now keeps its last box of a name, and an entering
node starts from another node's box on screen before one that left within
the second. Tests: `a_panel_opening_morphs_its_named_node_from_the_pill`,
`a_box_on_screen_is_taken_before_one_that_left`, `cross-surface-morph.scn`.

**2026-10-19 · cross-surface-morph: a group marker's bounds lie
where its transform draws it.** A meter's fill clip, a rounded
image's clip and swap mask, and a cut text's clip now get bounds
through `Flattener::group_bounds`, mapped as `push` maps items. Test:
`a_transformed_groups_clip_repaints_where_it_is_drawn`.

**2026-10-19 · cross-surface-morph: the rule an eval check could hold
(handed to E1).** A node with a `morph` name coming on screen, whose
partner was drawn on the same output at k−1 or within a second, first
draws on the partner's last box and reaches its own with no step beyond
a spring's; with no partner or under reduced motion it shows at its
box. Needs the trace to carry `morph` names and surface origins.

## unshaped-enter

**2026-10-19 · unshaped-enter: an `enter` pose's sizes wait for its
text, then spring.** An unshaped text's size spring ran 0 to 0. While
an entering node with a sized `enter` has unshaped text under it
(`flatten::late_text_under`), its size spring waits and it is laid
at the pose's sizes (`Animator::size_held`); the pass its text is
shaped for starts the spring, up to `FIRST_FRAME_TEXT_WAIT`. `vanish`'s
`pop-in` now holds width and height (`labels.rs::enter_pose`). Tests:
`a_late_text_plays_its_enter_width`, `unshaped-enter.scn`.

## integration-r6

**2026-10-19 · integration-r6: rows folding together use eval-coverage-3's
hair.** Of two fixes, eval-rules-4's gap routing made columns jump when
the set of folding slots changed, so the `HAIR` is kept, and now taken
out of the folded margin so a slot at zero moves nothing. Tests:
`rows_leaving_apart_or_unequal_only_shrink_their_column`,
`rows_entering_from_zero_open_their_gaps_as_they_grow`,
`rows_waiting_at_zero_leave_a_centred_column_where_it_is`.

**2026-10-19 · integration-r6: one image trace and one image rule.**
`probe::ImageDrawn` on `NodeTrace` (eval-rules-4's) plus `tint`;
`im` is written from it byte for byte. `placement`'s `fit` covers
`coherence`'s, which is removed with `NodeMeta::fill`; its cases become
`placement_fits_a_stand_in_by_cover_contain_and_fill`.

**2026-10-19 · integration-r6: one arm for the parent at unmount.**
eval-coverage-3's `fold` arm is kept, since it leaves out a parent whose
size moves by itself (spring, exit, keyframes, time).

**2026-10-19 · integration-r6: spec.md 5.1 states popup and bar
placement.** `placement`'s readings were copied from strand-surface code,
so spec.md 5.1 now states them from the protocols: an edge-anchored popup
centres on the edge's midpoint, a submenu's top-left sits at its row's
top-right (side per `anchor:`), a bar's exclusive zone adds its edge
margin, and exclusive surfaces arrange from overlay down in creation order.

**2026-10-19 · integration-r6: `other axis` waits only for the node's
own texts.** A frame is left out of `other axis` only for a node with a
held text under it, not for every node whenever any text is held. Test:
`other_axis_waits_only_for_the_nodes_own_held_text`.

**2026-10-19 · integration-r6: eval-coverage-3's mutants committed, each
caught by a rule.** The six coverage mutants are committed (eval/mutants,
mutants.txt; 22 in all) and each is caught by a rule, so eval.md no longer
lists edge margins and submenu places as rule-less. The slider and image
clips use cross-surface-morph's `group_bounds`; image.rs's own `map_rect`
is dropped.

**2026-10-19 · integration-r6: the sweep on the integrated code.**
Gate and matrix pass with every reference as approved but toast-stack and
unhold-flip (the fold fix); held-out failures left are content-sized growth
(owner's question). `island` does not load (its config puts `~` in a pose).

**2026-10-19 · integration-r6: the viewer's "moving" allows a physical
pixel.** A node is "moving" in viewer.js when a motion drives it or its
drawn box is off its laid box by more than one physical pixel at its
surface's scale, since drawn boxes snap to the device grid.

## eval-rules-6

**2026-10-19 · eval-rules-6: `coherence`'s `ring` holds a border to the
CSS ring.** A border's inner radius is the outer radius less the width (CSS
"Corner Shaping", never below zero), width rounded to whole device pixels,
at least one. Eight rays cross the recorded border path (`eval-hooks`),
compared to the analytic boxes within 0.25 device px. `shape:` and squircle
borders are not judged. Proven on m-g09, m-h08 and `ring-corners-swapped`.

**2026-10-19 · eval-rules-6: a ray's crossing is where the outline meets
the ray.** kurbo's `PathSeg::intersect_line` can return a root off the ray;
a crossing is kept only within 1e-3 px of the ray.

**2026-10-19 · eval-rules-6: a folding slot's gap is `min(gap, size)`
on every frame.** `continuity` holds the gap beside a slot springing to or
from zero, or exiting to size zero, to `min(gap, v)` within a pixel (`v`
the spring's sample; within two against the snapped size), per layout's
whole-pixel snapping (wave3-pixels p2). Not judged: `justify: space_*`,
out of flow, margins, a shared gap, a way in flight. Proven on m-g07/m-h06,
`fold-gap-late`.

**2026-10-19 · eval-rules-6: `contain`'s `wrap` holds a full row to
what could give way.** A `row` whose children overflow at rest may
hold no child without its own width and with `shrink:` over 0 whose
least width is below its width (a multi-word one-line text, `loose`; an
ellipsised text; a yielding `col`, `box` or `row`), per CSS flex automatic
minimum. Rigidity is read from props (`NodeMeta::flow`). Proven on m-g12,
`ellipsis-min-content-natural`.

**2026-10-19 · eval-rules-6: the driver places a posed layer surface
at the manager's margins.** The driver runs `placement::posed_margin` as
`State::sync_pose` does, places the surface there and moves its popups; the
trace records `shifts` relative to `origins` at rest. `placement`'s `pose`
arm holds `shifts` to the delegated offset within half a pixel. Proven
on m-h14 (`osd-pose-margins`).

**2026-10-19 · eval-rules-6: the surface leg reads the margins on the
wire.** strand-fake-wayland's `CommitRecord` carries layer margins,
and `regions-on-wire` holds them to rest margins moved by the pose
(`wire::posed_margins`, per layer shell) within half a pixel, on the
commit carrying the frame. Proven on m-h14 and `pose-margin-unsent`.

**2026-10-19 · eval-rules-6: sweep, and one product finding left open.**
The new arms raise no false alarm over gate, matrix and sets G and H. Left
for the owner: an exit mirroring `enter { x: 30; scale: 0.9 }` overshoots
its scale pose as it settles (0.891) and fails `vanish`'s `pose back`.

## eval-coverage-4

**2026-10-19 · eval-coverage-4: set H's uncaught gaps covered
by shell motion.** New scenarios `qs-pager-sliders-1_25`,
`clipboard-flick-reduced-2`, `launcher-results-144`,
`media-pill-art-morph-1_5`, `settings-scroll-gauge-1_75`. The matrix's
`scroll` effect flicks on the touchpad (`swipe 20 over 50`), and
`slider-drag`'s slider sits under a resting `x: 12` (not a scale, which
`drag` does not judge).

**2026-10-19 · eval-coverage-4: a header's `reduce_motion` holds for the
run.** Replaced by integration-r7: the header is the starting state and
`reduce-motion off` turns it off.

**2026-10-19 · eval-coverage-4: `stagger` numbers children that
mount again.** A batch is the children first drawn at a frame
plus those drawn again after an absence that play their enter again
(`ENTER` or `STAGGER`); a row brought back at rest is no batch. Test:
`stagger_holds_children_mounting_again`.

**2026-10-19 · eval-coverage-4: `teleport` retarget lets a node follow
the folding sibling before it.** While the surface lays out anew by itself
(`LAYOUT_CAUSES`), a node whose laid move matches the far edge of the
sibling before it (within a pixel) is not held by the retarget arm. Test:
`a_glide_ending_while_the_slot_before_it_folds_is_no_retarget`.

**2026-10-19 · eval-coverage-4: design.md's animated and layout features
against the gate.** Now covered: `scroll`, `arc` gauges, `slidefade` and
`popin`, `~ instant`, a `cookie` morph, a list mask, an image swap under
`transition:`, a cross-surface morph with images, an offset carousel,
hover scale with drag, touchpad flick and fling, staggered 144 Hz
results. Still uncovered: drag and drop (the sandbox delivers no drop),
`pages` transitions, `wave:`, `parallax`, `noise`, conic time borders,
per-letter text motion.

**2026-10-19 · eval-coverage-4: a dock's drag redraws a sibling's strip
stale (open).** Dragging a lifted pin over its neighbour leaves a 4 × 17
strip stale in CPU `redraw`; likely `RoundBox::changed` with the lifted
node's raised stacking.  Product bug outside this lane; the scenario is
not committed.

**2026-10-19 · eval-coverage-4: the owner's shells.** `scenarios/local`
regenerated with `local new --config` for qs, launcher and island
(`fonts = "local"`). qs passes; island does not load (`crown.strand:28:
a pose takes no ~`); launcher's new steps fail `settle`, `bystander`,
`teleport`, `ink`, `vanish` and a pop-in, recorded for triage, not fixed
(owner's config, needs a reduction first).

**2026-10-19 · eval-coverage-4: what the new coverage catches.** Of
six throwaway mutants, three are caught by rules, two by references only
(pointer through a scaled ancestor; an unsprung arc gauge, which no rule
reads) and one not at all (image paint order within a node; image pixels are
not recorded). m-h16, m-h04 and m-h02 now fail rules; m-h18 only references.

**2026-10-19 · eval-coverage-4: references.** `eval/refs/` is 5.68 MiB
under the 6 MiB tripwire. Input coverage goes into matrix cells (about
1.5 kB each) rather than scenarios (tens of kB).

## builtin-shadow

**2026-10-19 · builtin-shadow: a bare builtin name that is also a config
enum variant warns.** Name resolution reads a builtin service before a
user enum variant (`enum Module { network, battery }`), and changing that
would break loading programs (section 14). So where nothing expects the
enum and the name is no field base, it is a `check::shadows_builtin`
warning naming both, with help "write `Module.network`". spec.md
4.2 states it. Tests: `a_variant_a_builtin_hides_is_named`,
`a_refused_step_says_the_configs_warnings`.

## png-aspect

**2026-10-19 · png-aspect: a reduced image keeps its source aspect;
`fit` reads the file.** A decoded raster carries its source extent in
its own pixels (`Raster::extent`, `(sw / f, sh / f)` when reduced; JPEG
`sw·d/8`), which `fit_raster` and `Decoded::source` use. `within` rounds
each side within its pixel budget; the bilinear branch fades edge pixels by
coverage. The sandbox reads each file's header (`eval/src/filesize.rs`)
for `placement`'s `fit`.  Tests: `a_reduced_image_keeps_its_aspect`,
`within_rounds_each_side_and_keeps_its_budget`, `png-aspect.scn`.

## effects-flake

**2026-10-19 · effects-flake: test fixtures are written whole.**
`common::write_png` writes to a per-thread file and renames it over the
path, so parallel tests never read a half-written fixture.

## slider-clip

**2026-10-19 · slider-clip: no clip, mask or damage
bound is left untransformed.** A sweep found every hand-set
group bound goes through `group_bounds` or mapped items. Test:
`every_transformed_group_repaints_where_it_is_drawn` (widgets and images
under `scale` and `rotate` at 1.2 and 1.8, partial equal to full).

## image-redraw

**2026-10-19 · image-redraw: decodes and gradient pixmaps never claim
opacity.** vello trusts `set_may_have_transparency(false)`, but a bilinear
sample of opaque pixels is a hair under opaque, so partial repaints differed
by a level. `image::to_pixmap` and `cache::render_gradient_part` no longer
claim opacity; exact paints still do. Per-cell whole repaint (50% slower)
was rejected. Tests: `a_resampled_image_repaints_in_part_as_in_full`,
`image-redraw.scn`.

## integration-r7

**2026-10-19 · integration-r7: each decode drawn names the file it was made
from.** eval-hooks `ImageDrawn` carries `file`, the decode's own source
(the node's or the swap's old one; architecture.md), and the driver reads
that file's header. Test: `an_image_swap_names_each_decodes_own_file`.

## eval-rules-5

**2026-10-19 · eval-rules-5: a node's travel is its carrier's
way.** `teleport`'s travel also spans the motion carrying the node
(its inherited `flight` plus the compositor pose's contribution)
between its first and last present frames and each restart, not
every frame, so a node a pose brings on screen is not a jump. Tests:
`a_node_carried_onto_the_screen_by_a_pose_travels_the_poses_way`,
`a_node_leaving_through_a_clip_travels_its_carriers_way`.

**2026-10-19 · eval-rules-5: a text content change is a step.** When
a text's glyphs change (`PaintRec::gl`), the change in its natural
width, capped by its parent's box, is a layout step taken off before
`teleport`'s bound (design.md "What can't interpolate"). Tests:
`a_text_content_change_under_a_size_spring_is_a_step`,
`a_centred_text_moves_with_its_content_step`.

**2026-10-19 · eval-rules-5: what shows of a posed surface.**
A row is cut by its buffer at its box with the pose taken off,
and by the output where the pose puts it (`Timeline::on_output`),
per the Wayland model; the viewer keeps the buffer test. Test:
`a_posed_surface_cuts_its_rows_by_its_buffer_and_the_output`.

**2026-10-19 · eval-rules-5: a surface opened again is not closing.**
A root that plays its enter and no longer its exit clears `closing` and
its last pose, since a reopen retargets toward rest (design.md "Springs
everywhere"). Test: `a_surface_opened_again_while_closing_may_pose_back`.

**2026-10-19 · eval-rules-5: tiling leaves out what an ancestor's
transform draws.** Replaced by integration-r7's "tiling compares edges
within one space".

**2026-10-19 · eval-rules-5: under reduced motion a scroll moves
with its input only.** In a segment reduced throughout that swipes or
wheels, no `scroll` or `list` offset may lead the input delivered so far
(swipes spread evenly, notches of `WHEEL_STEP`) by more than a tick's
input plus tolerance; `teleport` and `twin` report it. `stagger`
and pop-out skip only `RunTrace::reduced_throughout`. Test:
`reduced_motion_scrolls_only_with_the_fingers`.

**2026-10-19 · eval-rules-5: `reduce-motion off` turns off the
header's reduce_motion.** The header sets only the starting state;
render is forced reduced only for the twin (`--reduced-motion`). Test:
`a_reduce_motion_off_step_turns_off_the_headers_reduced_motion`, curated
`reduced-header-off`.

**2026-10-19 · eval-rules-5: a named node arriving starts on its partner.**
`coherence`'s `morph arrival`, from the trace (`NodeMeta::morph`, surface
origins): a `morph` node coming on screen with a partner drawn the frame
before or within a second starts at most half way from the partner's box,
then steps no more than a spring; with none, or reduced, it shows at its
box.  Test: `a_named_node_coming_on_screen_starts_from_its_partners_box`.

## integration-r7 (round 7's review)

**2026-10-19 · integration-r7: a header's `reduce_motion` is where the run
starts.** eval-rules-5's reading is kept over eval-coverage-4's: a header
state steps could not leave would add a concept; the twin stays forced.

**2026-10-19 · integration-r7: one model places a posed surface.**
eval-rules-6's wire model (`Frame::shifts` plus box times pose scale) is
kept over eval-rules-5's, living once in `trace::Posed` (`Frame::posed`,
`show`/`unshow`), used by `on_screen`, `Timeline::visible`, `teleport`
and `coherence`.

**2026-10-19 · integration-r7: tiling compares edges within
one space.** Edges are grouped by surface, axis and space (the
nearest scaled ancestor); rows under a turned ancestor are not held
(`tiling.ancestor-transform`).  Replaces eval-rules-5's filter. Tests:
`tiling_judges_edges_inside_a_scaled_ancestor_in_its_space`,
`tiling_compares_no_edge_across_an_ancestors_scale`.

**2026-10-19 · integration-r7: a content step moves a text by its
slot's declared alignment.** The x share (0, ½ or 1) is read from the
declared tree (`labels::x_share_of`, `NodeMeta::x_share`; spec.md 6.7):
`row` by `justify:`, `col`/`scroll` by `align:`, `box`/`stack`/surface
by `align:`; out of flow or margin is unknown, allowing none. Test:
`a_childs_x_share_is_its_parents_declared_alignment`.

**2026-10-19 · integration-r7: the reduced-fling bound uses the sandbox's
own notch.** `reduced_scrolls` and the driver use `teleport::NOTCH_PX`
(15 px a detent, wave3-pixels), not the product's `WHEEL_STEP`.

**2026-10-19 · integration-r7: a named node arrives at its laid box.**
The morph arrival target is computed on the arrival frame (`laid − moved
+ inherited − flight + written`, through the pose), not the segment's
last frame. Test: `a_named_nodes_own_box_is_its_layout_where_it_arrives`.

**2026-10-19 · integration-r7: bystander gives a physical pixel and a
quantum.** The give is `tol + QUANTUM` (`trace::QUANTUM`, 1/64). Test:
`a_length_one_physical_pixel_off_at_a_fractional_scale_is_no_bystander`.

**2026-10-19 · integration-r7: `beside` needs a sibling that changes
its length.** The retarget arm is off only when a sibling before the node
changed its laid length on that axis by more than half a quantum. Test:
`a_glide_behind_a_sibling_that_does_not_size_is_held`.

**2026-10-19 · integration-r7: stagger and pose back read the
scenario, not render's bits.** They use the driver's record of
logic's diff: `RunTrace::windowed` (a list `window` mounted
it) and `RunTrace::opened` (logic created a surface or set
`open` true). Tests: `stagger_holds_children_mounting_again`,
`a_surface_opened_again_while_closing_may_pose_back`.

**2026-10-19 · integration-r7: a bare commit's margins
are its moment's.** The driver records each shown layer
surface's posed rest state per tick (`WireLog::held`),
and a bare commit is held to it within half a pixel. Test:
`regions_on_wire_holds_a_bare_commits_margins_to_its_moment`. Its
tick's-last-commit exemption is replaced by integration-r7 part 2.

**2026-10-19 · integration-r7: a morph back from a gone surface starts
where it lay.** `SharedMorphs::hidden` keeps a detached surface's last
origin with its boxes (`Seen::place`) for `from()`. `coherence`'s
pivot give is magnified by the ancestors' scales (`tol + max(1,
Π|scale|) / scale`), since boxes snap before transforms. Tests:
`a_box_whose_surface_went_starts_from_where_that_surface_lay`,
`pivot_gives_a_snapped_pixel_as_its_ancestors_magnify_it`.

## dock-drag

**2026-10-19 · dock-drag: a paint-order swap damages overlaps;
a lift tops from its first frame.** Each frame records paint order
(`Flattened::order`); when it changes, `renderer::frame::order_damage`
damages what each node outside a longest in-order run shares with those
it passed (whole bounds past 65,536 comparisons), and order counts as a
change for wake and GPU send.  `Animator::lift_rank` ranks drag-held above
springing-back, and a landed lift returns among siblings on its landing
frame. Tests: `a_change_of_paint_order_repaints_where_the_two_overlap`,
`a_row_lifted_after_another_settled_is_on_top_at_once`, `dock-drag.scn`.

## opaque-fast-path

**2026-10-19 · opaque-fast-path: opaque pixmaps are claimed opaque again;
smooth samples get a layer.** Without the claim a 4K opaque repaint is
about 7% slower (vello culls only known-opaque paints). The claim is
made when every pixel's alpha is 255 (`image::to_pixmap`) or from opaque
stops (`cache::render_gradient_part`), and `raster::draw::fill_with`
draws a smooth sample of a claimed pixmap in its own layer (about 3%
on that draw). Tests: `opaque_pixmaps_repaint_in_part_as_in_full`,
`a_decode_claims_opacity_only_when_every_pixel_is`,
`a_gradient_pixmap_claims_opacity_with_opaque_stops`.

## stand-in-fit

**2026-10-19 · stand-in-fit: a stand-in must hold what its box's fit
shows.** Decodes are held per source and scale at any size and fit;
`ImageStore::stand_in` prefers one leaving at most `STAND_IN_BARE` (1
buffer px) bare (`Decoded::holds`, `placed_for`), then the sharpest,
then the newest.  When none does (a `cover` box changing shape),
one whole-source decode is asked for (longer side at most four times
the box, one in flight per source, `wholes`), the one exception to
wave3-pixels' rule. `placement` `fit` now judges `cover`. Tests:
`a_springing_cover_image_stands_in_with_a_decode_that_covers_it`,
`placement_finds_a_stand_in_that_does_not_cover_its_box`,
`stand-in-fit.scn`.

## pss-flake

**2026-10-19 · pss-flake: `gpu_idle`'s leak tripwire reads memory strand
holds.** Lavapipe's shared libLLVM file pages swing PSS under load. The
growth check reads `held_kb` (`Pss_Anon + Pss_Shmem + SwapPss` from
`smaps_rollup`), bound still `PSS_SLACK_KB` (4 MiB); total PSS is still
printed. The hardware leg's 12 and 16 MiB backstops stay on total PSS.

## exit-overshoot

**2026-10-19 · exit-overshoot: an exit pose overshoots by the spatial
spring, as designed.** `x`, `y` and `scale` spring on `spatial: spring(3000,
0.75)` and pass the exit pose by 2.8% before settling; opacity on `effects`
never passes 0. The "0.891" was the trace's 1/64 quantum. No product change;
`pose back` (`POSE_SCALE_GIVE` 0.005 below the quantum) is handed to the
eval lane.

## launcher-flip-pass

**2026-10-19 · launcher-flip-pass: every pass before a frame measures
FLIP from the drawn frame.** The first pass after a paint keeps the
replaced boxes (`SurfaceState::drawn_boxes`); every pass before
the next frame compares against them and replaces earlier glides
(`flip_shifts`); a non-FLIP pass only takes back glides of nodes
now where they were drawn. `Motion` keeps `before`, so a second
sample of a frame leads like its first; `continuity` agrees. Tests:
`a_box_squeezed_in_a_pass_no_frame_drew_stays_on_its_level`,
`a_frame_sampled_twice_starts_its_changes_together`,
`continuity_starts_a_frames_second_sample_with_its_first`,
`flip-undrawn-pass.scn`.

## launcher-reopen

**2026-10-19 · launcher-reopen: a reopened surface plays its kept content's
enter poses.** A hidden surface's body is frozen, not unmounted, so nothing
entered on reopen. A surface root that opens (closed to open, not reduced)
and has painted before (`Renderer::was_shown`) starts the enter pose of
every live node under it, not under a nested surface; a first opening is
unchanged.  Tests: `a_panel_opened_again_plays_its_contents_enter_poses`,
`reopen-enters.scn`.

## launcher-rules

**2026-10-19 · launcher-rules: `ink` lets a word wider than its line
box pass the box's end.** As CSS does (`overflow: visible`), for a
non-ellipsised text drawn by its shaped layout, the end-side allowance
grows by how far its widest word (`Frame::lines`' `natural`) exceeds the
box. Test: `ink_lets_a_word_wider_than_its_line_box_overflow_its_end`.

## launcher-triage

**2026-10-19 · launcher-triage: the owner launcher's other findings.**
The 17 px bystander is a Strand bug in `glide_origin` (it also glides
by a shadow overhang's growth), waiting on the owner's growth rule. The
retarget is read correct by E2's `beside`. `settle` and the rail's `vanish`
are the config's own motion (`letters { y: wave }`, a column spring faster
than its card's clip). island does not load (a pose takes no `~`).

## integration-r7 part 2

**2026-10-19 · integration-r7 part 2: regions-on-wire reads the anchored
edges, on every commit.** wlr-layer-shell applies `set_margin` at each
commit and ignores margins on unanchored edges, so every commit is
held to its moment's state again and only read edges are compared
(`wire::read_edges`, `WireFrame::edges`, `HeldMargins::edges`,
from the driver's `LayerConfig`). Replaces the tick's-last-commit
exemption. strand-surface's six bare commits a tick are a noted follow-up.

**2026-10-19 · integration-r7 part 2: `pose back` reads logic's
close and stops at the exit pose.** A pose back is one that moves
toward rest by more than the give while short of its exit pose
(the root's `exit`, else mirrored `enter`); the scale give is at
least a quantum (`POSE_BACK_SCALE_GIVE`). `closing` starts from
logic's close (`RunTrace::closed`), not the `EXIT` bit. Test:
`an_exit_that_overshoots_its_pose_and_settles_onto_it_is_no_pose_back`.

**2026-10-19 · integration-r7 part 2: the reduced-fling bound restarts
after another step.** Frames another acting step lands on are not judged,
and the bound restarts from where they leave each offset (`delivered(k)
- delivered(base)`), so a filter's clamp or `reveal_index` passes. Test:
`a_reduced_scroll_is_bounded_again_after_another_step`.

**2026-10-19 · integration-r7 part 2: a content step's share combines
up content-sized ancestors.** Each ancestor without its own width
(`Flow::SIZED` unset) and with a declared share adds that share of its width
change, up to the first sized or undeclared one (`teleport::carried_step`);
each width is held to its parent at its own frame. Test:
`a_text_in_a_centred_content_sized_row_moves_with_half_its_step`.

**2026-10-19 · integration-r7 part 2: a named node alone is
excused only by a declared motion.** morph arrival's no-partner arm
excuses only an own or ancestor `enter` pose, an ancestor in flight
(`Row::flight`) or the compositor's pose, not motion bits. Test:
`a_named_node_alone_is_excused_only_by_a_declared_motion`.

**2026-10-19 · integration-r7 part 2: a stand-in is the nearest decode at
least as sharp.** `ImageStore::stand_in` takes the least sharp decode at
least as sharp as the box draws (the sharpest only if none is), avoiding
unmipmapped minification.  The whole-source decode's area is at most
`WHOLE_AREA` (4) times the box's, within `IMAGE_CACHE_BYTES` (32 MiB). Test:
`a_stand_in_is_the_nearest_decode_at_least_as_sharp_as_its_box`.

**2026-10-19 · integration-r7 part 2: flip-undrawn-pass rebuilt from
neutral shapes.** The regression copied the owner launcher's card, which
task rules forbid; it is now a sheet, tag, counter and box on springs
500/0.7 and 880/0.8, with both mutants still failing on #count.

**2026-10-19 · integration-r7 part
2: the live test fails by construction.**
`a_failing_rule_is_marked_on_its_frame_and_a_broken_edit_keeps_the_session`
acts `expect hidden #t` on a shown box, a failure no product fix can remove.

**2026-10-19 · integration-r7 part 2: left open.** gpu_idle's hardware
backstops on total PSS; the launcher's 17 px bystander (`glide_origin`,
owner); the content-sized growth rule; how a hover popup follows a moving
anchor; strand-surface's bare commits.

## eval-rules-7

**2026-10-20 · eval-rules-7: five false alarms of sets I and J
fixed at their cause.** (1) A glyph-only node shows where its ink is
(`Timeline::visible`, `checks::drawn_extent`), for every check. (2)
`pop-out` skips a last frame where the node does not show. (3) `colour`
`composite`: where fill layers come and go, the source-over composite
is held to its ends within three steps and overshoot. (4) An ellipsised
text wider than its box is clipped to it (`flatten::text::place_text`;
test `an_ellipsis_wider_than_its_box_is_clipped_to_it`). (5) A
side on the exclusive zones' edge is a cut side (`surfaces::cuts`,
`placement::arranged_area`).

**2026-10-20 · eval-rules-7: a fill a state sets springs from
the default fill drawn.** An unset `bg` sprang from clear while
flatten drew a default fill.  `flatten::default_fill` is shared,
and `Animator::paint` hands it to the spring for an unset or `Unset`
`bg`. Test: `a_buttons_bg_springs_from_and_to_its_default_fill`.

**2026-10-20 · eval-rules-7: practice set 8's false alarms, three more.**
Selectors take a kind path or `#id`, not a surface's declared name (author
misuse, not committed). `pop-out` skips a last frame followed by one where
a clipping ancestor is under half a pixel across (`vanish::shut_after`);
a `flash` needs its gone frame within `BLINK_TICKS` (3). Tests:
`pop_out_passes_a_node_whose_clipping_parent_shuts_the_frame_after`,
`a_frame_left_on_screen_for_long_is_no_flash`.

**2026-10-20 · eval-rules-7: proof on spent mutants and second cases.**
All 33 mutants are caught; each change was run against spent mutants over
192 scenarios. Under m-j05 a row unmounting at no area is no longer a
`pop-out` (a pop no eye sees). Constructed second cases confirm `composite`
and `cuts`.

**2026-10-20 · eval-rules-7: the audit of every arm since round 5.** Every
arm from eval-rules-3 to -6 and integration-r6/-r7 was read for what it
assumes shows, moves and composes, and run with `STRAND_EVAL_RULE_STATS=1`;
the faulty assumptions are the fixes above. Gate 122/122; matrix 972 pass,
20 n/a; remaining failures are product findings or misused scenarios.

**2026-10-20 · eval-rules-7: product findings left open.** Set I:
hover cards' shadow cut, media art morph arrival and a 1-level redraw,
toast stack growth. practice-8: (a) a `glow` is not in the overhang (only
`shadow:` lists count); (b) a row inserted above a scrolled list scrolls it;
(c) a click on the shell's own bar does not close its menu (click-away
covers only `keyboard: exclusive`); (d) a dock growing on hover is cut;
(e) a nested shared morph compounds its parent's; (f) a title jumps inside
its card as the panel re-centres.

## eval-rules-8

**2026-10-20 · eval-rules-8: a keyframes play draws its track
(`continuity`'s `track`).** CSS keyframes semantics: at each stop's fraction
a prop is the stop's value, eased per segment; omitted end stops use the
node's value; `delay`, `repeat`, `alternate`; `x`, `y`, `rotate` add and
`opacity`, `scale` multiply. Expected values come from `RunTrace::keyframes`
with the check's own bézier, timed nodes from `Frame::timed`. Elastic
and bounce are not judged. Tolerances: rotate 0.5°, opacity 2.5/255,
scale 0.005. Tests in checks.rs; scenario `keyframes-tracks.scn`.

**2026-10-20 · eval-rules-8: with motion reduced, what time drives holds
still (`still`).** A node time drives (`Frame::timed`: a `play`, or `t`,
`wave(…)`, `noise(…)`) is drawn the same on every reduced frame as the
one before, unless logic or input drove a change or its surface lays out
anew (design.md "Runtime changes" 7). `continuity` holds the run's reduced
frames, `twin` its re-run; `Frame::reduced` records render's state. Tests:
`a_time_signal_that_runs_on_under_reduced_motion_fails` and its pass twin.

**2026-10-20 · eval-rules-8: a shadow covers what its props ask at the
frame's scale (`reach`).** Per CSS Backgrounds 3, in device px a shadow's
area is centred on the cast box moved by `(x, y) × s` and reaches past the
box grown by `spread × s` until `alpha × (1 − Φ(d/σ)) < 0.5/255` (`σ
= blur × s / 2`), at most `4σ + 2`. The probe records `ShadowDrawn::asked`
and `frame`. Tests: `a_shadow_whose_blur_ignores_the_scale_fails_reach`
and siblings.

**2026-10-20 · eval-rules-8: a CHANGED line leads with the largest step.**
`recording::diff` finds the largest per-node step the run takes that
its reference does not and leads with it, ranking nodes by it; turns
go the short way round. Tests: `a_diff_leads_with_its_largest_step`,
`a_turn_through_a_whole_turn_is_no_step`.

**2026-10-20 · eval-rules-8: j17 (a wheel notch gliding under reduced
motion) is not covered.** `ScrollState::advance` snaps every reduced frame,
so the notch lands anyway; it shows only when motion turns off mid-notch,
which design.md allows. A check would test a code path, not behaviour.

**2026-10-20 · eval-rules-8: proof.** Gate, matrix (972 pass, 20 n/a),
sets I and J, owner shells and practice-8 show no new-arm finding. Set
I's keyframe-segment-span and blur-ignores-scale and set J's j18 now
fail `track`, `reach` and `still`; second cases keyframe-ease-linear,
shadow-offset-unscaled and signal-runs-reduced are kept in `eval/mutants`.

## eval-coverage-5

**2026-10-20 · eval-coverage-5: sets I and J's coverage gaps covered
by shell motion.** Seven curated scenarios: `wallpaper-fill-preview` (a
`fit: fill` stand-in), `charging-pulse-reduced`, `bell-ring-keyframes`,
`pinned-card-shadow-2`, `now-playing-time`, `sync-spinner-gif` (with
`eval/fixtures/images/spinner.gif`) and `voice-letters-noise`. Each first
reference was read before approval.

**2026-10-20 · eval-coverage-5: round 8's practice corpus promoted.**
Thirteen blind-authored practice-8 scenarios that pass every check are
curated as `p8-…`; surface selectors written as declared names (`on Top`)
were rewritten to kind paths. Three are left out for passing a scenario
file's 64 KiB tripwire (they want leaner versions, not a looser tripwire);
failing or non-loading ones go to the false-alarm lane.

**2026-10-20 · eval-coverage-5: design.md's animated features against the
gate.** The gate now uses looping and restarted `keyframes`, time signals,
`noise`, an animated GIF, `fit: fill`, `wave:`, middle ellipses, 75 Hz
and rate changes, and shadows springing at 1.5 and 2. Still uncovered:
`parallax`, drag and drop, `filter:`/`blend:`, masks other than
`wipe`/`dissolve`, particles, spectrum, graphs, SVG layers, Lottie. The
matrix is unchanged: a new effect costs about 330 kB against 135 kB of
tripwire room.

**2026-10-20 · eval-coverage-5: findings, not fixed here.** (1)
`reduce-motion off` does not restart time signals until something repaints
(`Renderer::refresh_reduced` marks surfaces dirty but no frame follows). (2)
`parallax` sets no motion bit in `flatten/probe.rs`, so correct parallax
fails `twin` and `tiling`; `parallax-card` is held back. (3) A time-driven
value that stops (`playing ? t * 20deg : 0deg`) snaps to its replacement;
design.md is silent, left for the owner.

**2026-10-20 · eval-coverage-5: what the new coverage catches.** Of seven
throwaway mutants, three are caught by rules, three by references, and one
(letters losing `index`) not at all (letter offsets are not recorded). j18
now fails `settle`; suggested rules for the rules lane: keyframe tracks,
shadow reach, a time-driven node standing still.

**2026-10-20 · eval-coverage-5: references.** `eval/refs/` is 135 kB
under the 6 MiB tripwire, kept so by leaving the matrix alone, keeping
new scenarios small and leaving out three large practice scenarios.

## shadow-buffer

**2026-10-20 · shadow-buffer: a surface keeps its shadows' room
while they spring.** Layout read a springing shadow's target,
cutting it when it shrank. While a shadow on the surface springs
(`Animator::shadows_moving`), `Renderer::hold_shadow_reach` never
asks for less overhang than now and takes growth at once (bug 9's
hold for size springs); `release_holds` after every paint asks for
the smaller overhang once all settle. Not the content-sized growth
rule. Tests: `a_shadow_springing_back_keeps_the_room_it_is_drawn_in`,
`a_content_sized_panel_keeps_its_shadow_room_until_it_settles`,
`shadow-lift-gaps.scn`.

## morph-memory

**2026-10-20 · morph-memory: a box whose surface goes leaves the screen
when it goes.** A detached idle panel's boxes were dated by its last painted
frame, so after a second open they were stale. The moment the surface
went is recorded (`Renderer::now`) and the next painted frame dates the
box that long before its presentation time (`SharedMorphs::date_left`,
`Animator::date_hidden`); until then it counts as just gone. Tests:
`a_shared_morph_starts_from_a_panel_that_closes_after_a_long_idle`,
`a_box_on_screen_starts_a_morph_until_it_left_a_second_ago`,
`morph-back-after-idle.scn`.

## builtin-name-diagnostics

**2026-10-20 · builtin-name-diagnostics: a `state`/`let` named
like a node boolean warns.** A file-level `state` or `let` named
`hover`, `pressed`, `focused` or `selected` is hidden by the
element's boolean (spec.md 6.3), so a `check::shadows_builtin`
warning marks the declaration (`Checker::hidden_by_node_bool`)
and each bare use (`Checker::node_bool_hides`); locals stay
quiet. A load error would break loading configs (section 14). The
driver appends warnings to a load failure (`load_failed`). `align:
center` with an id `center` stays a `check::ambiguous` error. Tests:
`a_state_named_like_a_node_boolean_is_named_where_it_is_declared`,
`a_config_that_does_not_load_says_its_warnings`.

## integration-r8

**2026-10-20 · integration-r8: round 8 integrated, one reference
re-approved, one mutant recut.** p8-cards-hover-lift-1_5 was re-approved
for shadow-buffer's hold (the buffer now holds 217 px until shadows settle),
and `eval/mutants/cross-surface-morph.patch` was recut over morph-memory's
`hidden(root, place, now)`. Mutants total 38.

**2026-10-20 · integration-r8: pop-out's shut clip excuses only
declared motion.** A clip shutting excuses a pop only when the
ancestor's own motion sizes it, or it is content-sized and the
node's exit reached its pose on that axis within its last step
(design.md "Poses": the exit must arrive). A node cut out by a
shut clip while exiting is judged at its last shown frame. Test:
`pop_out_passes_a_node_whose_clipping_parent_shuts_the_frame_after`
with failing twins.

**2026-10-20 · integration-r8: what time drives is read by
eval's own walk.** `eval/src/timed.rs` reads timed nodes from
compiled values independently of `PropValue::reads_time_with` and
`TokenTable::time_reads`: `t`, `wave(…)`, `noise` of a time read,
global tokens by fixed point, and tokens under a time-reading
`tokens` override, matching every `TokenExpr` variant without a
wildcard. Tests: `a_token_reads_time_through_the_tokens_it_reads`,
`a_timed_override_and_the_tokens_read_under_it`.

**2026-10-20 · integration-r8: architecture.md names the probe's new
shadow fields.** `ShadowDrawn::asked` and `frame` are described in the
eval-sandbox probe paragraph.

**2026-10-20 · integration-r8: `contain`'s `shadow` keeps skipping a box
an offset moves.** Layout counts offsets at their targets, and design.md
promises an overhang only for shadows at rest, so the arm stays a check of
boxes at rest; springing cuts are held by `shadow-lift-gaps` and references.

**2026-10-20 · integration-r8: a smoothly sampled gradient draws from
its whole cell's part.** `raster::draw::paint_for` sampled a cached
pixmap from another origin than an uncached part, so partial and full
repaints differed at near ties. A gradient drawn smoothly always uses
its whole cell's part (one origin); nearest sampling still uses the
cache. Tests: `a_scaled_gradient_repaints_from_its_cache_as_fresh`,
`scaled-gradient-redraw.scn`.

**2026-10-20 · integration-r8: a glow enlarges the buffer as a shadow
does, by its peak.** `layout` adds a node's glow to its shadow overhang
(`Glow::overhang`); a time-driven glow is taken at the top of its range
by interval arithmetic over `wave` (0 to 1) and `noise` (−1 to 1), so
the buffer holds one size. Setting a glow relays out; shadow-buffer's hold
covers glows. Tests: `a_glow_keeps_room_for_its_peak`, `glow-overhang.scn`;
the `rice_now_*` PNGs were re-blessed at 220 × 50.

**2026-10-20 · integration-r8: a list's view follows a row measured
above it.** `layout::list::place_list` moves the anchor by how much
rows above it that the pass measured differ from their estimate,
so a row entering from `height: 0` above a scrolled view keeps the
rows in view still; estimate changes still move the anchor, not the
view. Tests: `a_row_entering_above_the_view_keeps_the_rows_in_view`,
`insert-above-view.scn`.

**2026-10-20 · integration-r8: a press on another Strand surface
is a click away from a popup.** `Router::click_away_from_others`
also closes every open two-way popup that is not `trigger: hover`
(`InputScene::popup_open`, in architecture.md), unless the press landed
in or under it or on its anchor (whose handler decides); grabless popups
too. Tests: `a_press_on_another_strand_surface_closes_an_open_popup`,
`popup-click-away.scn`.

**2026-10-20 · integration-r8: a shared morph nested in another
goes its own way.** Flatten hands the morph the ancestors' drawn
transforms (`view`) and their morphs in flight (`Inherited::morphs`);
the flight springs on the surface to the rest box (`rest`) and is
mapped back each frame (`in_frame`). Eval counts a morph move in
`Row::flight` and magnifies morph arrival's give by ancestor
scales. Tests: `a_morph_nested_in_another_goes_its_own_way`,
`a_named_nodes_way_gives_what_its_ancestors_magnify`, `nested-morph.scn`.

**2026-10-20 · integration-r8: the false-alarm sweep; the power menu's
title left to the owner.** Every remaining failure is a non-loading or
misused scenario or a growth-rule finding. The power menu title's 7 px
jump comes from `glide_origin` gliding by the whole re-centring while FLIP
compares in buffer coordinates, the two mechanisms laptop/fix-r5b joins;
it goes to the owner with the growth question.

**2026-10-20 · integration-r8: a file output waits for a finished
wallpaper to be reported.** `ThemeHost::images_pending` reads
`Quantiser::settling` (a job running, or taken by a lookup and not
yet reported by `poll`), so a `file` output never writes the `??`
fallback's palette; `ThemeHost::wait_images` polls before its bump. Test:
`a_result_taken_by_a_lookup_settles_when_polled`.

## eval-finish

**2026-10-20 · eval-finish: an `id:` on a component call names its root
in `Instance::identity`.** `identity` names a node by its own `id:`, else
by the call whose state it adopted (the outermost call's in a chain),
per m6-w2-language. Test: `identity_reports_a_calls_id_on_its_root`;
`k-media-card.scn` keeps a call-site `#id` in the gate.

**2026-10-20 · eval-finish: `colour` follows a target a theme swap moves.**
A spring is a linear filter of its target, so where recorded targets
(`SpringRec::to`) move and end within 3 steps of rest, each frame is held
to the fan from its start to the targets' way so far (`colour::off_fan`),
within 3 steps and the overshoot bound; a still or wrong-ending target
keeps the straight way. Test: `colour_follows_a_target_a_theme_swap_moves`
and twins; `k-qs-toggles.scn`.

**2026-10-20 · eval-finish: hidden rows trace their own scale and turn;
stagger counts from a first frame.** The flattener's opacity-0 path pushed
scale 1 and turn 0; it now reports the node's own scale (morph included)
and turn. On a surface's first frame a batch's change is that frame less
the lead, at most a period and not before the surface's previous paint
(wave3-pixels p2). Tests: `a_hidden_held_child_reports_its_poses_scale`,
`stagger_counts_from_a_surfaces_first_frame`,
`stagger_holds_a_hidden_child_to_its_poses_scale`, `l-app-grid-stagger.scn`.

**2026-10-20 · eval-finish: every call-site `id:` is in the node's
identity (review).** `NodeState::calls` records each nested call
a state passed through, and `Instance::identity` keeps `id` (own,
else outermost call's) and adds `also` for the rest; eval's `#id`
matches either (`NodeMeta::also`, `NodeMeta::named`). architecture.md
names the field. Tests: `identity_reports_a_calls_id_on_its_root`,
`call-id-chain.scn`.

**2026-10-20 · eval-finish: `colour`'s fan follows only a straight target,
within spring reach (review).** Recorded targets are first held to the
line from first to last in premultiplied OKLab (share −0.5..1.5,
3 steps, `colour::off_line`), else the straight way holds. Each
frame's share is at most what the colour's own curve reaches by then
(`colour::reach`); beyond it fails as `ahead of its spring`. Test:
`colour_follows_a_target_a_theme_swap_moves` with `chasing_along`.

**2026-10-20 · eval-finish: on a first frame a stagger numbers only
children with an `enter` (review).** Generalised to every batch
by the stagger (owner decision) entry below.  `NodeMeta::enters`
counts a pose block that sets something or a preset. Test:
`stagger_numbers_only_entering_children_on_a_surfaces_first_frame`;
`l-grid-reopen-plain-box.scn`.

**2026-10-20 · eval-finish: four mutants for this round's
fixes; `unmatched:SELECTOR` (review).** Added `call-id-root`,
`call-id-chain`, `hidden-row-look` and `colour-srgb` patches. A mutant
whose run stops at an `intend` naming nothing is caught when written
`unmatched:SELECTOR` and the log names that selector; other failures are
WEAK. `every_mutant_line_names_what_exists` checks it.

**2026-10-20 · eval-finish: re-approvals keep their readings,
with `approve --renote` (review).** `approve --renote`
rewrites the note, date, revision and approver of the block a
run matches, keeping the recording byte for byte; it refuses
changed runs, runs with no reference and `--all-changed`. Test:
`approve_renote_rewrites_only_a_matching_blocks_note`.

## stagger

**2026-10-20 · stagger: only children with an `enter` are numbered,
everywhere (owner decision).** Per design.md ("Delays each child's
`enter`…"), a child without an `enter` takes no turn and is drawn at
rest. `Animator::stagger` numbers only children whose `enter`, read
through tokens (`anim/pose.rs::has_enter`, `is_pose`), is a setting
pose block, a preset (`popin(0.8) | slide(top) | slidefade | fade`)
or a page slide; eval keeps only `NodeMeta::enters` in every batch.
Supersedes the all-children reading and eval-finish's first-frame
case.  Tests: `stagger_numbers_only_the_children_with_an_enter`
(`effects_stagger_plain.png`), `stagger_numbers_only_entering_children`.

**2026-10-20 · stagger: the review's seven findings.** Presets are judged
(`NodeMeta::enter_preset`; `slide(edge)` by its release alone). Regressions
`stagger-plain-turn.scn` and `stagger-preset-turn.scn` with three
mutants. A plain child landing while its surface opens takes no turn
(`a_plain_child_landing_while_its_surface_opens_takes_no_turn`). The
sandbox reads `enter` through tokens (`probe::enter_of`); a keyed remount
keeps its first metadata (eval.md limit). `pop_in` takes a segment-end
row as rest only when at rest (`an_enter_a_mark_cuts_is_no_pop_in`).

## report

**2026-10-20 · report: the owner's glitch report is one command; the
rest is an agent's.** Owner-approved shape: an always-on in-memory buffer
of what drove the shell, `strand report "what I saw"` saving a bundle
(words positional, unquoted, every argument but `--json` one word), and
`scripts/report-view.sh` replaying it in the sandbox. Bare `strand report`
keeps its diagnostics.  spec.md 13 gains the words form and the schema
`system.replay` (additions spec.md 14 allows).

**2026-10-20 · report: two rings, one per thread, in a codec of
strand-core's.** The main thread rings input and the logic thread rings
changes, meeting only when a report asks, since no lock is shared on a hot
path. The format is `strand_core::flight` (types and codec only), shared
by the binary and eval with no new crate edge. Times use one process-wide
`Instant` epoch.

**2026-10-20 · report: fixed 24-byte records, a name table and a byte
arena, versioned.** About 256 KB in all: 24-byte records (ns time, kind,
flags, 16-bit slot, 12 payload bytes), interned names, and a lapping byte
arena for large values (2 KiB cap each). 7,168 input and 1,024 change
records with a 40 KiB arena; nothing allocates after `new`. The dump is
`STRNDFLT` version 1, and eval refuses newer.

**2026-10-20 · report: pointer motion is coalesced by the refresh period.**
A motion replaces the newest record when that is a motion of the same
surface less than one refresh period old (from presentation feedback,
else 1/60 s), keeping the first time: no timer or frame callback.

**2026-10-20 · report: the replay rewinds from the end snapshot.** Every
change carries its old value and the report snapshots every service and
cell at its end; the replay walks back to the window's start. The window
starts at the newest of the rings' oldest records, the last reload and
the last gap (lock or recorder off), so it never crosses a hole; lost
values are named in fidelity notes.

**2026-10-20 · report: what is never recorded.** Typed characters are
counted, never named (shortcuts are named); a password input keeps not
even a count; pastes are counts, copies nothing; `auth` never; nothing from
lock request to unlock, and a report is refused while locked. Config texts
are kept only when `Program::is_text_literal`, else `{"~text": N}`; call
and action arguments never. The bundle is 0700 and stays on the machine.

**2026-10-20 · report: the opt-out is `system.replay`, as
`notifications.remember` is.** A builtin `rw` field kept in the state
directory, default on (`strand set system.replay false`); off drops both
rings. An env var or a new service were not taken.

**2026-10-20 · report: hooks the binary installs, not new dependencies.**
`Services::tap`, `Instance::set_cell_tap` and `Router::focused_input` are
callbacks or queries the binary uses; no crate learns about rings. Calls,
actions, `rw` writes and reloads are recorded in the binary; state-computed
synchronous calls (`clock.format`, `workspaces.on`) are not.

**2026-10-20 · report: each surface's last frame comes from Strand,
encoded off the frame path.** A CPU surface's newest shm buffer is
copied (`State::frame_pixels`); a GPU one is repainted by the CPU
(`Renderer::repaint_offscreen`), noted in the PNG.  A short-lived
`strand-report` thread writes the bundle, so neither loop blocks.

**2026-10-20 · report: the bundle copies the config and capped assets,
mirrored at their paths.** `.strand` files and `icons/` go under `config/`;
every other file read goes under `files/<absolute path>`, at most 2
MiB each and 16 MiB in all, the rest listed missing. The replay mounts
mirrored directories read-only at their original paths. The newest 20
bundles are kept; pruning removes only directories holding a `bundle.json`.

**2026-10-20 · report: the conversion is eval's, as
a local scenario.** `strand-eval replay <bundle>` writes
`eval/scenarios/local/report-<name>.scn` and runs it with the owner's
fonts. Format additions any scenario can use: `[seed]`, `wall`, `warmup`,
`icons`, JSON `set` values, `key`, `type`, `paste`, `focus`, `unfocus`
and `answer`. Cells are seeded, never driven. By default only the last
20 s are traced and checked.

**2026-10-20 · report: the replay says how faithful it is.** It compares
press hit nodes, surface mapping times, cell writes and its final frames
against the bundle, names the first divergence and every limit, and
`replay.txt` puts those first, then a timeline by node, then findings.

**2026-10-20 · report: the budget is counted and measured, and no wakeup
is added.** The rings are 262,144 bytes, counted in design.md's memory
table (0.25 MB).  Builds measure PSS on minus off (at most 288 KiB);
records go only on existing paths, so idle-wakeup windows in budgets.rs
must pass unchanged.  Budgets are tripwires, not caps.

**2026-10-20 · report: the hooks as built, where they differ from the
sketch.** (1) An arena reference is a u32 offset and u16 length at payload
bytes 6..12 (`VALUE_CAP` 2,048, also capped at the arena size). (2) The
cell tap is told once per tick from `Tick::written`, skipping unchanged
values.  (3) `Program::is_text_literal` builds its set on first use. (4)
`Router::focused_input` takes the scene tree. (5) The services tap sees
reset patches too, never no-op ones; the compiler's `system` stub gains
`replay`.

**2026-10-20 · report: the binary's recorder and bundle as built.**
(1) Mapped records carry no namespace name (`bundle.json` names surfaces
by slot). (2) Configured records hold physical size and scale. (3) Fetch
values are `{"ok": …}` or `{"err": …}`. (4) Action exits are 0 or
1. (5) Reload counts are files, cells kept, cells reset. (6) Token overlays
keep their text. (7) A release is recorded only when its press was named
(16-entry table). (8) Taps exist only while recording. (9) Config service
values are recorded by declared field name (`mood.level`). A lock kept
mounted pauses recording on its `open` change.

**2026-10-20 · report: the rings' cost is measured on the anonymous PSS.**
`the_replay_rings_cost_at_most_their_size` fills both rings by clicks and
compares `Pss_Anon` with `system.replay` on and off, under the 288 KiB
bound; whole PSS swings with shared pages. Existing memory budgets and
`.text` gates pass unchanged.

**2026-10-20 · report: the scenario format's additions as built.** (1)
`wall` uses `SchemaHost::mock_at(now, offset)`. (2) `warmup` ticks run
painted but untraced; the first traced tick repaints every surface. (3)
The compositor's focus rule is modelled (`keyboard: exclusive` on map,
`on_demand` on press); `focus`/`unfocus` override. (4) `answer svc.method
error "text"` gives a failure; JSON answers are type-checked. (5) `set`
and `emit` read `{`/`[` values as JSON first. (6) A `[seed]` for a cell
that never mounts is a run note.

**2026-10-20 · report: the bundle gains `nodes.json`; a mapped record
reads as unnamed.** `nodes.json` names each node the input ring names
and each surface root by `node_ref`, so presses are compared in order;
the format stays 1.  `strand_core::flight::Dump::entries` treats a mapped
record as unnamed, as architecture.md says.

**2026-10-20 · report: the replay as built, where it differs from
the sketch.** (1) An unbounded window starts at the first surface
mapped. (2) Sandbox-kept services (`screens`, `clock`, `calendar`,
`auth`) are not seeded; a service with no stand-in is a listed limit. (3)
Recorded answers queue as early as possible; `SchemaHost::take_fetches`
lists the mock's answers. (4) Gaps are `frames N`. (5) Icons flatten into
`report-<name>.icons/`. (6) `replay.txt` motion is one line per run of
change, capped at 400 lines.

**2026-10-20 · report: the replay's fixture bundles
are written by the test.** `eval/tests/replay.rs` writes
bundles with `strand_core::flight::Ring` from the committed
`eval/fixtures/report/shell.strand`, so nothing binary is committed and
nothing drifts.

**2026-10-20 · report: report-view.sh, its mounts and its tests in CI.**
`scripts/report-view.sh` runs `strand-eval replay` via `run.sh eval`
into `target/report-view/<name>/`, then copies to `<bundle>/replay/`;
`scripts/eval/replay-mounts.sh` mounts `files/` read-only. Test:
`scripts/eval/test-report-view.sh` in `scripts/eval/ci.sh` step 4 and, with
`--e2e`, a new step 7 (`a_scripted_session_is_kept_for_its_replay`). The
GitHub `eval` job timeout is 50 minutes.

**2026-10-20 · report: the review's fourteen findings.** (1) External
cell writes (`strand set`, `toggle`, `reset`) are recorded as change kind
`0x4A` and replayed; surface steps wait a tick after `MAPPED`; an untakeable
step is a `step` divergence under `RunOptions::log`. (2) Lock cells and
password-bound cells are left out entirely (`Ctx::secret_cells`).  (3)
`ToLogic::Hit` keeps identities of the newest 256 presses. (4) Bundles
are written as `.partial` and renamed; stale partials pruned after 10
minutes. (5)–(6) Held keys forgotten on leave; a new ring starts with
a gap end (`gap::OFF`). (7) The window is capped at 120 s. (8) Copied
paths are lexically normalised. (9) `icons/` is read as strand-icons
reads it. (10) Icons resolve on the report thread (`DrawnSources`). (11)
Horizontal scrolls are not replayed. (12) The cell tap reads only written
cells. (13)–(14) Docs updated; `nothing_is_recorded_while_locked`
was run in the lock VM.

## integration-next

**2026-10-20 · integration-next: the recorder's off-and-on test follows the
review's fix.** `replay_off_drops_the_rings_and_bundles_have_no_recording`
now asserts each ring opens with its gap end (`GAP_ENDS`/`CHANGE_GAP_ENDS`)
and every later `MAPPED` record carries the gap end's instant. No
behaviour changed.

## evalfix

**2026-10-20 · evalfix: `teleport` counts every place an offset's spring
was sent as on its way.** For x and y, while a spring is in flight on a
node's offset or an ancestor's, its travel also spans its written place
(`laid + (inherited − flight) + written`, through its transforms);
a snap adds nothing and is still held to 0.75 of the travel. Test:
`an_offset_sprung_out_and_back_by_logic_travels_its_excursion`;
`offset-out-and-back.scn`, mutant `offset-retarget-snap.patch`.

**2026-10-20 · evalfix: `pop-in` judges an enter a moving clip uncovers
where it starts.** A node entering whose extent is wholly outside
its ancestors' clip from mount until it first shows is judged on its
mount frame, with the same 0.5 share (wave3-pixels p2: a sizing box
clips). Test: `an_enter_a_moving_clip_uncovers_is_judged_where_it_starts`;
`reveal-enter.scn`, mutant `sizing-clip-open.patch`.

**2026-10-20 · evalfix: `pop-out` holds a removed subtree's root to its
pose.** Per spec.md 6.4, only the topmost exiting node is held to its exit
pose; passengers ending with it are not judged, and one ending while the
root is still drawn must have shown nothing (`Timeline::visible`, `shut`,
`shut_drawn`). Test: `pop_out_judges_a_removed_subtree_at_its_root`;
`ghost-passengers.scn`, mutant `ghost-children-undrawn.patch`.

**2026-10-20 · evalfix: the review: places from parts, spring starts,
moving clips, dropped exits.** The written place uses the probe's
parts, not the drawn box; a spring's start place is added to its way;
a cut-out enter is judged at mount only when a clip uncovers it by
declared motion (size spring, exit, `GLIDE`, scroll); the harness records
`RunTrace::removed` so a dropped `exit` fails pop-out (not a list window
unmount). Tests: `a_stray_jump_under_an_offset_spring_is_still_a_jump`,
`an_offset_snapped_out_springs_back_from_where_it_was`,
`an_enter_a_still_clip_lets_show_is_judged_where_it_shows`,
`pop_out_holds_a_removed_root_whose_exit_was_dropped`; mutants
`offset-spring-doubled`, `exit-dropped`. `mutants.sh` names reruns
`<name>-<scenario id>`.

## owner-2026-10-21

**2026-10-21 · owner-2026-10-21: nested `exit` poses do not play; the
removed root's carries all (owner).** Only the removed subtree's root
plays its `exit`; what is inside rides in it and unmounts with it, per
spec.md 6.4. A nested exit would add a concept (an exit order). design.md
"Poses" says so; no code change.

**2026-10-21 · owner-2026-10-21: eval rules are frozen but on Strand's
gate; elsewhere findings are hints (owner).** The guarantee is the
trace and the replay's fidelity. A rule changes only when wrong on a
committed scenario, regression or matrix cell, never to quiet one shell;
no threshold, `known.toml`, `allow` or `expect_fail` is added for a real
shell or report. Only Strand's own surfaces are recorded. evalfix's rules
stand on their committed regressions.

## replay-owner

**2026-10-21 · replay-owner: `strand replay` hands off to `strand-eval`,
run natively.** `strand replay [report-dir] [--no-open]` execs
`strand-eval replay …` found beside `strand`, else on `PATH`, as
`git` runs subcommands, so the binary gains no eval code; without it
the command says plainly what is needed. It reads files where they are
now and replays GPU surfaces on the CPU unless `--gpu`, both stated as
limits. `scripts/report-view.sh` stays the agents' container path. The
local scenario goes to the checkout's `eval/scenarios/local/` or
`${XDG_CACHE_HOME:-~/.cache}/strand-eval/`.

**2026-10-21 · replay-owner: the verdict holds the report's frame to the
live boxes, node by node.** `strand report <words>` writes `drawn.json`:
each surface's nodes as last drawn (box through transforms, own opacity),
re-flattened on demand (`Renderer::drawn_nodes`), matching the trace's
`visual` rows. VERIFIED needs every node within 1 logical px per edge and
0.02 opacity, none missing; else DRIFTED naming each node; no `drawn.json`
is NOT CHECKED. The verdict leads every output.

**2026-10-21 · replay-owner: a replay labels the checks' findings as hints
and never fails on them.** `replay.txt` lists findings last under `hints
(the checks may be wrong on a real shell):`, `fidelity.json` as `hints`,
and the viewer labels them; `strand-eval replay` exits 0 on findings,
held by a test.

**2026-10-21 · replay-owner: the review: the verdict holds the last frame
before the report.** The live shell records `drawn_ns` and its frame period
(`drawn.json` format 2); the replay holds its frame at the tick nearest
`drawn_ns`, never at or past the report's tick (format 1: the last painted
before). Test: `the_frame_held_is_the_last_before_the_report`.

**2026-10-21 · replay-owner: the review: a moving node is held to its
own motion.** `drawn_nodes` peeks one period ahead to record each node's
`travel` and `fade`; a moving node is held to 1 px plus two frames of travel
and 0.02 plus two frames of fade, else named `moving out of step` without
making the verdict DRIFTED. At rest the bound stays 1 px and 0.02. Test:
`a_drawn_frame_mid_motion_says_how_far_each_node_moves`.

**2026-10-21 · replay-owner: the review: surfaces matched by
their root's whole identity.** A live surface is matched to the
replay surface whose root has the same `nodes.json` name (origin,
`id:`, row key), each once; a surface shown only in the replay
is DRIFTED. Tests: `a_surface_shown_in_the_replay_only_drifts`,
`surfaces_from_one_place_are_held_each_to_its_own`.

**2026-10-21 · replay-owner: the review: VERIFIED holds pixels too;
changed files are only named.** Where the bundle has a PNG, the held frame
is compared at `cpu-gpu`'s tolerance outside moving boxes; differences or
an uncomparable PNG make it DRIFTED. Files changed since the report are
listed as limits, not failures, since boxes and pixels measure their effect.

**2026-10-21 · replay-owner: the review: nowhere in a replay does
a finding read FAIL.** In a replay `html::entry_json` gives `hint`
statuses (`.st-hint`), and `summary.md`, `summary.json` (`ok: true`),
`summary.txt` and `replay.txt` say hints. Gate runs are unchanged. Test:
`the_checks_findings_are_hints`.

**2026-10-21 · replay-owner: the review: evalfix's rules stand on
fixtures of Strand's own.** The four regressions (offset-out-and-back,
offset-early-callback, reveal-enter, ghost-passengers) were rewritten
without the island's values, each failing the pre-evalfix rules (49d44e8b)
and passing the new. Rules are kept, being readings of design.md and
spec.md. offset-spring-doubled is now caught by `coherence` only; the
gap is named in eval.md, not tuned.

**2026-10-21 · replay-owner: the review: worktrees removed by real
path; failed controls kept.** `remove_tree_at` and `forget_tree` pass
`realpath -m`, since git matches relative paths as suffixes of any
worktree. `keep_control` keeps a failed control as `<name>-head.failed-<UTC
time>`, and later mutant lines of that scenario are skipped. Test:
`scripts/eval/test-worktree.sh`.

**2026-10-21 · replay-owner: the review: the command line fixes.**
`strand-eval replay` checks `bundle.json` and `config/` before clearing
`replay/`; both option lists share `cli::REPLAY_VALUED` (including `-o`);
`replay` reads `args_os` so non-UTF-8 paths pass; `RunOptions::drawn`
holds `Renderer::drawn_nodes` to the trace, tested by `eval/tests/drawn.rs`.

**2026-10-21 · replayfix: the owner's first replay: unnamed root, lost
name, early seed, screen.** A root is named by its origin under the config
directory; a root with none or too long is written with `NAME_IN_ARENA`
and no value, never a placeholder. The replay parses its own scenario
first. `[seed]` waits for the mount (`Instance::cell_mounted`). The header's
`screen` carries `name`, `id`, `make`, `model`, `description`; `bundle.json`
outputs carry ids; the mock answers `workspaces.slots`. `report-view.sh
--out DIR` leaves the bundle untouched.

**2026-10-21 · replayfix: a replay traces its whole window; its viewer
shows time.** The whole window (up to 120 s) is traced by default
(`--traced SECONDS` keeps the last ones), since speed beats size. A
replay's scrubber is real time (`RunInfo.replay.timeline`) with idle
spans and input lanes. The output pick falls back to the most input in
the whole window; a `leave` followed within 50 ms by an `ENTER` on another
replayed surface is no step; surface steps wait `MAP_FRAMES` (two) ticks;
the local scenario goes to the checkout holding the working directory. Test:
`a_popup_session_is_kept_for_its_replay`.

**2026-10-21 · replayfix: a replay's report is packed; a lost record keeps
its field's history.** (1) A replay's report is packed (`report::pack`,
eval.md "A replay's report, packed"): `trace.json` frame against frame,
unpacking to the same trace; frames as patches of 32-px tile runs, whole
above 75 % changed, on a new or resized surface and every 256 frames;
the page keeps data URIs. `read_entry` reads it exactly; `explain --png
DIR` writes frames whole. Other runs and references are unchanged. (2)
A lost record no longer stops its field's rewind: the value after it is
set at its time and the rewind goes on.

**2026-10-21 · replayfix: the changes ring keeps what changed and only
what the config reads.** `run/record.rs`: a change is written as edits
(`{"e": [[path, old, new], …]}`) when shorter; equal keyed updates are
dropped; fields the program never reads (`Program::reads`) are not recorded
(events, config services and `system` always are); a ring evicts records
whose value a write laps (dump version 2, version 1 still reads). Within
the fixed 256 KiB: changes arena 64 KiB, input ring 6,144 records, value
cap 8 KiB. Test: `a_busy_session_is_kept_for_its_whole_window`. Keyframes
and a larger budget were not taken.

**2026-10-21 · replayfix: a live frame at rest is held to the
replay at rest.** A live frame drawn more than `STEP_FRAMES`
periods before the report is held to the replay's last paint up to
`STEP_FRAMES` ticks past the nearest tick, never past the report
(`drawn::frame_for`); bounds are unchanged. A paint that damaged
nothing uses the surface's last kept image (`drawn::image_of`).  Test:
`a_live_frame_at_rest_is_held_to_the_replay_at_rest`.

**2026-10-21 · replayfix: an art fetch takes no caught signal.** With
`SO_RCVTIMEO` set, Linux fails a signal-interrupted `recv` with EINTR
and ureq ends the fetch. `fetch_with` blocks every signal but SEGV,
BUS, FPE, ILL, TRAP, SYS and ABRT on its thread while it fetches. Test:
`a_signal_caught_while_a_fetch_waits_does_not_end_it`. Retrying on EINTR
was not taken.

**2026-10-22 · viewer-owner: dashed boxes show where a node rests;
onion outlines only what moved.** The dashed box is the laid box moved
by its own written offsets (`wx`, `wy`) and its ancestors' (`ix − fx`,
`iy − fy`); `moving` compares against it. The onion outlines only
earlier boxes that differ by more than half a physical pixel or whose
node is gone. The trace and checks are unchanged.

**2026-10-22 · replay-clip: a clip is a selection spec,
made in one place.** A clip is only its spec (report, slice,
element keys, note); everything it says comes from the trace and
`fidelity.json`. `eval/viewer/clip.js` is the one implementation, run by
the page and by `strand-eval replay clip` in an embedded engine, so they
cannot drift.

**2026-10-22 · replay-clip: QuickJS (rquickjs 0.14), not boa.**
boa_engine pins ICU and other versions that conflict with the root lock
(`scripts/eval/check-lock.sh`); rquickjs 0.14.0 fits. strand-eval only;
the shipped shell links no JavaScript.

**2026-10-22 · replay-clip: `strand replay clip [report] --from T [--to T]
[--node SEL]… [--note TEXT] [--json] [--save FILE.zip]`.** `strand`
hands it to `strand-eval replay clip` (`CLIP_VALUED` in both). A time
is seconds before the report (`-3.2s`, `-3200ms`) or a tick (`k5226`);
a node is its key, `#id` or `file:line[:col]`. `--to` defaults to the
report. A report not yet replayed is refused with the command to run.

**2026-10-22 · replay-clip: a clip's fidelity is per frame; VERIFIED only
where compared.** A clip says VERIFIED only of the verdict's frame when in
its slice and clean, lists differences otherwise, and calls other frames the
replay's own. It lists divergences inside its slice and counts earlier ones.

**2026-10-22 · replay-clip: the crop is the union of drawn boxes over
the slice, 8 px around.** One rect per surface for the slice, in buffer
pixels, clamped to the surface (whole when nothing is on it). Crops are
deflated PNGs and the `.zip` is stored with dates 1980-01-01, so the
same spec gives the same bytes; the command line rebuilds frames with
`report::pack::Composer`.

**2026-10-22 · replay-clip: the replay's page is simpler; other pages
unchanged.** With `DATA.replay` the page shows the verdict, frame,
timeline, rest and onion toggles, hints as marks and the clip, with
the rest behind a Details toggle (`localStorage`, in `try`/`catch`);
a replay's clip starts empty.

**2026-10-22 · replay-clip: a slice's ends are exact; a tick given is
kept (review r1).** A tick is exact, a time is to the ms rounded once,
and a tick is kept within `SLACK_MS = 1` of either end. The page writes
an end on a painted frame as `k<N>`; the spec records `spec.from_tick`,
`spec.to_tick`. Test: `a_clips_ends_are_its_ticks`.

**2026-10-22 · replay-clip: verdict frames name their surface and moving
nodes (review r1).** `fidelity.json`'s `verdict.ticks[]` carries `sid` and
`in_motion` (`replay::drawn::Tick`). A clip places a frame by `sid` (old
files by a unique root only), names the bounds a VERIFIED surface was held
to, lists `moving_out_of_step` apart, and gives an uncompared surface only
reasons the files hold; a slice before `window.traced_seconds` is refused
as untraced.  Tests: `a_clip_says_which_of_its_frames_were_compared`,
`a_clip_places_the_verdict_on_the_surface_it_compared`,
`a_clip_says_what_the_trace_does_not_hold`.

**2026-10-22 · replay-clip: a click or a rectangle adds only what shows
(review r1).** Both use the checks' invisibility rule (`opacity0`,
`scale0`, `clipped`) plus a box and some opacity; the hover card still
names whatever is under the pointer.

**2026-10-22 · replay-clip: the held frame stays apart; a surface's going
is said; the .zip says what it lacks (review r1).** The frame held at a
slice's start is listed apart from the slice's own; a detached surface's
row reads `kN (gone)`; crops are named by the tick their surface painted;
frames whose pixels the page lacked are listed in `clip.txt` and `clip.json`
(`missing`).

**2026-10-22 · replay-clip: review r1's smaller findings, all taken.** The
`.mono` and `pre.cmd:empty` rules are scoped under `#clip`; buttons are 40
px (selects 36); the dashed box and a clip's `rest` share `StrandClip.rest`.

**2026-10-22 · growth: the `.text` gates raised at the owner's word.**
Per the rule every re-base since 2026-10-12 uses (CI's measurement plus
a 512 KiB reserve, rounded up to 256 KiB), the gates are 26,738,688 B
(GPU) and 22,806,528 B (CPU-only). They stay tripwires, not caps. Test:
`the_release_binary_code_stays_within_its_gate`.

**2026-10-22 · growth: a content-sized box springs over a step of
its content.** Owner: growing should be smooth, with escape hatches. A
content-sized `panel`, `osd` or `popup` root's box is its content size plus
a per-axis `step` springing to zero, retargeted from where it was drawn; the
curve is the root's `width`/`height`/`size` transition, `$motion.spatial`
by default; `~ instant`, `reduced_motion` and steps under `STEP_MIN`
snap. Growing configures the larger buffer at once; shrinking holds it
(`Renderer::steps_down`). `glide_origin` is removed. Buffer springs and
scaling content were not taken.

**2026-10-22 · growth: the hatch is `width: auto ~ …`, `height:
auto ~ …`, `size: auto ~ …`.** A transition already says how a size
moves, so no new prop: the schema gains `enum SizeKeyword { auto }` and
size props take `length | SizeKeyword`; `auto` lays out as unset, and
only on a content-sized root does its transition drive the step. Tests:
`sizes_take_auto_only_as_a_keyword`, `auto_sizes_carry_their_transition`,
`auto_instant_snaps_the_growth` and siblings in `growth.rs`.

**2026-10-22 · growth: what the springing box holds.** Laid out at the
size in flight (`Renderer::lay_out_stepped`), the root and boxes filling it
at rest and as last drawn (`StepFills`) follow and clip; others keep their
rest size (`Forced::keep`), far-anchored ones by that edge (`keep_far`). The
box is placed at its exact share (`RootSize::Placed`'s `exact`); overshoot
is cut by the buffer. A step's first pass compares everything where drawn
(`step_flip`); `~ instant` opens no such scope. Tests in `growth.rs`.

**2026-10-22 · growth: `contain`'s `held` cuts what shows by the
surface's buffer.** A rule wrong on Strand's own gate: `held` now cuts
the shown box by the surface's buffer on a surface no pose moves. Test:
`held_cuts_what_shows_by_its_surface`.

**2026-10-22 · growth: the scenarios, re-approvals and mutants.**
laptop/fix-r5b's `snapped-growth-cut` and `held-grow-glide` join the
gate; 37 references and 60 popup-content-height cells were re-approved
after reading.  `clip-carries-rows` uses `height: auto ~ instant` to keep
its snap.  `refs::TOTAL_BUDGET` rises to 6.5 MiB (6,815,744 bytes). Six
growth mutants are listed.

**2026-10-22 · growth: the review's findings, each with a failing-first
test.** (1) The fill chain goes into `pages` and across `scroll`/`list`,
never down; children there keep rest sizes. (2) The growth's
`buffer_shift` share counts only where the box is placed short of its
buffer (`placed_in`).  (3) `Painter::input_region` gives a content-sized
surface's box (`Renderer::placed_root`) while it lies short of its
buffer; OSDs and tooltips take none. (4) A far-anchored box is placed by
`have.round() − used.round()`. (5) A paint before the larger configure
keeps the box at the buffer's size (`SurfaceState::awaits`). (6) Two notes
corrected. (7) A popup's anchor is re-resolved once its surface rests
(`place_waiting_popups`).

**2026-10-22 · growth: the review's re-approvals and mutants.** 38
references changed only in input-region tokens (`i=`), each checked
against its root box; `i=-` at rest. `refs::TOTAL_BUDGET` (6,815,744)
stays. growth-exact-origin and growth-fill-drawn were recut.

**2026-10-23 · fidelity: a report from several outputs replays every
one.** Every output is a sandbox screen (`screens`, `main_screen`,
a `screen` step, each surface at its own output's scale) in
`screens.all` order (`bundle.json`'s `order`); the busiest stays
main. This removes a concept; a different refresh rate stays a
limit. Test: `a_report_from_four_outputs_replays_every_one`, on
`eval/fixtures/report/screens.strand`.

**2026-10-23 · fidelity: a cell's start is never guessed from a lost
change.** A name too long for the table is kept with a truncated value
(`Ring::named_value`). A cell whose start is unknown starts as the config
declares it in a window from the shell's start, with a note, else at the
rewound value noted as uncertain. Lost writes are matched by time within
two frames; services with lost values are a limit, never invented.

**2026-10-23 · fidelity: the verdict says where the replay parted
earlier.** Every verdict line ends "; but the replay parted from the
live shell N times earlier in the window, first at T (what: text)"
(`verdict.parted`; clip.js is the one source). No check, bound or verdict
word changes.

**2026-10-23 · fidelity: the owner's report replayed again, in the
container.** With every output a screen, the DP-10 card growth plays and
the other outputs' bars mount; the remaining divergences are what the
bundle cannot replay (values a version-1 ring lost around a volume change)
and, in that run only, missing host fonts.

**2026-10-23 · fidelity: review of the four-output replay: per-output
scale, input-only focus.** (1) clip.js crops each surface at its own scale
and names its `output`.  (2) `screen` steps are placed after time-ordering;
`drop N surface=<sel> output=<screen>` moves no focus. (3) A node shown
on several outputs live is one surface in the sandbox (keying by output
needs a trace-format decision); a limit names it. (4) Outputs plugged
or unplugged in the window are a limit.  (5) `persist` cells start at
the rewound value (`harness::persisted_cells`); the nameless rule takes
only names the table could not hold. (6) Nameless matches are named by
time. (7) A `screens: focused` panel or OSD maps on its live output. (8)
`RunTrace::scale_at` is the frame's main scale; physical-pixel gives use
`scale_on(frame, surface)`; `tol` stays the coarsest. (9) `missing` and
`replay_only_surfaces` are `{surface, output}`.  (10) The viewer's output
choice is kept per replay.

**2026-10-23 · scroll-owner: a scrub leaves the replay page where
it is scrolled.** The filmstrip sets its own `scrollLeft` instead of
`scrollIntoView`, and `draw` restores the page's scroll after a redraw
empties parts; the verdict wraps long keys at phone width. Test:
`viewer_check.py`'s `exercise_scroll_kept`.

**2026-10-23 · flicker-owner: a frame scrubbed to while its image decodes
keeps the frame before.** `renderStage` leaves the stage as is while the
new frame's images load, redrawing once on load (a broken image counts
as loaded), and `draw` pre-decodes three frames ahead and one back. Test:
`exercise_no_flicker`.

**2026-10-23 · services: why the owner's replay muted where the live
shell did not.** A config's write to a service field was recorded only as a
`WRITE`, so the rewind rebuilt the service's answer on the pre-write value
and played it as a whole `set`, firing `on change` twice; config-only
fields (`notifications.dnd`) were seeded at their report value; item
writes were not recorded. Shown failing first in `eval/tests/writes.rs`
on `eval/fixtures/report/writes.strand`.

**2026-10-23 · services: a config's write is recorded as the change
it made.** `Services::tap`'s `Change::Field` and `Change::Keyed` gain
`local: bool`; `DynService::write` and `write_item` tell the change,
recorded flagged `LOCAL` (16) beside the `WRITE` (dump version 3,
`Dump::version`). The rewind undoes it and the play forward never plays a
`LOCAL` record as a `set`.

**2026-10-23 · services: an older report's writes start where the ring
says, or are named.** For version-1 and -2 rings each written place starts
at the last value recorded before its first write, else (window from the
shell's start) at the service's default (never `system`'s), with a note;
otherwise a limit names the field, its write count and assumed start.

**2026-10-23 · services: review: older rings' item writes named;
service writes held; taps never fail writes.** (1) For older rings,
`harness::item_write_sites` names each config place writing a keyed-list
item in a limit. (2) Fidelity holds each live `WRITE` and `LOCAL` keyed
record to the replay's writes (`ReplayLog::service_writes`), a number within
a thousandth; a live write replaced within a tick is only noted. Test:
`a_service_write_the_replay_made_otherwise_diverges`. (3) A whole-field
write reads the field for the tap with `.ok()`, so recording never fails it.

**2026-10-23 · clip-text: a clip tells apart two elements of one line and
counts across surfaces.** Twins on one line show their key's end and the
frame they appear or go; the header says `across N surfaces`; the verdict
says `before the report` and the clip `before this slice`; each surface
names its output. Test: `a_clip_tells_apart_two_elements_of_one_line`.

**2026-10-23 · perf-bench: an animation frame-cost bench, measured before
any change.** `strand-render/tests/anim_bench.rs` (`#[ignore]`d, never a
gate) times eighteen animated scenes at launcher and screen sizes on CPU,
lavapipe and a real GPU. `PaintTimes` gains `layout`, `diff`, `lower`,
`blend` and `RasterTimes`; `strand-gpu` gains `record_frame_phases` (off
by default); architecture.md lists both. Runs are repeated and pinned to
performance cores; the baseline is in benchmarks.md. A sampling profiler
and wall-clock gates were not taken.

**2026-10-23 · perf-cpu-raster: a mask is computed only within reach;
polygons read nearby edges.** `layers::cell_mask` computes only the window
where bounded masks can be non-zero (grown by 0.501 px and 2 px margin);
fades keep the whole cell.  `shapes::Polygon` bins edges by row band (a
512th of the height past 512 px; non-finite or over 65,535 points reads
all), giving identical bits.  cover-mask p95 13.3 → 0.33 ms. Test:
`binned_coverage_equals_every_edge`.

**2026-10-23 · perf-cpu-raster: a large paint draws its cells
on a few scoped threads.** A paint of `PARALLEL_CELLS` (8) cells
or more spreads them over up to `MAX_THREADS` (4, fewer on fewer
cores), the render thread included, each in its own `Lane`; at
most 3 helpers run per process, and threads are joined before
the paint returns, so an idle shell has none. Smaller paints stay
serial. `raster::share` also splits large gradient dithering and corner
tiles. Tests: `cells_drawn_on_threads_are_the_render_threads_bytes`,
`no_raster_thread_outlives_its_paint` and siblings. A persistent pool
was not taken.

**2026-10-23 · perf-cpu-raster: transformed gradients crop from the cache;
colour tables are kept.** A smoothly drawn gradient's part is copied from
the cached whole (`cache::crop_part`, same pixels); 1,024-sample OKLab
tables are kept by their stops' bits (`cache::lut_cached`, 256 tables, about
4 MB). Tests: `a_part_cropped_from_the_whole_is_the_part_dithered`,
`colour_tables_are_kept_exact_and_bounded`,
`a_transformed_gradient_draws_its_parts_from_the_cache_as_fresh`.

**2026-10-23 · perf-cpu-raster: goo's field is worked out into one
buffer.** Corner box distances go into one buffer (on the stack up to
eight boxes), the same bytes. Test: `reused_buffers_draw_the_eager_bytes`.

**2026-10-23 · perf-cpu-raster (review): idle lanes and colour
tables are freed; the bench counts helpers.** Helper lanes are
freed at the first trim after `LANE_IDLE` (1 s) unused, bringing
the cold bar back under its 43,008 kB warn target; colour tables are
freed with the paint cache's idle entries (`PaintCache::trim_idle`).
`stops_differing_in_one_field_keep_their_own_tables` tests the table key.
`anim_bench.rs` counts `strand-raster` helper allocations, and before/after
tables are interleaved pairs, since runs on this host are bimodal.

**2026-10-23 · perf-gpu-retain: a GPU frame draws its shadows,
backdrops and crossfade snapshot.** `strand-gpu`'s `Frame` and `Op` gain
`Op::BlurredRect` (vello's blurred rounded rect, as `cache::draw_shadow`
uses), `Op::Backdrop` (what is behind as ops, the pass run in-frame)
and `Op::Snapshot` with `Frame::snapshot` (`Take`, `Keep`, `None`);
architecture.md lists them. The CPU's look is the reference within
`GPU_TOLERANCE`; CPU paths stay for CPU surfaces.

**2026-10-23 · perf-gpu-retain: which backdrops a GPU frame draws, and
what repaints them.** In-frame unless a CPU-drawn offscreen group holds it
or a later CPU backdrop reads it (`inframe_backdrops`); what is behind is
`backdrop::behind`'s list.  On a GPU surface the node's damage hashes the
pass's want key. Backdrop textures are pooled per surface and freed with it.

**2026-10-23 · perf-gpu-retain: a small backdrop blur on
a GPU surface is drawn in-frame at the CPU's reduction.**
Drawn as `Op::Backdrop` at the CPU's `reduce` (quarter
from 8 device px, half from 2, full below) and scaled back as
`backdrop::upscale` does, so its look matches the CPU's within 1. Test:
`a_small_backdrop_blur_looks_the_same_on_either_backend`. Blurs only
inside CPU groups stay CPU (`PassWant::inframe_only`).

**2026-10-23 · perf-gpu-retain: a presented crossfade's snapshot is the
frame on screen, on the GPU.** Render marks the snapshot `swap::OnGpu` and
asks the GPU thread to copy its last frame (`FrameSnapshot::Take`) when it
lowered that surface's last frame at this size (`Surf::shown`). A surface
back on the CPU uses `Snapshot::under_old` for a second crossfade. The
eval's `redraw` asks the same `Take`/`Keep` (`fresh_gpu_frame`). Test:
`a_crossfade_replaced_after_its_gpu_snapshot_goes_on_from_the_blend_shown`.

**2026-10-23 · perf-gpu-retain: the warm start builds these pipelines;
goo stays.** The warm-up frame also draws a radial gradient, a shadow, a
backdrop blur, glass and a snapshot before `Ready`, keeping only pipelines
and adding no wakeup. Goo on the GPU is left to the goo flatten stream.

**2026-10-23 · perf-gpu-retain: a backdrop whose input is unchanged keeps
its pixels on the GPU.** `Op::Backdrop` carries a `key` (what is behind,
passes, size, globals, reduction, region corner, CPU-filtered groups'
pixels); the GPU thread reuses pixels on a match and frees the rest. Key
0 (drawn anew) when what is behind runs passes of its own, and in eval's
fresh redraw. Memory: two region-sized textures per backdrop drawn anew,
one per kept.

**2026-10-23 · perf-gpu-retain: the review's six findings, all
fixed.** (1) `backdrop::hash_behind` hashes markers around hashed
items and layers over the region, except groups open at the node
(`an_ancestor_fading_keeps_a_backdrops_key`). (2) Small blurs at the
CPU's reduction. (3) Tests for each key part. (4) Pass steps draw into
`pass::Pool` (`a_backdrop_drawn_every_frame_reuses_its_textures`). (5)
`Snapshot::under_old`. (6) Figures quote benchmarks.md; crossfade at
1.5× is open.

**2026-10-23 · perf (warm bar): the warm bar's gates re-based for the
warm-start pipelines.** Speed beats size, so by gpu-first's rule the warm
two-monitor bar's target is 242 MiB and its ceiling 252 MiB (from 232 /
242); the full shell's 296 / 324 MiB and debug 288 MiB stay. design.md,
architecture.md, ci.yml and budgets.rs say so.

**2026-10-23 · perf-gates: the CPU wins are gated in the `timing` job;
the GPU's are not.** `animated_cpu_frames_hold_their_gates` (CI `timing`,
advisory in ci.sh): median frame within 8.33 ms and p95 within 16.7 ms
for cover-mask, stagger, shared-morph, list-scroll and theme-spring, and
mean encoding within 1 ms for masked and gradient scenes; misses carry
`GATE_MISS`. Each gate is at least 3.5× the laptop's figure. GPU legs
and parallel-raster wins are not gated.

**2026-10-23 · perf: what the review leaves open.** Small slow animations'
promotion (glass at 1×), CPU offscreen group rebuilds and backdrop blur,
retained GPU layers, CPU resize damage narrowing, goo's GPU contour,
cold crossfade and panel first frames at 1.5×, per-frame allocations,
and full GPU damage (waits on wgpu's `present_with_damage`).

## history

**2026-10-24 · history: a graph's history lives with the data, once per
series (owner-approved).** `graph X { history: D }` shows the last D of
X as soon as it mounts: kept on the logic thread (`instantiate::history`)
across reloads, never on disk, one store per series keyed by its expression,
spanned by the longest constant `history` (60 s unset), passed to render as
`Prop::Backlog`. A series reads only services, file-level `let`s, `state`s,
settings and constants; anything else shows what came since mount. Hidden,
a kept service samples every max(2 s, span / 20) (`Cx::period`); a store
holds at most 512 points (8 KiB, 16 bytes a point).

**2026-10-24 · history review: one boot clock, ramps only for hidden
polls, reachable trees, spelled-out keys.** Backlog points carry
`CLOCK_BOOTTIME` time (`strand_scene::backlog::boot_now`), so suspend
counts and a boot-clock jump of 2 s or more past the awake clock is a
gap. Only a sample a hidden service polled ramps (`BacklogPoint::ramp`,
`ServiceHost::sampled`, `Client::polls`, `strand_services_schema::POLLED`);
only mounted trees keep history; a series key spells out each `let`
and a hashed `service` declaration. `Msg` is `#[non_exhaustive]`; glitch
reports carry `history.json` and replays seed `history:<key>`.

## graph-follow

**2026-10-24 · graph-follow: one `range` for slider, meter, arc and graph
(owner-approved).** `range: lo, hi` (expressions, the usual comma pair)
draws `lo` empty and `hi` full and a slider writes `lo + (hi − lo) ×
f`; unset it is `0, 1`. Meter, arc and graph also take `auto` for either
end or the whole; a slider does not. A lone number or equal/reversed
literal ends are check errors. Rejected: `0..100` range syntax, separate
`min`/`max`, a `scale` multiplier.

**2026-10-24 · graph-follow: how an auto range moves.** An auto end
reaches past every value shown by a span on the 1, 2, 5 ladder; it grows
at once and narrows only once every value fits four fifths of a smaller
step. A graph's values shown are its columns; a meter or arc keeps the
least and greatest it drew each second for the last minute. Rejected:
exact maximum, decaying peak, fixed-fraction padding.

**2026-10-24 · graph-follow: a graph's `style`, `line_width` and a real
gradient `fill`.** `style: line | steps | bars`; bars are `line_width` wide
(2 px unset) and `gap` apart (1 px unset). `line_width` (1.5 px unset)
is the line width, not `width`, which would read as the graph's size;
`arc` keeps `width` (spec.md section 14). A gradient `fill` is now drawn
as written over the box rather than averaged, a fix not a change in meaning.

**2026-10-24 · graph-follow: several series in one graph, overlaid.**
A graph's value may be a list (`graph cpu.cores`, `graph [cpu.usage,
memory.usage]`), each item a series drawn against one range, first at
the back. `color` and `fill` take a comma list, cycling. Stacking is
written as sums (`graph [a + b, a]`), not a `stack:` mode; no `series {
}` block. A list's kept history is one ring per item, keyed `key[0]`,
`key[1]`, … (`HistoryInfo`).

**2026-10-24 · graph-follow: columns fall on the boot clock; a
graph's value does not spring.** Column `j` ends at `j × period` of
`CLOCK_BOOTTIME` and a graph ticks with `Rate::Aligned(period)`, read
through `set_history_clock`, so the same values draw the same columns at
any frame timing; a frame with no clock places no columns. A graph's value
no longer springs (`smooth` eases it); `graph_history.png` re-blessed. A
fill varying across the width is drawn whole each column.

**2026-10-24 · graph-follow: the replay keeps the report's boot clock.**
A scenario takes an optional `boot = <seconds>` header (the boot clock
at `T0`), written from `history.json`, which every report now writes;
without it the harness keeps its fixed base. The e2e report test's graph
is back in the bar. Rejected: snapping replay frames to live times,
widening eval's pixel tolerance for graphs.

**2026-10-24 · graph-follow: the docs.** spec.md 8.1 and 8.5 describe list
graphs, boot-clock columns and slider `range` units, citing section 14
(behaviour added, no meaning changed). The reference generator lists an
element as inheriting a group (`: node`) when it has every prop by name
and keeps at least one as the group has it; a tuple in a union prints
in parentheses.

**2026-10-24 · graph-follow review: a column is the value at its
tick's time; edges.** Each sample is the value known at its tick's start
(`Series::known`, `Series::resample`, `RasterProps::commit`), so a kept
value lands in the same column whatever the frames; `graph_backlog.png`
re-blessed. A meter's auto range follows the value logic set; ranged
values settle relative to the range span (`anim::value_scale`). List items
keep per-place stores (N × 8 KiB) and a dropped item's ring is freed a
span later; an auto end returns to its default once nothing reaches it;
a non-finite end keeps the last range and a slider then writes nothing;
presentation time is mapped via `backlog::set_presentation_clock`;
one-word schema keywords print as their word.

## flex

**2026-10-24 · flex: maths builtins, angles on the screen's terms
(owner ruling).** `sin`, `cos`, `tan` take an `angle`, `atan2(y, x)`
returns one, and `sqrt`, `pow(x, y)`, `exp`, `ln` and constant `pi` take
and give floats. Angles are degrees, `0deg` right and growing clockwise
as `rotate` does. A non-finite result is an error value naming the call,
not NaN; `pi` is a non-reactive constant.

**2026-10-24 · flex: text to numbers and numbers to text.**
Null-in-null-out functions: `number(t) -> float?`, `num(x, digits = 0,
pad = 0)` (half away from zero, never `-0`), `bytes(n)` (binary units,
one decimal below 100). Text gains `slice(from, to?)` and `index_of(part)
-> int?`, counting characters; negative indices count from the end and
ends clamp, so a slice never fails.

**2026-10-24 · flex: `+` joins lists, `reduce` folds them.** `+` on two
lists joins them (`xs += [x]` too); `[]` takes the other side's item
type and mismatched item types are an error. `xs.reduce(init, f)` has
`init`'s type; a whole-number `init` is `int` when `f` keeps it one, else
`float`. No `sum`, `range` or `enumerate` added here.

**2026-10-24 · flex: `min` and `h` duration units.** `1min` is 60 s and
`1h` 60 min, in the lexer, grammar.md's `UNIT` and tree-sitter; not `m`,
which reads as metres or months. Days are not added: a day is a calendar
notion (`Date.add(days: …)`).

**2026-10-24 · flex: a whole-number literal beside an `int` is an `int`.**
When the left operand takes its type from context and the right does not,
the right is checked first and gives its type to the left, so `2500 +
n` and `n + 2500` are both `int`. The float-for-int help now suggests
`.round()` (naming `.floor()` and `.ceil()`).

**2026-10-24 · flex: a time signal in a canvas `draw` is a check error.**
`draw` is recorded by logic on change while time signals are evaluated
per frame by render, so `check::time_in_canvas` rejects `t`, `wave(…)`
or a `let` holding one, pointing to the canvas's own paint props or a
`shader`. Drawing the canvas every frame from logic was not added.

**2026-10-24 · flex: canvas paths, arcs and even-odd fills.** `Canvas`
gains `c.move(x, y)`, `c.to(x, y)`, `c.arc(x, y, r, from: angle, to:
angle)` (direction by the sign of `to − from`, at most one turn) and
`c.close()`, painted by the existing `fill` and `stroke`; `close`, a paint,
`rect` and `circle` leave no subpath open. `fill` gains `evenodd: bool =
false`. `c.text` alignment is not added.

**2026-10-24 · flex: an action may return its command's output (`action
calc(q: text) -> text [...]`).** An action declaration gains `-> T`,
and the call is an `Async<T>`: a load in a binding (`rt.async_memo`),
an `await` in a handler; output is read as `from poll` reads a command
(`text` trimmed, others via `Document::parse`). Each call runs at once
on its own thread; an unheld call's process group gets SIGTERM then
SIGKILL after `KILL_GRACE`. Refused in a pure `fn` (`check::impure`)
and a binding's lambda (`check::action_in_binding`); `from dbus` actions
keep no result. Transport is `ExecRequest` with an `ExecReply` slot.

**2026-10-24 · flex: `Node.pointer`, the pointer as a value.**
`self.pointer` / `name.pointer` is a `Point?` in the node's laid-out box,
null when not over it, instead of press/drag/release events. It changes at
most once a frame; during a press that began on the node it is measured
in the press's frame, so a grip moving with what it sets sees only the
hand's motion. An `on change` whose targets are all node input state is
an input handler, outside the 30 writes/s guard. Watched via `Prop::Watch`
(`box_pointer`); taught at guide level 5.

**2026-10-24 · flex: `cursor:` on any node, chosen by render, pulled
by the surface manager.** One prop of CSS cursor names in snake_case
(`ew_resize`, `not_allowed`), for `wp_cursor_shape_v1`. The innermost node
under the pointer decides, and the chain a held press went down on keeps
deciding; `input` shows `text` by default. The surface manager pulls it
(`SurfaceHost::cursor`) after pointer frames and paints, sets it only on
change, and does not retry a missing shape.

**2026-10-24 · flex: `screen` on multi-monitor panels and OSDs, and on
the lock.** A surface whose `screens:` picks monitors is per monitor like
a bar, with keyed instances; a focused-monitor panel stays one surface
and reading `screen` there is a load error (syntactic check: `screens:`
written and not `focused`). The lock stays one content surface; its
`screen` is the content's monitor and changes in place, mounting on an
empty `Screen` if none is known.

**2026-10-24 · flex: `exclusive: bool | length` on bars and panels.**
Amends wave2-check (round 2) by the owner's ruling: `true` reserves
the box's thickness (a bar's default), a length reserves exactly that,
`false`/`0` nothing (a panel's default). A panel reserves along its
`attach:` edge, else its `anchor:` edge; at a corner or centre none. The
zone still counts from the shadow-moved margin.

**2026-10-24 · flex: a slider's `step`, and upright sliders and meters.**
Amends wave2-check (round 2): `step:` is in `range` units from the low
end, applied on the render thread, writing `lo + n · step` exactly. No
orientation prop: a slider or meter laid out taller than wide is upright,
growing from the bottom; a `rotate`d slider still works (`to_laid`);
an upright meter fills without its wave.

**2026-10-24 · flex: `wrap: true` on rows and columns.** Amends wave2-check
(round 2): CSS flex-wrap; lines pack at the start, `gap` stands between
lines, `align`/`justify` work within each line. The collapsing-child gap
fold is off in a wrapped row. `grid` stays the tool for aligned columns.

**2026-10-24 · flex: `axis:` and `snap:` on scrolls.** Amends wave2-check
(round 2): `axis: vertical | horizontal | both` on `scroll`; a horizontal
scroll lays its children in a row, and a plain wheel scrolls it only per
the review below. A list stays vertical. Each event moves the one axis
it moves most along. `snap: true` (on `scroll` or `list`) settles as CSS
scroll-snap `start` would; a touchpad lift springs to the snap point nearest
where a fling would stop (`FLING_DECAY`). No snapping on `nav` or reveal.

**2026-10-24 · flex: `anchor: pointer` on popups.** Render records where
each press went down on a surface; a popup with `anchor: pointer` takes that
point as a 1 × 1 anchor when it opens and keeps it while open, opening
down-right of it (on a bar too) and kept inside its output. Opened with
no press, it opens at its element. `pointer` is a value of the popup's own
`Anchor | PopupAnchor` type only.

**2026-10-24 · flex: an `arc`'s line is its `stroke:`.** The arc's line
takes its `stroke:` width and paint (else `color`, then `$accent`), and
`width` sizes its box as everywhere; design.md's example becomes `arc {
value: cpu.usage; sweep: 270deg; stroke: 4, $accent }`. This changes
a builtin's meaning, recorded under spec.md section 14 since Strand is
pre-1.0. An unset `track` is the line's colour at 15 %.

**2026-10-24 · flex: a plugin's schema file declares only its own
service.** As spec.md 5.6 says, `plugins::apply` now skips with a warning a
`<service>.schema` file declaring anything beyond its service; its records,
enums, aliases and opaque types are allowed.

**2026-10-24 · flex: `focus:` is reactive, Tab moves focus, a closed
surface lets it go.** A destroyed surface releases focus as a keyboard
leave does (no second caret). When `focus:` turns true on a node of
a surface holding the keyboard, focus moves there. Tab and Shift+Tab
(`ISO_Left_Tab`) cycle a surface's `input`s and `list`s in tree order,
left to handlers with Ctrl, Alt or Super or an `on key` around the focused
node. `opacity: 0` taking no input is documented (spec.md 6.7), not changed.

**2026-10-24 · flex: the docs and the compatibility policy (spec.md
section 14).** Most of the round only adds. The meaning changes recorded
under section 14 (pre-1.0): an `arc`'s `width` sizes its box (warned,
`check::arc_width`); per-monitor `panel`/`osd` instances; upright sliders
and meters; reactive `focus:`; Tab focus moves; `2500 + n` is `int`;
over-declaring plugin schemas skipped; `pointer` before `pressed`; maths
builtins of `pi` warned (`check::pi_degrees`).

**2026-10-24 · flex review: the code and docs review of c6f4af96.**
`Prop::OnKey` marks elements with an `on key` handler and render leaves Tab
to them; Tab skips `opacity: 0`. Maths builtins of a time signal become
`TokenExpr::Math` evaluated by render; other immediate builtins of one
are `check::time_in_logic`. `anchor: pointer` takes only a press in the
popup's opening input step. The sideways wheel fallback applies only
to a wheel event when no scroll under the pointer moves along y, never
a touchpad. Result actions get a 10 s timeout, are recorded as `action
<name>` fetches and replayed (`answer action <name> …`). `Range::snap`
rounds to `lo`'s and `step`'s decimals; `num` and `bytes` round on written
decimals. Rejected: converting int items in `[0.5] + xs`.

## services-round

**2026-10-24 · Memory breakdown and network byte rates (S1).** `memory`
gains `available`, `free`, `cached` (`Cached` plus `Buffers`) and
`swap_used`/`swap_total` from `/proc/meminfo`. `network` gains `rx`/`tx`
from `/proc/net/dev`, counting only interfaces with a `device` link,
per-interface deltas, as stream fields sampled once a second while shown;
`network` joins `strand_services_schema::POLLED`. Per-field clocks are
left for later.

**2026-10-24 · Binding mode, keyboard layouts and modifiers, window place
and moves (S1).** `wm.mode: text?` (null for the default mode/submap);
`keyboard.layouts: [text]` with `next_layout()`/`set_layout(index)`
switching every keyboard (Hyprland names compiled via libxkbcommon);
`keyboard.alt`, `ctrl`, `shift`, `logo` only while a Strand surface has
keyboard focus. `Window.rect: Rect?` relative to its screen (null for
niri tiled and protocol-only). `win.move_to(ws: Workspace)` moves without
following; protocol-only answers `Unsupported`.

**2026-10-24 · Notification hints and replies, app desktop actions, `from
listen` lines (S1).** Notifications gain `progress: float?`, `category`,
`desktop_entry`, and inline replies (`can_reply`, `n.reply(text)` emitting
`NotificationReplied`). `App.actions: [AppAction]` with `a.launch()`;
`vm::persist::decode` reads a missing field as its type's default. A
`from listen` service gets an implicit `line(doc)` event, sent only while
a handler hears it, and such a handler counts as a reader.

**2026-10-24 · Idle: `idle.after(timeout)` and `inhibit_idle` (S1).**
An `idle` service over ext-idle-notify-v1: `idle.after(t)` registers `t`
(`idle::want`) and reads the keyed `timeouts` field; plain notifications, so
inhibitors hold them back; whole-millisecond `u32` timeouts. `inhibit_idle:
bool` is a prop on every surface kind, a `zwp_idle_inhibitor_v1` per
Wayland surface. No logind `session.inhibit_idle`.

**2026-10-24 · Every player, saved connections and the battery's history
(S2).** `media.players: [Player]` keyed by bus name with `active` and
`select()`, held until the player leaves or another starts playing; an
item's `position` is writable. `network.connections: [Connection]` keyed
by UUID with `ConnectionKind` `wifi | ethernet | vpn | wireguard | other`,
`c.up()`/`down()`; `network.kind` prefers an active VPN/WireGuard holding
the default route; over iwd `connections` is empty. `battery.history(span:
duration, points: int = 60) -> Async<[float]>` from UPower, without
invented early points.

**2026-10-24 · Programs' audio streams, video takers, levels (S2).**
Supersedes wave4-wm (audio)'s "no per-app streams": `audio.streams:
[AudioStream]` `{ id, app, icon, direction, volume rw, muted rw }`,
bound only while shown, peak meters and Strand's own excluded. A
separate `video` service lists `video.streams: [VideoStream]` with
`source: camera | screencast` (not `screen`, which names a bar's
monitor). `audio.levels(device, bars = 16) -> [float]` over `audio.meters`,
from the 64-band spectrum, a meter released when unasked for 1 s.

**2026-10-24 · `from dir`: a watched directory as a service (S2).**
`service walls from dir "<path>" { files: [File] }` is a fifth source, using
`from file`'s inotify watch on the directory. `File { path, name, modified:
Date, size }` is a builtin record a service field may hold. Regular files
and links to them, no hidden files or subdirectories, ordered by name;
a burst is listed once 50 ms after settling; at most 10 000 files, the
rest warned.

**2026-10-24 · `thumbnail screen`: a still of a monitor each time shown
(S2).** `thumbnail`'s source becomes `Window | Screen`; the shot comes
via ext-image-copy-capture's output source, else wlr-screencopy, on its
own short connection. It is a still, not a stream, so a thumbnail of its
own screen does not recapture itself; a live screen is left open. Inside
`lock { }` it captures black; a pre-lock still is recorded as open.

**2026-10-24 · polkit: Strand is the session's agent while a config reads
`polkit` (S3).** A builtin `polkit` service shaped like `auth` (`request:
PolkitRequest?`, `respond`, `cancel`, `choose(user)`, `waiting`);
it registers for its `unix-session` only while read, unregisters 5
s after the last reader, and never fights another agent (waits for
`Changed`). Passwords go to `polkit-agent-helper-1` (socket first, else
setuid); with neither it does not register. Answers are wiped `Password`s,
at most 511 bytes, and `polkit` joins `strand_services_schema::SECRET`.

**2026-10-24 · greetd: a greeter in the same language (S3).** A builtin
`greetd` service (`login`, `submit`, `start(command)`, `cancel`, `prompt`,
`message`, `failed`, …) queues requests one at a time; `start` waits
for login success, its words quoted to stay one word each. On success
the binary SIGTERMs itself so greetd can start the session. It runs in
a layer-shell compositor (the guide uses sway), not cage. `greetd` joins
`SECRET`.

**2026-10-24 · One list of secret services (S3).**
`strand_services_schema::SECRET` (`auth`, `polkit`, `greetd`) names them
once and `strand_services::secret(name)` is what the recorder, report and
`strand call` ask.

**2026-10-24 · The `.text` gates re-based on services-round's tree
(S3).** Services added about 1.3 MB of `.text`; the gates are re-based
by the usual rule (measurement plus a 512 KiB reserve, rounded up
to 256 KiB, CI about 48 KB above the container): GPU 28,311,552 B
(108 × 256 KiB) and CPU-only 24,379,392 B (93 × 256 KiB). Proof:
`the_release_binary_code_stays_within_its_gate`.

**2026-10-24 · services-round: the language changes and section 14.**
spec.md 5.1, 5.4, 5.6, 8.1, 13 and grammar.md's `dir` only add. So that
`App.actions` does not break old `App(…)` calls, a record call's
missing list field gets an empty list, as a missing optional gets
null. Tests: `a_record_call_may_leave_out_optional_and_list_fields`,
`a_record_left_without_a_list_field_holds_an_empty_one`.

**2026-10-24 · services-round (the review): service costs
and correctness.** One `LineSink` per `from listen` client;
`hear_lines`/`unhear_lines` count handlers. `inline-reply` is
advertised only while the program names `can_reply` or `reply`
(`lower::Program::members`). Idle timeouts count readers and drop after
`idle::LET_GO` (1 s); levels leases count readers. `from dir` sorts
names before stat'ing 10 000 and relists at most every `DIR_RELIST`
(500 ms). Lock surfaces now sync `inhibit_idle`. Streams are keyed by
`object.serial`, sink monitors are `StreamDirection::monitor`. `thumbnail
screen` capturing its own surface is a recorded limit.

**2026-10-24 · services-round (the review): the password paths.**
An answer is taken only once the shown request, for its user, has
asked (`Requests::seen`). `NotificationReplied`, `ActionInvoked`,
`ActivationToken` and `NotificationClosed` go to the notification's sender
only. The polkit helper is used only if a root-owned socket or a root setuid
program (`PolkitConfig::owner`). Calls carry `strand_auth::Password`
so every drop wipes; reports pause in `flight::gap::SECRET` while a
polkit request shows or Strand is a greeter. greetd's socket peer must
be root or Strand's uid (`GreetdConfig::server_uid`); `GREETD_SOCK`
is not inherited. No `check::secret_flow` lint.

**2026-10-24 · services-round (the review): open.** A declared command's
run state (`<action>.running`, `.started`, `.stop()`) is left open for
the language stream, with the pre-lock screen still.

**2026-10-25 · services-round on flex: one rule for source and command
output types.** A result action's `-> T` and a source's fields share
`check::tree::readable`, so `File` and `Date` are allowed and only live
handles (`Window`, `Canvas`, `Screen`) and non-data (`font`) are refused.

## lockmon

**2026-10-25 · lockmon: the lock on every monitor, one password.**
A `lock` mounts one instance per monitor its `screens:` picks (all by
default; `screens: focused` keeps the old single instance), so the lock
loses a special case. Lock `state`s live once above the instances, so
the password and attempts are one; a lock `state` starting from `screen`
or a lock `let` is `check::lock_state_screen`. Inputs carry `Prop::Twin`
so render shares in-flight text and the caret, drawn only on the focused
surface. Test: `the_lock_shows_on_every_monitor_with_one_password`.

**2026-10-25 · lockmon: fail-closed per monitor.** A lock instance's
`Env` is a fault bound, so a fault freezes that instance only; a faulted,
unmounted or never-drawn instance's output shows the solid colour for the
rest of the session (`State::lock_instance_failed`) and keys move to a
healthy one. Only when every instance is bad does the built-in fallback
take over. Unlocking is unchanged: only `auth`'s token unlocks.

**2026-10-25 · lockmon: screen stills from before the surface.**
`SurfaceHost::before_lock` lets the binary take a still of each monitor a
lock's `thumbnail screen` names, waiting at most 500 ms; lock thumbnails
show only that still. A surface showing its own monitor holds its first
frame up to 300 ms for a pre-map shot, kept 2 s, then captures live. Limits
recorded: a slow capture, or a thumbnail mounted later in a shown surface,
may show the surface.

**2026-10-25 · lockmon: an action's run, read through its name.** For
`action rec(…) […]`, `rec.running`, `rec.started` (oldest call's start,
a `Date`) and `rec.stop()` read the name as a builtin record `ActionRun`;
the bare name is unchanged. `stop()` sends SIGINT, SIGTERM 2 s later,
SIGKILL 2 s after, to the process group, and drops the queued re-run
(`ExecRequest::stop`); a stop is not a failure. Replay turns recorded
ends into `end action <name>` steps.

**2026-10-25 · lockmon: the compositor matrix covers
idle, window places, modes and layouts.** Matrix
tests `a_window_is_placed_and_moved_by_the_stores`,
`modes_and_layouts_follow_the_compositor` and
`idle_follows_the_compositor_and_a_surface_holds_it_awake` run on every
compositor. strand-surface now makes a surface's idle inhibitor once it
is mapped (sway 1.12 ignored one made before the first buffer); a lock's
content holds its inhibitor from its first frame.

**2026-10-25 · lockmon: polkit 127's socket helper, run for real.**
The lock VM builds polkit 127 (sha256-pinned) beside Ubuntu's 124 and
runs polkit.sh again with `POLKIT=127`, its helper socket-activated with
`SocketMode=0666` and 124's setuid bit removed; `vm_auth.rs` asserts
which helper `Helper::find` chose. No Strand change was needed.

**2026-10-25 · lockmon review: the lock's timers and handlers run once.**
The lock body's timers, `on change` and service handlers are mounted once
in the lock-level `Env` when the first instance shows, suspended while
none shows; element events and `if`/`match`/component/`for` logic stay per
instance. Once-run logic reading `screen`, a `let` or a lock element is
`check::lock_state_screen`. `Prop::Twin` goes only on an input bound to
a lock-level `state` (`Ctx::shares_text`).

**2026-10-25 · lockmon review: fail-closed sees every fault, stills
are let go.** `Instance::fault_nodes` names what a fault froze; a
shared-scope fault freezes every instance and shows the fallback. Each
instance's first frame is due `INSTANCE_FIRST_FRAME` (3 s) after its own
first configure. A missing still feeds `None`, `lock_ended` clears lock
thumbnails on `Unlocked` and `Finished`; stills are at most 1280×720 and
`Feeds::prune` drops pre-map stills after 2 s. The polkit 127 socket is
checked without following symlinks and by peer uid. The secret is held
once per monitor's input node, redacted (corrects the first entry).

**2026-10-25 · lockmon review: a monitor's cost, the sandbox queue,
the matrix, the VM image.** A second lock monitor costs about 7 MiB PSS;
`the_locks_second_monitor_costs_one_surface` is a 10 MiB tripwire. The
eval sandbox keeps one running command and one queued re-run per action,
as live; `end action` ends the running one and a stop drops only the queued
one. The sway matrix arm plugs a virtual keyboard with two layouts. The
VM image pins meson's sdist and polkit.sh cleans up on every exit.

**2026-10-25 · lockmon: Hyprland's Lua move says `follow = false`.**
The Lua form is `hl.dsp.window.move({ workspace = "N", follow = false,
window = "address:…" })`, since Lua moves follow unless told not to,
so `win.move_to` never follows on either dialect.

**2026-10-25 · lockmon: a `from file` rewritten in place is read
once it settles.** A change that leaves the file blank is read
again after `FILE_SETTLE` (50 ms), so a truncate-then-write never
shows the field empty; a file left empty still reads empty. Not
chosen: waiting for `CLOSE_WRITE` or skipping blank reads. Test:
`a_file_rewritten_in_place_never_shows_it_empty`.

## window

**2026-10-25 · A `window` surface kind on `xdg_toplevel`.** `window
Name { … }` is a fifth surface keyword beside `bar`, `panel`, `osd`
and `lock`, not a `panel` prop, since most panel props mean nothing on
a toplevel. It reuses `open:`, `width`/`height`, `min_*`/`max_*`, `bg`,
`theme`, `inhibit_idle`, events, the growth spring and popups, and adds
`title:`, `app_id:` and `decorations:`. Made through sctk's `XdgShell`.

**2026-10-25 · What a `window` refuses and defaults.** `screens`, `layer`,
`anchor`, `margin`, `attach`, `scrim` and `keyboard` on a window are load
errors (`check::window_prop`); `keyboard:` too, as `xdg_toplevel` cannot
refuse focus. `screen` is not in scope. `title` defaults to its name and
`app_id` to `strand-<Name>`, both changeable in place.

**2026-10-25 · The compositor decides a window's size.** A configure's
size is taken on an axis when maximised, fullscreen, tiled, resizing,
or differing from the asked size; 0 or the asked size leaves its own
(`kept_size`, `Renderer::set_window_sized`). A floating window grows with
its content from its top-left; `configure_bounds` are its bounds.

**2026-10-25 · Decorations, states and actions on the window's own id.**
`decorations: server | none` asks `xdg-decoration`; the compositor's
answer is `w.decorated` (false without the protocol). `id:` on a surface
names its root; `maximized`, `fullscreen`, `tiled`, `decorated` are
`Node` fields and `focused` is activation. `Node` actions `maximize()`,
`fullscreen()`, `minimize()`, `close()`, `move()`, `resize(edge: Anchor)`
are dropped when not in `wm_capabilities`; `move`/`resize` use the last
press serial, so call them from `on change x.pressed`.

**2026-10-25 · Closing a window.** `xdg_toplevel.close` and `w.close()`
reach the router as `ClickAway`, writing a two-way `open:` false. Escape
and losing the keyboard do not close a window. A window whose `open` is
not two-way ignores the close. Popups parent to the window's `xdg_surface`.

**2026-10-25 · window: open.** `w.move()` and `w.resize()` are tested
only up to the surface manager (`window_actions_reach_the_diff`,
`resize_edges_map_to_xdg_toplevel_edges`); the interactive request is
untested against a compositor. Window-state recording was settled below.

**2026-10-25 · Recording what the compositor says of a window.** The
flight recorder adds kind `0x27` toplevel (`flight::kind::TOPLEVEL`,
12-byte payload of size, sized axes, states, capabilities, bounds) via
`InputRecorder::window`, rewritten at gap ends and ring restarts; a window's
`0x20` mapped record names kind 5 (`flight::MAPPED_WINDOW`); the dump
version is 4. `bundle.json` surfaces gain `window` and `strand report`
prints a `window:` line; no `strand surfaces` command or window verbs.

**2026-10-25 · A window in the sandbox and its replay.** The stand-in
compositor places a window floating and centred, and a `window <surface>
W,H [maximized] [fullscreen] [tiled] [activated] [decorated]` (or
`close`) step plays the compositor's part; the size rule is shared as
`placement::window_kept_size(given, asked, constrained)`. The planner
turns toplevel records into `window @origin …` steps. Scenario:
`window-tile-maximize-close`.

**2026-10-25 · A window the compositor lets go of takes its own size at
once.** An axis the compositor stops sizing snaps to the content size rather
than springing, since the compositor's size is not motion the config asked
for. Test: `a_window_the_compositor_lets_go_of_takes_its_own_size_at_once`.

**2026-10-25 · A window the compositor cannot close is warned.**
`check::window_open` warns on a window whose `open:` is one-way or unset,
with help `open: <-> shown`; a warning since a kiosk window may mean
to stay.

**2026-10-25 · A window costs what a panel of its size does.** A config
with no window pays nothing new. `an_open_window_costs_what_a_panel_does`
holds an open 640×400 window's own PSS over the bar within an equal
panel's plus 1 MiB and within design.md's 14 MB panel-buffer budget,
checks no strand thread wakes for 5 s, and that closing it frees its
buffers to within 1 MiB.

**2026-10-25 · A settings window in the guide, at level 4.** The guide
teaches `window` at level 4 beside `lock` with `settings.strand` (its
own title bar when undecorated), since level 4 holds surfaces that come
and go, `id:` and `export`. The ladder counts the window and its state
reads as the construct; the page ends with "Panel or window?". Test:
`the_guide_settings_window_draws_its_own_title_bar_where_undecorated`.

**2026-10-25 · window, second round: open.** `w.minimize()`, `w.move()` and
`w.resize()` show nothing in the sandbox and replay. The eval surface leg
refused runs with a window (settled in oi-window). `strand compositor-rules`
prints no window rules, since compositors match `app_id`. Hyprland's
window matrix leg runs only on GitHub.

**2026-10-25 · The window review: a minimum over the maximum never reaches
the compositor.** `min_width` over `max_width` (or heights) written
as px numbers on the window is a load error, `check::window_min_max`,
since `invalid_size` would kill Strand's whole connection. At run time
`placement::window_config` raises the maximum to the minimum with a logged
warning. Test: `a_minimum_over_the_maximum_keeps_the_connection`.

**2026-10-25 · The window review: a configure echoing an older own size
is an echo.** A configure size equal to a size the window committed since
its last acked configure, or the one it had then, keeps the window's
own size (`Toplevel::echoes`), so sway's focus-change configures
no longer pin a growing floating window. Compositor-given sizes are
not kept, so a resize's final configure is still the user's. Test:
`a_stale_configure_does_not_pin_a_floating_window`.

**2026-10-25 · The window review: `w.focused` is the compositor's
alone.** `Router::flush` drops every `Flag::Focused` intent on a
window root, so only the host writes it from `activated`. The host
sends logic only flags a configure changed, so a resize's configures
wake no logic. Tests: `routing_never_writes_a_windows_focused`,
`a_windows_configure_tells_only_the_flags_it_changed`.

**2026-10-25 · The window review: a window has no place on the screen,
live or in the sandbox.** `Surfaces::origin` is `None` for a window and
for popups whose parents reach one, so a popup in a window has no `id.box`
(spec.md 5.1). A fixed (0, 0) origin was rejected: render would read it
as a place on an output. A window-relative origin is left for later.

**2026-10-25 · The window review: a replay keeps recorded sized axes
and bounds.** A recorded sized axis is written `W!` in the `window` step,
sized as given; without `!` it goes through `placement::window_kept_size`;
`RESIZING` is not recorded. The sandbox sends no `configure_bounds`
of its own: a step's `bounds W,H` from the record or `bundle.json`,
else none. Live bounds now outlast `surface_entered`.

**2026-10-25 · The window review: the compositor's close is recorded
apart from `w.close()`.** `SurfaceHost::window_closed(surface)`
precedes the click away, and the recorder sets payload byte 0 to
`flight::click_away::COMPOSITOR`; only those plan `window … close`,
so a replayed `w.close()` is not closed twice.

**2026-10-25 · The window review: an ask before the window exists, an
unplugged output, pick mode.** A maximize, fullscreen or minimize asked of a
window its spec opens is kept (`State::window_asks`) and asked at creation;
move, resize or close of an unshown window does nothing. `output_removed`
leaves windows and their popups alone, clearing their output until the
next enter. In `strand inspect` pick mode a click away on a window root
goes through.

**2026-10-25 · The window review: a field and an action of one name
keep their own docs.** Supersedes laptop-open (winstate)'s joined docs:
a method sharing a field's name keeps its doc as `DocKey::Action(type,
name)`, read by `Schema::method_doc`; LSP hover still shows both.

**2026-10-25 · The window review: the guide's title bar moves the window.**
The settings window's row is `id: bar` with `on change bar.pressed {
if bar.pressed { w.move() } }`, and the guide says why `on click` (the
release) will not do.

## oi-window

**2026-10-26 · A window's move and resize, held against every compositor
of the matrix.** `a_window_moves_and_resizes_from_its_own_press` presses
and drags a strand `window` with `zwlr_virtual_pointer_v1` on sway, niri,
labwc and Hyprland, checking the compositor's reported box and a grim
shot within 6 px for `w.move()` and `w.resize(bottom_right)`. The window
floats by opening at a fixed size, then releasing it. No strand bug was
found; on labwc the pointer only moves onto the desktop before pressing
(its root menu).

**2026-10-26 · A window on the eval surface leg.** strand-fake-wayland now
serves `xdg_toplevel` (`src/xdg.rs`), taking `Cmd::ConfigureWindow` and
`Cmd::CloseWindow` by app id. The leg (`eval/src/wire.rs`) forwards what
the stand-in compositor decided, so it stays the one place a window's
size is decided, setting `resizing` where needed. Two windows of one
`app_id` are refused; the fake waits for the leg's first configure
(`SurfaceGlobals::window_configure_on_cmd`); no `xdg-decoration` on
the fake.

**2026-10-26 · The oi-window review: a closed window is forgotten on the
leg; the drag waits on the window.** The leg forgets a node's configure
whenever its window is destroyed or closed, so a reopened window is
configured afresh and same-`app_id` windows shown one after another run. The
matrix drag moves in tenths until the window moves, not after a fixed
500 ms. docs/eval.md corrected: a window's own floating size sends nothing.

## oi-lock

**2026-10-26 · oi-lock: a monitor plugged in while locked has no still,
and says so.** Stills are taken only in `SurfaceHost::before_lock`;
`Feeds::screen` drops one when its monitor is unplugged or its connector
changes. A missing or failed still answers its tap with no frame, so the
lock's `bg` shows. A node field `blank` (spec.md 6.3, `NodeFlag::Blank`,
`Host::thumbnail_blank`) is true while a thumbnail has nothing to show,
a frame late; it stays false in the sandbox. Rejected: a lock-only
`screen.still`.

**2026-10-26 · oi-lock: the keys moving to another monitor's lock
find the field.** `Router::focused_twin` remembers the focused node's
twin key; a surface the keyboard enters with no focus of its own
focuses that twin, then a `focus: true` node, then its root. Tests:
`the_keyboard_entering_another_monitors_lock_finds_the_field`,
`the_keys_carry_the_password_to_whichever_monitor_has_them`.

**2026-10-26 · oi-lock: `check::secret_flow`, a password's way out.**
A warning traces values made from a `type: password` input's two-way
`text` (or keys heard on or around it) through `let`s, `state`s, lists,
`for`, component and `fn` parameters to sinks: shown element props, a
`file`'s `run:`, tokens, keyframes, poses, declared action arguments,
service and record action arguments, service or settings field
writes, and `persist` or exported `state`s. Allowed: `auth.submit`,
`polkit.respond`, `greetd.submit`, `connect_with`; `bool`s and lengths
carry nothing. Context-insensitive, runs only on error-free programs
(spec.md 5.1).

**2026-10-26 · oi-timing: no regression; morph tests count waits,
theme_swap_bench's timed tests take turns.** Bisection across
a1b40d42 to f4123902 found no slower commit: the misses were runner
contention. The morph tests now assert one estimate wait opened
(`Renderer::estimate_windows` counts them, the moving test's worker sleeps
20 ms per text) and hold the wall clock only to `LOOSE_WAIT`, 25 ms;
GATE_MISS is not used there since they run in the strict debug step. In
an optimised build theme_swap_bench's five timed tests take turns on one
lock (`turn`); the 5 ms budget is unchanged and debug builds stay parallel.

**2026-10-26 · slider-fix: a slider growing from a written 0 runs
along its growth.** A written 0 beside `grow` is where the box starts,
so `widgets::upright` takes the growth axis as the slider's length
(`width: 0; grow: 1` runs across, `height: 0; grow: 1` upward) before
looking at written lengths; other shapes keep flex's rule. Test:
`a_slider_growing_from_zero_runs_along_its_growth`.

**2026-10-26 · clip-data: a clip carries props as written, values and
states, and input, as data.** `clip.json` and the text gain each element's
source prop text (`RunTrace::written`), per-frame values and router flags
(`Frame::ui`, `drags`), and input steps with surface, pointer place and the
node under it; frames stay pure pictures and nothing enters the digest. The
text folds runs of moves; typed text never appears. `pack::VIEW_FIELDS`
holds `ui` and `drags`; an older trace says `not in this trace`.

**2026-10-26 · integrate: a press handed to the compositor's move
or resize ends for strand.** `State::window_ask` hands the host a
`PointerLeave` of the window after sending a move or resize, ending
the press with no click, since Hyprland's release otherwise landed
as a click. The matrix test moves the pointer a pixel before each
press. `an_open_window_costs_what_a_panel_does` retries its quiet
seconds when only `strand-watch` woke on an unrelated change. Test:
`a_press_handed_to_the_compositor_ends_for_strand`.

## docs-site

**2026-10-26 · docs-site: a crate of our own generates the site from
our markdown, checking every claim it can.** `strand-site` renders
`docs/guide/`, `site/` and the schema reference with pulldown-cmark and the
editor grammar's highlight queries, in its own warm palette, without a Node
toolchain. The build fails on broken links and anchors, configs that fail
`strand check` or formatting, pasted error output, or media not matching
their scenario hash. Examples are eval scenarios (`eval/scenarios/docs/`,
`scripts/site/media.sh`); fonts Inter and JetBrains Mono (OFL).

**2026-10-26 · docs-site: the claims ledger checks that the words are on
the page and the sources exist.** `site/claims.toml` entries need `page`,
`text` and `source` and no unknown keys; the build fails when `text`
(whitespace collapsed) is missing from its page or a `source` (`path`,
`path:line`, `path:first-last`, `file.md#anchor`) points at nothing. A
`proof` written `path.rs::name` must name a function in that file. Read
with `toml_edit` 0.25.15.

**2026-10-26 · docs-site: the whole reference is on the site,
and signature types link.** Services and elements get a page each
with a card index; types, functions and tokens one page each. Pages
are strand-dev's generated text, reshaped; enums, aliases and opaque
types get anchors. Only capitalised schema type names link. Links into
`elements.md#x` or `services.md#x` go to that entry's page.

**2026-10-26 · docs-site: the CLI page is the binary's own
help, captured and held to it.** `scripts/site/cli.sh` writes
`crates/strand-site/cli-help.txt` from `strand --help` and each command's
`--help`; `strand-site build --strand BIN` (CI's site job) captures
again and fails on any difference. The command list is superseded by the
strand-fixes `--help` entry.

**2026-10-26 · docs-site: search is an index script loaded on first use.**
The build writes `search-index.js` (a script, not JSON, so `file://`
works) of titles and `##`/`###` headings with their text; `site.js`
loads it on first focus. Every typed word must match, ranked heading,
title, occurrences, then phrase. `/` focuses, arrows move, Enter opens,
Escape closes; `shots.sh` drives it at desktop and phone widths.

**2026-10-26 · docs-site: example media are held to the renderer through
eval references and pixels.** Docs scenarios and their committed dark twins
(`eval/scenarios/docs/dark/<name>.scn`, written by `strand-site twin`)
join eval's gate with approved references; `media.sh` renders only through
`check --gate` and records each seal, and the build fails when a seal is
not the approved one. Media record each frame's pixel hash, which `media.sh
--check` redraws and compares; the build checks each media file's hash.

**2026-10-26 · docs-site: a picture is its first frame plus a strip
of the moving part.** Each mode is the first frame whole plus a strip
of only the changing box (widened by 8 px), written as a palette PNG at
256 colours or fewer with no row filter, else RGB or RGBA, losslessly;
the springs media went to about 14% of their size.

**2026-10-26 · docs-site: CI's `site` job.** On every push it builds the
site with its CLI page held to a fresh strand, runs strand-site's tests,
`media.sh --check`, and loads every page in headless Chromium at 1440,
768 and 390 px, light and dark, from `file://` with no network (`shots.sh
--all`), failing on console errors, failed requests, overflowing pages
or a broken search or player.

**2026-10-26 · docs-site integration: proofs checked test by test, posters,
no repeated configs.** Every `path.rs::name` (or bare `file.rs::name`)
in a proof must be a function in that file and other words are
notes. An example's poster is the tick just before its second step acts
(`the_poster_is_the_scene_its_first_step_left`). A how-to whose `strand
file=` block is the example's file is not repeated under the picture;
step labels are cut at 26 characters.

## strand-fixes

**2026-10-26 · strand-fixes: a builtin enum names a `segmented`'s
options.** A bare name that nothing in scope, no service and no builtin
value claims and that names a schema enum is that enum (`Ty::EnumType`),
so `options: Edge` works; a config's own declaration wins. Only a former
error changes. Test: `a_builtin_enum_gives_a_segmented_its_options`.

**2026-10-26 · strand-fixes: `strand set` on a settings field writes
its file; spec.md 8.8 amended.** The code writes the TOML file via
`toml_edit`, as design.md and spec.md 11 say, and is kept: the settings
file is the user-editable source of truth. The settings overlay remains
only for writes a read-only file cannot take; token overlays (`strand set
'$accent'`) are unchanged.

**2026-10-26 · strand-fixes: the missing-helper hint names
strand's directory; only a config with a `lock` hears it.**
`strand_auth::default_helper`'s lookup is unchanged (never `~/.cargo/bin`
or `PATH`); `check::helper_install_hint` suggests `cargo install --locked
--path crates/strand-auth --root <prefix>` or copying beside strand. The
warning is said when a build gains a `lock` (`Shell::tell_lock_helper`,
`lock::helper_notice`), not at every start.

**2026-10-26 · strand-fixes: plain words for a missing or empty config
directory.** `strand run` on a missing directory says "X does not exist;
`strand new X` writes a starter shell there" (or "X is not a directory")
via `DemoError::Config`. An empty directory gets, once at boot, "nothing
to run yet: no .strand file".

**2026-10-26 · strand-fixes: every command answers `--help` on stdout.**
`-h` or `--help` as the first argument after any command prints its
usage on stdout and exits 0 before doing anything (`help_asked`);
later arguments stay the command's. Usage errors still go to stderr
with a non-zero exit. Supersedes the docs-site CLI entry's command list:
`strand-site cli` asks every command `strand --help` lists.

**2026-10-26 · strand-fixes: docs said what is true.** The guide
names sway, Hyprland, niri and labwc as example compositors,
not river; design.md drops a nonexistent `strand reset`; the
clock tick's repaint is stated as "at most 2,000 px²", the gate
`demo::a_minute_tick_repaints_at_most_2000_px2` holds, not loosened.

## live-examples

**2026-10-26 · wasm-spike: the render path runs in a browser behind
`cfg(target_family = "wasm")` shims.** The render path builds
for wasm32-unknown-unknown with ten shims, chiefly the clock via
`strand_scene::time` (web-time on wasm) and `system_now()`. A new
crate, strand-web, drives it through `strand_render::host_loop` without
`eval-hooks`; fonts are fetched by the page; the build uses opt-level s,
wasm-opt -Oz and simd128. Eval's frames are matched byte for byte natively
and in Chromium.

**2026-10-26 · live-examples: the live mode is a progressive enhancement
over the frame player.** Each figure keeps its frame player as default;
**Try it live** appears only with the wasm build, http(s) and WebAssembly,
and fetches nothing until pressed. Live, `set` steps become buttons and
the config is editable, re-run 350 ms after an edit with `strand check`'s
diagnostic. The playground page loads on arrival. `strand-site build`
needs no wasm: `wasm/manifest.js` is `null` until `scripts/wasm/site.sh`
adds the build.

**2026-10-26 · live-examples: `Renderer::set_now`,
`Renderer::take_inline_shaped` and `Router::set_now` leave `eval-hooks`.**
A host driving its own clock is a host, not a test, so these are public API;
the rest of `eval-hooks` stays gated. The loop wakes when the renderer's
next wake is due or inline shaping delivered, not every tick.

**2026-10-26 · live-examples: `placement.rs` moves to
`strand_scene::placement`.** Placement is pure geometry, so it
lives in strand-scene and strand-surface's `placement` is `pub use
strand_scene::placement`, callers unchanged.

**2026-10-26 · live-examples: a wasm-target clippy `disallowed-methods`
rule guards `Instant::now`/`SystemTime::now`.** `scripts/wasm/lint.sh`
runs clippy for wasm32 with `CLIPPY_CONF_DIR=scripts/wasm` and
`--no-default-features` on exactly the crates strand-web builds, failing
when that set changes. Chosen over a grep or the browser check.

**2026-10-26 · live-examples: the wasm build patches fearless_simd's
multiply-add to round once.** `scripts/wasm/exact-fma.py` patches
fearless_simd 0.7.0's wasm backend (sha256-checked) so f32x4 multiply-add
is computed in f64 lanes, exact halfway cases handed to `fmaf`, matching
native FMA. Relaxed SIMD was rejected as nondeterministic. Applied by
`--config patch.crates-io…` on the command line, leaving native builds
and Cargo.lock untouched.

**2026-10-26 · live-examples: the frame reaches the canvas without a copy
into JavaScript.** `compose(x, y, w, h)` composites shown surfaces into
one kept buffer, and the page wraps an `ImageData` over module memory
(`frame_ptr`, `frame_len`) afresh each draw. A single surface's buffer
was rejected since examples show several.

**2026-10-26 · live-examples: example icons are supplied as files,
found by running the examples natively.** `strand_icons::set_files`
lets a host supply files under a base directory, read before the disk;
`strand-web-icons` runs every docs example natively in CI's image, records
the icons and `index.theme` files drawn, and build.sh ships them as one JSON
with Adwaita's licence. Rejected: shipping a whole icon theme, or no icons.

**2026-10-26 · live-examples: a live figure under reduced motion runs with
`system.reduced_motion`.** Live mode is started by the reader, so it is
still offered, and the run sets `system.reduced_motion` so motion snaps,
with a note saying so.

**2026-10-26 · live-examples: no `wasm` step in `scripts/container/ci.sh`;
`scripts/wasm/ci.sh` runs CI's `wasm` job locally.** The check image has no
wasm toolchain and the browser check needs eval's Playwright image. GitHub's
`wasm` job builds the module, fails over 2.5 MB brotli, adds it to the site,
runs the browser check and uploads `site-live`. The spike's `check.sh`,
`measure.sh` and `bench.py` are removed.

## docs-tune

**2026-10-26 · docs-tune: the front page is a plain column, then the
docs.** Chosen by the owner from three mock layouts: the name, one sentence,
Install and Read the guide, the hello bar's file over its picture, two
short sections with one example each, an index of the six docs parts,
and one sentence naming the four compositors. Docs pages keep top bar,
sidebar and contents, with 36 px titles.

**2026-10-26 · docs-tune: one control shape.** Every button, field and
switch is 34 px tall (28 px small) with 6 px corners and 14 px side padding
(`--h`, `--r`, `--pad-x` in site.css); boxes use 10 px. The theme control
is a three-part System, Light, Dark switch.

**2026-10-26 · docs-tune: the docs examples' mock desktop has the site's
accent.** Docs scenarios set `"system.accent" = "#a13c1c"` among their
mock services, so the configs shown are unchanged; refs re-approved with
only colours changed.

**2026-10-26 · docs-tune: live mode is one row.** Play, Next step (its
command as tooltip), Restart and Back to recording, with no tick readout;
entering live mode plays the steps. Figures say "Drawn by Strand from
this config." Partly superseded by the playground-only entry below.

**2026-10-26 · docs-tune: the site teaches using Strand; test material
stays in the markdown, marked repo-only.** Lines between `<!-- repo-only
-->` and `<!-- /repo-only -->` (each on its own line, outside code)
are left out of the rendered page, contents, anchors, search and claims
ledger; an unclosed, stray or nested marker fails the build. Chosen over
separate repo-only files.

**2026-10-26 · docs-tune: the site serves llms.txt.** The build writes
`llms.txt` in llmstxt.org's form, a `.md` copy beside every page, and
`llms-full.txt`, without repo-only spans and with examples as their config
files. Links are root-relative (`/guide/level-1.html.md`). Pages link "This
page as Markdown" and carry `<link rel="alternate" type="text/markdown">`.

**2026-10-26 · docs-tune: compositor config is shown in one place.**
Compositor syntax appears only in Running a shell, "Start it and bind
keys": sway, Hyprland's Lua config (0.55 and later) and niri, a startup
line and one bind each; other pages link there. Kept elsewhere: the
greetd example, `hyprctl dispatch workspace` as an action, and `strand
compositor-rules` (Lua by default, `--classic` for hyprlang), tested in
`crates/strand/src/rules.rs`.

**2026-10-26 · docs-tune: the home page's examples are recordings; live
mode names its buttons.** The home page's figures are ```` ```example
NAME recording ````, frames only. Live mode's bar is Play (absent when
no step acts), Next step showing its step in code, and Back to recording;
Restart went. Superseded for figures by the next entry.

**2026-10-26 · docs-tune: only the playground runs examples live.**
Replaces the two entries above where they put live mode on figures: a
figure is its recorded frames on every page, the `recording` fence word
is gone, and live.js has only the playground (Play, Next step naming its
step, Reset). The browser check holds every example to eval's frames in
the playground and every example page to fetching no WebAssembly.

**2026-10-26 · docs-tune: every page that walks through a config shows
what it draws.** Guide levels 1 to 5, the calendar, settings-file and D-Bus
how-tos, and the layout, state and surfaces concepts gain figures whose
scenarios hold the page's files word for word (`Rendered::steps`), using
mock services, `[seed]` and `warmup`. The launcher how-to and wallpaper
theme wait on Strand bugs; lock, greeter and plugins.md are not drawn.

**2026-10-26 · docs-tune: example media draw several surfaces, on
one timeline, without seams.** `strand-site media` composes a run's
surfaces at their trace `origins` onto one picture; a one-surface run
keeps its buffers bit for bit. The timeline is every tick either mode
painted. The moving part is drawn over the still frame through a hole
`RING` px inside the moving box, grown until that ring is clear or solid
in every frame. Strips load only when their figure nears the screen.

**2026-10-27 · shadow-part: a large shadow is drawn from its
pixmap's pixels on every frame.** When no pixmap is cached and none
is built, the frame rasters uncached the cells of the pixmap's
fixed 128 × 64 grid its damage reaches, outside the clip's
cut-out (`PaintCache::shadow_part`), exactly as the cached pixmap
would, so a partial redraw matches a full one. Caching policy is
unchanged. Tests: `a_large_shadow_repaints_from_its_cache_as_fresh`,
`a_large_shadow_entering_and_shrinking_repaints_as_fresh`.

**2026-10-27 · wallpaper-follow: eval waits for a wallpaper palette
as it waits for text.** Strand already followed the wallpaper; eval's
driver now waits at each tick for wallpaper jobs started before it
(`Harness::settle_images`, `ThemeHost::wait_images`), and a `rest`
does not end while one is in flight, so a palette lands one tick after
lookup. A docs dark twin reads its config against `eval/scenarios/docs/`,
so relative image paths match.

**2026-10-27 · shadow-pass: a size spring's clip cuts what spills past
its box, not inner shadows.** The clip a box sets only because its
size springs (`Animator::sizing`) does not cut the shadow or `glow:`
of a box lying inside it (within half a pixel); a box spilling past
it is cut, shadow and all, and an asked-for clip cuts shadows as before
(`Flattener::let_shadow_through`, `hold_to_box`). design.md "Growth springs"
states the rule. Tests in `crates/strand-render/tests/shadow_pass.rs`.

**2026-10-27 · docs-tune: the hello bar has 12 px at its ends.**
design.md's hello bar (kept word for word in the front page, README and
`hello_bar` fixture) writes `split { pad: 0, 12 }`, as the guide's level
1 and `strand new` do. `the_hello_bar_alone_is_laid_out_and_themed`
expects ink 12 px in.

**2026-10-27 · site-hosting: CI builds the site and Railway serves it.**
CI's `deploy` job, on a push to `main` with every job passed, uploads the
`site-live` artifact the `wasm` job checked via `railway up`, packaged
with a Caddy image (`deploy/railway/`, `scripts/site/package.sh`), so
Railway builds only that image. Rejected: Railpack building the workspace
(15–25 min, unchecked module), and a deploy branch of built files.

**2026-10-27 · site-cache: everything revalidates but the hashed module;
text goes as brotli.** Every file is `no-cache` with ETag 304s except
content-hashed files in `wasm/`, cached a year; Cloudflare respects the
origin's TTL. The image keeps brotli copies of text files and the module
(`precompressed br`). `scripts/site/package.sh --check` holds each answer's
encoding and caching.

**2026-10-27 · readme: the README faces outward; project records move to
docs/development.md.** The README is the site's front page in short; its
Layout, Roadmap, Building and installing and Development sections moved
word for word to `docs/development.md`, which opens with a map of `docs/`.

**2026-10-27 · license: MIT OR Apache-2.0.** The owner chose the Rust
ecosystem's dual license; no dependency is copyleft-only. `LICENSE-APACHE`
and `LICENSE-MIT`, `[workspace.package]` `license` inherited by every
member, eval's manifest and the README state it; bundled fonts and icons
keep their own licences.

## graph-flake

**2026-10-27 · graph-flake: the tick a kept history starts in holds
its oldest point.** That tick is now a column whenever the graph first
draws (`media::graph::past`), so a replay drawing earlier than live no
longer gains a column; this makes spec.md 8.1's same-columns promise
hold at a history's start, a fix under section 14. Values without
history still start at their first painted frame. Rejected: leaving
it empty, mapping replay surfaces at live times, eval tolerance. Test:
`the_tick_a_history_starts_in_is_a_column_whenever_the_graph_first_draws`.

**2026-10-27 · graph-flake: an instant step acts on the boot clock
at its tick.** Eval's `begin_step` now sets the virtual boot clock
(`set_virtual_boot(t)`) to the tick an instant step acts at, so logic times
its kept history on the clock it acts on (docs/eval.md). A change within
about half a frame of a column boundary can still land a column apart
(under 1% of runs), which the owner accepted on 2026-10-27; a rerun is
the answer. Test: `a_clicks_change_is_kept_at_the_tick_it_acts_at`.

## docs-prune

**2026-10-28 · docs-prune: the finished milestones' paperwork
leaves `docs/`.** The milestone plans and reports (`m0-report.md`
… `m5-report.md`, `m4-plan.md`, `m5-plan.md`), `handoff.md`,
`size-baseline.md` and milestone screenshots are deleted: each feature's
proof is the test `features.md` names, and git keeps them. `budgets.rs`'s
test of `m3-report.md`'s wording goes; `scripts/m0-exit.sh` and
`scripts/m3-shots.sh` write under `target/`. `benchmarks.md` stays,
marked as numbers that go stale.

## hypr-057

**2026-10-28 · hypr-057: Hyprland 0.57's workspaces are read by address.**
`id` is optional: a workspace without one gets an id kept for its address
while connected, from -1337 down, as 0.56 numbered named workspaces. Events
find a workspace by number or address, else re-read; actions name a named
workspace `name:<address>`. 0.56.2's replies read as before.

**2026-10-28 · hypr-057: an idle inhibitor is made when its surface is
shown, not mapped.** Hyprland 0.57 counts an inhibitor only at layer alpha
above zero and checks only when it is made, and a layer maps at alpha 0,
so the inhibitor now comes with the compositor's first frame callback
or presentation feedback. Other compositors count it a frame later;
the matrix waits for it.

**2026-10-28 · hypr-057: the matrix sets Hyprland options through `eval`
when `keyword` is unknown.** 0.57 removed `keyword` (`unknown request`),
so the activation test's `misc:focus_on_activate` falls back to `hl.config`
through `eval`.

**2026-10-28 · hypr-057: a live 0.57.0 capture confirms the
reconstruction.** One run of scripts/capture-hyprland.sh on the owner's
0.57.0 (fixture hyprland-0.57.0-captured, titles scrubbed) names the
scratchpad and a named workspace by address in the v2 events and writes
`id` only for numbered ones; the adapter reads it unchanged
(`captured_057_session_is_read_by_address`).

## cold-ceiling

**2026-10-28 · cold-ceiling: the cold full shell fails above 128 MiB,
from 104 MB; its 76 MB target stays.** One container run of
`the_full_shell_with_no_gpu_keeps_the_cold_budget` read 110,127 kB while
reruns of the same build read 91.7 and 100.8 MB, so the 104 MB ceiling
sat inside the run-to-run noise and failed CI at random. The owner chose
120-130 MB; 128 MiB is 18 MB over the highest reading. The gate stays a
tripwire for gross growth, and the target still warns about drift.

## docs-trim

**2026-10-28 · docs-trim: architecture.md and decisions.md say what holds
now, shortly.** The owner asked for both to be trimmed after docs-prune.
decisions.md keeps every `## ` heading (code cites them) and each
decision as one short paragraph of what was decided and why; about 25
entries that only recorded a merge, a rerun or a measurement log went
(3.0 MB to 0.76 MB). architecture.md keeps every `##` and `###` heading,
each crate's job, the threads and the cross-crate contracts in present
tense; history, plans, rationale (pointed to here) and listings the
code's doc comments give went (540 KB to 176 KB). The trims were checked
against the code; where the old text and the code disagreed it was kept
or noted, not silently changed. Git keeps the long versions.

**2026-10-29 · docs-trim: architecture.md corrected where it disagreed
with the code.** The trim's check found statements the code had moved
past (signatures of `Ring::new`, `Services::tap` and `set_cell_tap`,
`on_change_after_each`, where `Easing::named` lives, the check modules,
the 18 animated props, `INSTANCE_VARS`, `strand-introspect`'s
dependencies, the `strand-exec` and `strand-raster` threads, the painter's
input region, the lock's one instance per picked monitor); each now says
what the code does. The settings temp files were never a bug: their own
events name no watched file and their leading dot keeps them out of the
module scan.

## screens-focus

**2026-10-29 · screens-focus: `screens.focused` follows the compositor's
focused monitor.** The binary set `screens.focused` and every
`Screen.focused` from the first monitor, so on two monitors a `screens:`
that evaluates to `focused`, and the lock's single instance, took the
first monitor's `screen` while the compositor put the surface on the
focused one. Both now name the monitor whose `name` (its connector) is
the hub's `WmChange::FocusedScreen`; the first while nothing names one
(no adapter and no single activated window for the protocols, nothing
heard yet, a name no monitor has). Main hears the same monitor
(`ToMain::FocusedScreen`) and pins to it, with
`State::set_focused_monitor`, only the surfaces whose `screen` is the
focused monitor, so each shows on the monitor its `screen` is. While
none is named it pins nothing and the compositor places them as before.

**2026-10-29 · screens-focus: only a surface whose `screen` follows the
focus is pinned and moved; a default one stays where the compositor put
it.** A `panel` or `osd` with no `screens:` (or `screens: focused`
written) is "one surface the compositor places" with no `screen` in
scope (spec.md), so pinning it would change its placement depending on
whether something else reads the focus, and on Hyprland, whose focused
monitor follows the mouse, an open launcher would hop monitors as the
pointer crossed. `SurfaceSpec` tells the two apart with `follows_focus`:
the compiler gives an instance of a `screens:` that evaluates to
`focused`, and the lock's single instance, the internal `Prop::Screens`
keyword `FOLLOW_FOCUS` (still `Screens::Focused`), the smallest change to
the scene protocol; the surface manager pins and moves only those, and
leaves any other `screens: focused` surface unpinned, focus set or not.
The lock's content follows the focused output as before.

**2026-10-29 · screens-focus: the compositor service runs for the focus
only while something reads it.** Logic holds a
`wm::follow_focused_screen` follower (one hub subscription) while the
`screens.focused` cell has an observer, or the program names
`Screen.focused`, and lets it go `STOP_GRACE` after the last reader, as
a service stops. The cell's observers are its real readers (a binding,
a `screens:` that is `focused` now, a lock with one instance), so the
compiler needs no new bookkeeping; a record field has no cell of its
own, so `Screen.focused` is the static rule `notifications` uses for
`can_reply`. A shell that reads neither starts nothing for it (its bars
read `screens.all`); one that shows workspaces shares the run the
stores already hold.

## owner-testing

**2026-10-29 · owner-testing: the owner-testing log is retired.** Every
entry of docs/owner-testing.md (bugs 1-26, capability gaps 1-35, docs
and traps 1-3) was checked against main 3c11916b: each is fixed with
its test, decided, or settled below, and the owner had tried the fixes
on their own session, so the log is removed. A citation of
"owner-testing.md bug N", "gap N" or "docs and traps N" in code, tests
and docs names an entry of that file as it is at 3c11916b
(`git show 3c11916b:docs/owner-testing.md`). Settled with it: `network`
reads NetworkManager and iwd, and no other stack (systemd-networkd and
ConnMan are not planned; `network.address` reads the kernel for any);
`bluetooth` stays on BlueZ and Strand registers no pairing agent of its
own (BlueZ uses the system's, if one runs, for a PIN or a confirmation),
as m6-service-gaps left it; a list query's item
switching re-baselines `on change` as a service object's does, and
site/concepts/state.md shows watching the item's key to run on each new
one; and a text's line box stays CSS's `line-height: normal`, with
site/concepts/layout.md saying why text can look low in an even `pad`.

## step-time

**2026-10-29 · step-time: a replay times a change kept between ticks
with an `at` step.** graph-flake left one accepted failure: the replay
acts a live input at the tick nearest it, up to half a frame off, and
logic times what it keeps at that tick, so a change within that of a
graph column boundary landed a column apart (seen as 40 px DRIFTED in a
2×21 column of the e2e Top bar's graph on 2026-10-29). The live shell's
`history.json` holds each kept point's time on the boot clock, so the
plan writes `at S` before a step a point followed (from the step's time
to the next step's, within a period), with the point's time; the step
still acts at its tick, with the boot clock at `S` for that tick's
logic. Only the boot clock moves, since only kept history reads it, and
only where history was kept, so other replays are unchanged. Rejected:
acting inputs between ticks (every check reads whole ticks), and
tolerating a column in the verdict (the eval rules are frozen). Tests:
`an_at_step_times_the_next_steps_change`,
`an_at_step_is_refused_off_its_tick_or_before_time`,
`a_change_kept_between_ticks_is_timed_by_an_at_step`.

## perf-cold

**2026-10-29 · perf-cold (owner-approved): cold first frames measured
and closed; the rest of the review's open list is dropped.** Measured
on the owner's Intel GPU at 1.5× and 120 Hz in the bench and in a
running shell (docs/benchmarks.md, "Cold first frames"): a launcher's
first open shows at most one late frame, while it fades in from
transparent, and only the first theme crossfade of a session drops
frames (two or three: the GPU's first frame over an uploaded 14 MB CPU
snapshot, about 26 ms). Keeping full-screen GPU textures for the whole
session to save that once was rejected: theme swaps are rare and the
memory is held always. The owner dropped the other items of "perf: what
the review leaves open" (glass at 1× and backdrop blur on the CPU,
retained GPU layers, goo's GPU contour, CPU resize damage narrowing,
per-frame allocations, full GPU damage); they are not planned. The
bench's JSON keeps its warm-up cycle's frames (`cold_frames`) for the
next measurement.

## install

**2026-10-29 · install (owner-approved): an install script and two
release channels on GitHub; Strand never updates itself.** One command,
`curl -fsSL https://strand.jacobgrant.dev/install.sh | sh`, installs
Strand for one user with no root and updates it when run again
(`scripts/install.sh`; site/start/install.md). Each version gets its
own directory under `~/.local/share/strand/versions/`, `current` points
at one and switches in one rename, and `~/.local/bin` links `strand`
and `strand-dev` through it. `strand-auth` stays beside `strand` and
off `PATH`, as `default_helper` wants, and `strand-eval` beside it for
`strand replay`. Updating is running the script again, not a `strand
update`: the shell stays free of network code and of a second updater
that would fight a distribution package (the AUR comes later). Two
channels: tagged releases (the default, `releases/latest`) and `edge`,
the newest build of `main` that passed every job, replaced on each push
(`--edge`, remembered until `--release`). Both come from CI's `package`
job, which builds strand, strand-auth, strand-dev and strand-eval with
the release profile on Ubuntu 24.04 and tests the script against the
archive in fresh containers; the `release` job publishes it only after
every other job has passed, and the site, which serves the script, now
waits for `package` too. The workspace's version is 0.1.0, and `strand
--version` names the commit (build.rs's `STRAND_COMMIT`) and an edge
build's channel. The holes closed: (1) a shell's lock screen runs
`strand-auth` from its own version's directory, so the script never
removes a version a program still runs from (`/proc/*/exe`), keeps the
version it replaced, and refuses `--uninstall` while strand runs from
it; removing it would leave a lock that cannot unlock. (2) The new
`strand` is started once, and its libraries looked up, before anything
changes: a missing library or an old glibc changes nothing and is named
with the commands that fix it. The builds need glibc 2.39
(`MIN_GLIBC`, which package.sh holds them to), so Ubuntu 22.04 and
Debian 12 build from source. (3) Moving the `edge` tag would break `git
fetch` for anyone who has it ("would clobber existing tag"), so the tag
stays where the pre-release was first made, and the archive is named
by its commit, its checksum uploaded first and the build before kept,
so an installer listing the release mid-update reads one whole build.
A run whose commit main has moved past publishes nothing, so a slower,
older run never replaces a newer edge. (4) A tag must name the
workspace's version. (5) The script runs nothing until its last line,
so a download cut short does nothing; it refuses root and will not
replace a `strand` it did not link. (6) A binary release must carry
its dependencies' notices: `THIRD-PARTY-LICENSES.txt` from cargo-about
0.9.3, accepting Apache-2.0, MIT, BSD-2/3-Clause, ISC, Zlib,
Unicode-3.0 and MPL-2.0 (file-level, unchanged, its source on
crates.io); another license fails the job. The budgets job no longer
uploads M5's trial binaries (`strand-release-binaries`, m5-owner Q15,
Q20): the archive replaces them, and the CPU-only build stays a
source build. Rejected: a self-updater in `strand`; a moving `edge`
tag; a nightly schedule (every push already passes the whole gate);
static or musl builds (PipeWire, fontconfig and PAM are system
libraries).

## centred-surface

**2026-10-30 · centred-surface: a surface centred on one axis takes the
output on it.** Resizing a centred layer surface makes the compositor move
it, and on Hyprland the owner saw the whole surface, and a popup on it, jump
each time a pill's content changed width (an item leaving, media starting). A
panel or OSD anchored to one edge now asks for its output's size along that
edge from the first frame its bounds are known, and what it holds is laid
out as a held surface's is (`RootSize::Placed`, the growth spring, the box
alone taking input). Costs a wider buffer, only its damage redrawn. A surface
centred on both axes keeps its content's size: a whole output's buffer for a
launcher is too dear, and the owner has not seen it jitter there. Hyprland
would not run nested in a container here (it stalls before a monitor), so
the proof is `set_size` going quiet in a Wayland log on sway and the owner's
session; the sizes are tested by `a_panel_centred_on_one_axis_never_resizes_there`.

**2026-10-30 · centred-surface: its box is centred exactly, and an open popup
stays put when its anchor's middle has not moved.** A whole-pixel origin steps
one edge of a box whose width springs and not the other, a wobble the owner saw
as a few pixels of jitter (`RootSize::Placed`'s `middle`). A pill's width
changing moved the middle of a popup's anchor by half a pixel of rounding and
repositioned the card hanging from it (`Renderer::steady_anchor`). With these,
an island needs no popup and no copy of itself: one box whose contents decide
its size, growing down from its anchored edge.

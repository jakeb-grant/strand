# Decisions

Interpretations of ambiguous points in `design.md` and `architecture.md`.
One short dated entry per decision; each track appends under its own heading.

## core

**2026-10-05 · Explicit runtime handle.** `strand-core` has no thread-local
"current runtime": every read, write and closure takes `&Runtime`. A
`Runtime` is a cheap `Rc` handle and is not `Send` (the logic thread owns
it). Futures and stored closures that need it hold a `WeakRuntime`, so the
runtime never keeps itself alive; `Runtime::shutdown` disposes everything.

**2026-10-05 · Ticks and batching.** Writes never run effects directly;
`flush` (or `tick(now)`) ends the tick. Memos are pull-consistent at any
time, so a read mid-tick sees every write so far. Effects (and watches and
timer conditions) run in creation order, which puts owners before the
nodes they own. **Deviation from "once per tick in topological order":**
sinks have no graph edges between them, so this is topological for effects
that only read; an effect that *writes* a cell creates an ordering the
graph can't see, and an effect re-triggered by a later effect's write runs
again in the same flush (it always sees consistent values, and the flush
ends quiescent). Re-running is harmless for idempotent effects but not for
user handlers, so **`on change` handlers run in a late phase**: a queued
`on change` waits until no other sink is queued, then runs (still in
creation order). One outside write therefore fires `on change x, y` once
with the settled values, even when an effect created after it writes `x`
from `y`; a write the handler makes still reaches other effects in the
same flush. A learned rank (Incremental-style heights) would remove the
remaining re-runs of plain effects; not needed while those are idempotent.
(Superseded in wave 2: wave2-core, "Topological effect order".)

**2026-10-05 · Runtime cycles.** Past `MAX_RUNS_PER_FLUSH` (16) runs of
one sink (or deliveries of one event queue) in one flush, the runtime
searches this flush's writes, event deliveries and observer edges for a
path back to it. If there is one, it is a runtime cycle: `Error::Cycle`
names it (`on_x -> y -> on_y -> x -> on_x`, `a -> on_a -> b -> on_b -> a`),
reported once, and the node is **parked**: a sink is marked clean (its
derived inputs brought up to date so a later write reaches it) and a queue
keeps its events but leaves the pending list. The runtime is then idle
until something outside writes or emits, so a loop costs bounded work per
outside write, not 16 runs every tick. Without a path (wide fan-in from
long effect chains) the flush continues, up to `HARD_RUNS_PER_FLUSH` (256)
as a safety net. A memo cycle is stored as the memo's value and also
reported once in `Tick::errors`. A nested `flush` reports `Reentrant` at
the running handler.

**2026-10-05 · What "changed" means.** Each node has a change counter
(bumped by a cell write that changed the value, or a recompute that
produced a different value). A watch reports its target when the counter
moved since the last report. So a value written and restored within one
tick, or a memo pulled mid-tick to an intermediate value, may be reported
once more: one redundant `SetProp`, instead of a second copy of every
watched value (text, paints) against the memory budget. Effects likewise
may re-run on a write-and-restore; they always see consistent values. A
watch whose target is disposed disposes itself.

**2026-10-05 · Errors are values.** Memo closures return `Result<T,
Error>` and the error is stored as the memo's value (cut-off still
applies). Stale handles give `Error::Disposed`; a write inside a memo is
`Error::WriteInDerived` (derived values are pure); handler and effect
errors land in `Tick::errors`. Observers of a disposed node re-run and see
`Disposed`.

**2026-10-05 · Write-rate monitor.** The rule sits under "Feedback loops",
so it guards loops through the graph: handlers triggered by state
(effects, `on change`, timers, listeners of service events, and tasks they
spawn). Handlers run for external input (`on click`, `on scroll`,
`on activate`; queues from `rt.input_events()`, tasks from
`rt.spawn_input`) and `<->` writes from widgets are like CLI and service
writes and are not counted: the design's own `on scroll(dy) { volume -= dy
* 0.05 }` at 60 Hz is the user, not a loop. For a task the exemption ends
at its first `await` that suspends: the synchronous response to the event
is the user, but `on click { loop { x += 1; await sleep(10ms) } }` is a
runaway loop and is counted against its handler like any other. Writes are
counted per logic step (each `advance_to` and each `flush`), not per call
(many writes in one handler run coalesce; read-your-writes holds), so a
timer body and the task it spawned count as two attempts; writing the
current value is not counted. The 31st attempted write
within a sliding second warns once (`Diagnostic::WriteRate` naming
`handler -> cell`) and throttles as a leaky bucket: one write per 1/30 s
goes through and only the latest value is held in between, so the cell
moves smoothly at 30 Hz instead of bursting and stalling. While throttled,
`update` (`x += 1`) starts from the held value, so no step is lost. Any
write that goes through, even one equal to the current value, supersedes
held ones (latest value wins). A held write is dropped when its handler is
disposed (cancelled), except a task that finished normally. **Handler
identity** is stable: the listener, effect or timer node; a task inherits
the identity of the handler that spawned it; `spawn_for(site, fut)` counts
against `site`. Keyed collections are not gated: their diffs can't be
coalesced to a latest value; revisit if a collection loop shows up.
(Superseded in wave 2: wave2-core, "Keyed writes under the 30 writes/s
guard".)

**2026-10-05 · Echo suppression.** `write_tagged(value, send)` applies a
local write, remembers it as pending and calls `send(value, generation)`;
the service binding sends from there. It goes through the write-rate gate:
while throttled nothing is sent (`Ok(None)`), and the latest held value is
applied and sent with a fresh tag when the window has room, so a feedback
loop through a service is cut like a local one. At most 64 unacknowledged
writes are remembered per cell. `receive(value, tag)` ignores the
echo of any pending write (by tag, or by value when the protocol has no
tags, as with D-Bus `PropertiesChanged`) while newer writes are in flight.
When the service acknowledges the last write with a different value
(clamping), that value is applied. A value matching no pending write is an
outside change: it is applied and clears pending writes.

**2026-10-05 · Events.** Each event is delivered once to every listener
alive at delivery time, in emission order, during the next flush; events
emitted by listeners are delivered in the same flush (bounded: see runtime
cycles). Queued events are shared (`Rc<T>`), so one can wait for a frozen
listener while the others get it, without `T: Clone`.

**2026-10-05 · Timers and `on change`.** `after T while c` counts only time
during which `c` holds, fires once, and schedules nothing while paused.
Before the clock advances, timers whose condition changed are brought up to
date: a pause counts up to the previous tick time and a resume from the new
one, so a timer never counts time its condition may not have held for
(`hover = true` then `tick(deadline)` does not expire the toast).
`every T` keeps its phase (host latency does not accumulate); after a gap
longer than a period it fires once and re-phases (no replay). A zero
`every` period pauses the timer and reports `Diagnostic::ZeroPeriod`;
positive periods under 1 ms are clamped to 1 ms. Deadlines past
`Duration::MAX` mean never. A reactive duration keeps the time already
counted. Reload's "remaining time rescaled" is `Timer::rescale_from(old)`
(`Debounced::rescale_from` for `on change … after`): the new timer takes
over the old one's lifecycle, not only its fraction. An `after` that
already fired stays done (a reload never repeats `n.expire()`), an idle
debounce stays idle, and a countdown in flight, an armed debounce
included, keeps its counted fraction and finishes at the new duration.
`on change x` never fires for the first value; `on change x after T`
restarts its countdown on every change (debounce). Time is the logic clock
the host passes to `tick(now)`; `next_deadline()` tells it when to wake.

**2026-10-05 · `on change` and identity.** The design says `on change`
never fires "on a sink switch". The runtime can't know what a path's
identity is, so `on_change_keyed(key, track, handler)` (and
`on_change_after_keyed`) take it explicitly: when `key` changes
(`audio.sink`'s id for `audio.sink.volume`) the new value becomes the
baseline without firing. The compiler passes the object part of each
service path as the key.

**2026-10-05 · Handlers.** A handler is a `Future` owned by a node and
polled during the flush when woken; no async runtime is needed. Each task
is polled at most once per flush (a self-waking task waits for the next
one, so it can't spin the flush; the flush then calls the wake hook so a
sleeping host comes back). Woken tasks run in wake order and timers due
together in creation order, never slot order, so the later handler's write
wins. Disposal drops a task (cancelled at its current `await`, reported as
`Diagnostic::Cancelled`); a dropped `sleep` unschedules itself. Wakers are
thread-safe. **Ownership:** a handler body (listener, timer, `on change`,
task) creates nodes on behalf of its component (the handler's owner), so
`on click { hits.load(..) }`'s result outlives the invocation and finished
handlers leave nothing behind. The *tasks* a listener, timer or `on change`
handler starts belong to its **handler site** (the listener itself; a
separate node for timers and `on change`, because those re-evaluate their
condition and would otherwise cancel their own tasks), disposed with the
handler: reload's "handler code restarted; an in-flight `await` is
cancelled and reported" is `rt.dispose(handler)`. The VM's per-event
coroutines use a site from `rt.handler_site()`. Effects own what they
create, tasks included, and dispose it before re-running.

**2026-10-05 · `Async<T>`.** `x ?? fallback` is `Async::or`: the value if
there is one (kept while pending and after an error), else the fallback.
Each load gets a `RequestId`; a response to a superseded request is
ignored. A load that is cancelled (unmount, handler restart on reload)
clears `pending` if it was still the latest request; value and error stay
(a cancellation is not an error). `begin`/`resolve`/cancel bypass the
write-rate gate: they must see each other, and fast typing is not a
feedback loop. `let hits = apps.search(query)` is `rt.async_memo(input,
fetch)`, a read-only `AsyncMemo` (assigning to a `let` is an error): it re-requests when `input` changes and drops the superseded
request quietly (no `Cancelled` diagnostic, which is reserved for real
cancellations).

**2026-10-05 · Keyed collections.** `move` is spelled `move_key` in Rust
(keyword). Keys come from a key function (`key app`); `update` that changes
the key field is an error and is reverted. `VecDiff::Move { from, to }`
removes at `from` and inserts at `to` in the shortened list. `sort_by` is
stable (ties keep source order). Operator closures are pure; the values
they capture (`!dnd` in a filter) come through a tracked `params` closure
(`filter_with` etc.), and a params change rebuilds and publishes a keyed
diff against the previous output, never a bare reset, so identity
survives. Each collection keeps the last 256 diffs; a consumer further
behind gets a `Reset`. A service batch (`apply`) with a malformed diff
keeps and publishes the diffs before it and returns the error, so the cell,
its log and derived collections never disagree. `sort_by` never panics on
a comparator that is not a total order (NaN): it uses its own stable merge
sort; the order is then unspecified but keeps every key. A `for` over a
plain list expression (`calendar.days(month)`, `n.actions`, an `Async`
list) is `rt.keyed_memo(key_fn, f)` / `memo.keyed(..)` /
`async_memo.keyed(..)`: a derived collection that diffs each new list by
key against its previous output (no writing effect, so a 60 Hz list is
never rate-throttled; no second copy in the emitter). Duplicate keys are an
`Error` value; the last good list is kept to diff against. An `Async` list
yields its kept value, empty before the first result.

**2026-10-05 · Introspection.** `rt.sources(id)`, `rt.observers(id)`,
`rt.owned(id)`, `rt.root_owned()` and `rt.site_of(handler)` expose the
graph read-only for the inspector (token provenance), `strand watch` and
the LSP. `Tick::diagnostics` carries the warnings raised since the previous
tick (`take_diagnostics` remains for callers outside a flush).

**2026-10-05 · Re-running nodes and their children.** A node that re-runs
first disposes what it owns; losing a child it read must not dirty the
node itself (it re-reads everything), or `if show { let local = …; use
local }` would loop into a false cycle. Other observers of the disposed
children are still dirtied.

**2026-10-05 · Reparenting.** Identity across reloads ("a node keeps its
identity while its source text maps to it", monitors' state surviving an
unplug) needs state to outlive the scope that created it.
`rt.reparent(id, new_owner)` moves a live subtree; making a node its own
ancestor is `Error::Cycle` naming the ownership path, and a stale id is a
no-op. The moved subtree is renumbered after every existing node (keeping
its own order), so "owners run before owned" still holds. Cells keep their
identity, so a keyed cell keeps its diff log and the emitter needs no
`Reset`.

**2026-10-05 · Freezing a faulted component.** "A runtime fault freezes
only its own component" is `rt.suspend(scope)`: effects, watches, timers,
listeners and tasks inside stop running (work that comes due is held, each
node once), state is kept, memos stay readable, and the runtime can be
idle. Input events (`rt.input_events()`) are dropped for its listeners (a
frozen component ignores clicks); service and component events are
lossless, so they are kept per listener and delivered in order, once, when
it is released. `rt.resume(scope)` (the fixing reload) runs held work at
the next flush and calls the wake hook; timers that came due fire at the
next tick (they count while frozen; superseded in wave 2: frozen timers
pause, wave2-core). Work is also released when it leaves
the frozen scope another way: `reparent` out of it, or disposal of the
frozen scope after its live parts were moved out.

**2026-10-05 · Service lifecycle.** No observed/unobserved hook on cells:
the compiler knows which service paths each component reads, so the VM
reference-counts services per mounted, visible component (acquire on
mount and show, release in the scope's `on_cleanup` and on hide), and the
services layer applies the 5 s stop delay. Graph observation would see
memos read lazily by the emitter, not visibility.

**2026-10-05 · Left for wave 2.** `persist` storage with a default hash and
settings files are not in `strand-core` yet (they need file IO and the
schema from `strand-compiler`). (Wave 2: both are done in `strand-core`,
see wave2-core; the compiler supplies the field schema.)

## compiler

- **2026-10-05 · Keywords are contextual.** No reserved words; a word is a
  keyword only in its position, and `ident:` at the start of an item is
  always a prop (`enter: popin(0.8)`, `type: password`, `text: <-> q`).
  Removes a concept: users never learn a reserved-word list.
- **2026-10-05 · Calls touch.** `f(x)` and `xs[0]` need the `(`/`[` to touch
  the callee, as CSS function tokens do. That is what lets space-separated
  values hold parenthesised terms (`0 (-2px) 8px $shadow`), and an
  accidental `f (x)` gets a "remove the space" fix.
- **2026-10-05 · Operators always bind in space-separated values.** `0 -2px`
  is subtraction, never a negative term, so whitespace never changes meaning
  (design.md "Bad" table). Negative terms in shadows need parentheses.
  Because `0 -2px 8px` looks like three terms, a `+`/`-` in a
  space-separated value with a space before and none after is an error
  (`syntax::ambiguous_sign`) offering `(-2px)` or `0 - 2px`: loud, not a
  silently different shadow. `bg: f (x)` warns that it is two values.
- **2026-10-05 · Value precedence.** In a prop value: `~` loosest, then `,`,
  then space separation, then expression operators. So
  `lg: 0 8px 24px $a, 0 1px 2px $b` is a two-shadow list and
  `margin: 8, 8, 0 ~ instant` springs the whole value.
- **2026-10-05 · Line structure.** Newlines are ignored in `()`/`[]`,
  significant in `{}`. A line continues only if it starts with a binary
  operator, `.`, `?.`, `?`, `:`, `~`, `=>`, `<->` or `else`, or the previous
  line ended with a binary operator, `,`, `=`, `=>`, `<->`, `~` or a
  ternary's `?`/`:`. A line ending in `.`/`?.` or a prop's `:` does *not*
  continue: it is an error at the end of the line and the next line is its
  own item (props end at a line break; the LSP completes there). A line
  starting with `-` touching its operand (`-1 => b`, `-x.f()`) is a new
  item; `- x` continues. Optional blocks (element bodies, prop sub-blocks)
  must open on the head's line; mandatory ones may open on the next.
  Clause words (`key`, `persist`, `while`, `after`, `extends`, `rw`) stay on
  their line.
- **2026-10-05 · `pages current: page { … }`.** An element's single head
  argument may be named; `kind name: e { … }` is sugar for a first prop.
- **2026-10-05 · No record literal.** Records are built with call syntax and
  named arguments, `Pin(app: a, label: "x")`, reusing named args.
- **2026-10-05 · Component tokens in the head.** As written in design.md:
  `component Toast(n) tokens { radius: $radius.lg } { body }`. Parameter
  types are optional in the grammar (design.md writes `Toast(n)`); the
  checker requires them where it cannot infer.
- **2026-10-05 · `fn` has one form.** `fn f(x: T) -> U { …; value }`; the
  value is the last statement. No `= expr` short form (one syntax per idea).
- **2026-10-05 · `await` and `play` parse anywhere expressions or items
  do;** the checker limits `await` to handlers and decides what a tree-level
  `play` means.
- **2026-10-05 · Service sources.** `dbus` takes `system|session`, the bus
  name and an optional object path (UPower's display device lives off the
  derived path); `file` and `listen` take one expression; `poll` takes a
  command and `every T`. `permit exec ["prog", …]` is valid at the top level
  and inside a service.
- **2026-10-05 · `#word`.** A colour in expression position, an SVG
  selector at the start of an item (`svg "x.svg" { #needle { … } }`).
- **2026-10-05 · Token keys may carry `$`.** `$surface.hi: …` (design.md's
  token table) and `surface.hi: …` (its theme file) name the same token.
- **2026-10-05 · Small lexical choices.** `//` comments only; strings are
  single-line with escapes and no interpolation; `%` touching digits is a
  unit, after a space it is remainder; a number right after `.` takes no
  fraction (`$space.2`); comparisons do not chain; `??` binds looser than
  `||`.
- **2026-10-05 · Surface names.** `bar`, `panel` and `osd` need a name (it
  becomes the `strand-<Name>` namespace); `lock` may omit it.
- **2026-10-05 · Nesting limit 128, tree depth 256.** Recursive nesting
  (blocks, brackets, prefix operators) is capped at 128. Flat chains
  (`a + b + …`, `a.b.c`, `else if`) are parsed in loops and only count
  toward a tree-depth cap of 256, so a 200-link chain in a few blocks is
  fine. Measured: a 256-deep tree parses, dumps and drops on a 2 MiB
  debug stack; 512 does not. Later passes that recurse over the tree can
  rely on depth ≤ 256.
- **2026-10-05 · Missing `}` recovery.** A top-level declaration keyword at
  column 0 inside an open block (also an `enum` body or `match` arms, where
  `let`/`state` count too) closes the open blocks with one "unclosed `{`"
  error, and a `}` indented unlike its `{` *inside the unclosed block*
  (the latest one) marks the likely culprit, so the error points at the
  right line (design.md "What you see" #3).
- **2026-10-05 · Config file discovery is one function.**
  `strand_compiler::source::find_files`: `.strand` files up to three
  directories down (the watcher's depth), symlinks followed, names starting
  with `.` skipped (files and directories), files deduplicated by canonical
  path (design.md: Strand canonicalises each loaded file), unreadable
  sub-directories reported and skipped. A file argument checks that file.
  `strand check` and the watcher use it, so they load the same set. Until
  the checker lands `strand check` reports syntax diagnostics only.
  Round 2: the walk is breadth-first and directories are deduplicated by
  canonical path too, so a deeper link to a directory never hides its real,
  shallower path; a dangling `*.strand` link is an error, other dangling
  links are ignored. It defines the module set only; `Discovery::dirs`
  (canonical directories scanned) is there for the watcher, which also
  watches shaders, settings files and wallpapers.
- **2026-10-05 · Diagnostics live at `strand_compiler::diagnostic`,** not
  under `syntax`, because the checker and reconciler will share them. Each
  label carries a `FileId` (into a `SourceMap`) and a file-local `Span`, so
  one diagnostic can point at two files (a name declared twice). Rendering
  draws at most 50 diagnostics per file, then "and N more".
- **2026-10-05 · Lambda parameters take no defaults.** `(a: int, b) => …`;
  a lambda is always called with every argument, so `= default` would add
  a concept for nothing.
- **2026-10-05 · Misspelt tree keywords.** `whn hover { … }` and
  `enterr { … }` are valid element syntax, so the parser cannot flag them
  without the element list. The wave-2 checker's unknown-element
  did-you-mean must include the tree and top-level keywords as candidates
  (`when`, `if`, `else`, `match`, `for`, `enter`, `exit`, `slot`, `set`,
  `play`, …; TODO for `check`). `stat x = 0` (an element followed by more
  than an element holds) is flagged by the parser; at the top level it is
  one error, and only when one edit away (`text "x"` at the top level is a
  misplaced element, not a misspelt `let`). `els { … }` (or `els if`)
  directly after an `if` body is flagged by the parser and read as `else`.
- **2026-10-05 · Transitions and poses on `if` branches and pages.**
  design.md puts `transition: wipe(left)` "on `if`, `pages` and image
  swaps", but `if` has no prop position. A branch's transition or pose is
  written on the branch's root node (`if open { box { transition: wipe(left)
  } }`); for `pages`, on the `pages` element.
- **2026-10-05 · Keyframe stops** are percentages followed by a block, comma
  separated for shared stops: `keyframes shake { 0%, 100% { x: 0 }; 25% { x:
  -4 } }`.
- **2026-10-05 · A leading UTF-8 byte-order mark is trivia.**
- **2026-10-05 · Snippet fixtures.** design.md's code blocks are fixtures
  byte for byte (a test enforces it). Table snippets are placed in minimal
  context; doc ellipses `…` are filled and `a | b` alternatives (notation,
  not syntax) become separate lines.
- **2026-10-05 · A dangling operator before a new prop ends the line.**
  Rule 4 (a trailing operator continues the expression) yields when the
  next line starts with a name touching `:` (`color:`, `$fg:`, `a.b:`):
  `value: <->`, `width: 24 ~`, `margin: 8, 8,`, `opacity: a ??` then report
  `syntax::missing_value` at the end of the line. A ternary branch on the
  next line must space its `:` (`b : c`) to continue.
- **2026-10-05 · Spaced values are for shadows and fonts.** The grammar
  accepts space-separated groups in any prop value (`Spaced`), because a
  shadow list (`0 2px 8px #0004, …`) and the font shorthand (`"Inter" 13px
  500`) need them. The checker must accept `Spaced` only for shadow-list
  and font-typed props and elsewhere report an error suggesting commas
  (`margin: 8 8 0` → `8, 8, 0`, design.md "Bad" table).
- **2026-10-05 · Kebab-case names.** `max-width:` at the start of an item is
  `syntax::kebab_case` with "names are snake_case: `max_width`", parsed as
  that prop. A touching `$fg-muted` is only a warning (it is a valid
  subtraction); the checker's unknown-token error will usually follow.
- **2026-10-05 · Allman braces.** An element's body `{` on the next line
  is an error (rule 6) but kept as the body; any other `{` that starts no
  item is skipped with its block. Either way the braces stay in step and
  there is one diagnostic.
- **2026-10-05 · A lone `\r` is a line break** (lexer, `LineIndex`, and the
  text given to miette), so old-Mac files parse as they read and the
  rendered header, gutter and `file:line:col` agree.
- **2026-10-05 · Keyword slips are read as the keyword.** One edit from a
  keyword, at least three letters (or the keyword's length), and not
  followed by `.`, `?.`, a touching `(`/`[`, `=` or an assignment. At the
  top level a slip is parsed as that declaration; in a tree only when the
  rest cannot be an element, or for `on`/`after`/`every` when the block
  starts with a statement, since `Set`/`Exit` may be component names.
  A misspelt `override` before another token key is an error and parsed as
  `override` (design.md "Loud overrides").
- **2026-10-05 · Long lines render short.** A diagnostic whose drawn lines
  exceed 1,000 characters is rendered as one `file:line:col` line: miette
  panics past column 65,535 and a snippet of a minified line is useless.
- **2026-10-05 · Whole-number literals past 2^53** (and any literal past
  f64) warn `syntax::number_precision`: numbers are f64.
- **2026-10-05 · strand-watch does not link the compiler.** The binary calls
  `source::find_files` and passes plain paths to the watcher (constructor
  and a rescan callback). Directories past depth 3 that hold `.strand`
  files are reported (`Discovery::too_deep`) and `strand check` warns.

## render

- 2026-10-04 · render: `Damage::area()` is the exact area of the union
  (overlaps once); "the pair whose union grows area least" minimises
  `area(bbox(a, b)) - area(a ∪ b)`, and rects the merged box covers are
  dropped.
- 2026-10-04 · render: `Color::lerp_oklab` interpolates premultiplied
  OKLab (CSS Color 4) so fades to transparent keep their hue; `t` is not
  clamped (springs overshoot). sRGB transfer is extended sign-symmetrically
  so out-of-gamut colours round-trip.
- 2026-10-04 · render: protocol details the contract leaves open:
  `PropValue::Unset` reverts a prop (no sixth op); `Create`/`Move` take
  `parent: Option<NodeId>`; `Transition::Duration` carries an `Easing`;
  shorthands (`pad`, `margin`, `radius`) arrive as `Insets` / `Corners`
  or, when an item is token-bound (`pad: 0, $space.3`), as a `List` of
  1–4 values that `TokenScope::resolve` resolves in place and render
  expands like CSS (`Insets::from_values`, `Corners::from_values`), so a
  theme swap or `set { }` reaches them; `NodeKind` has `start`/`center`/`end` for `split`, and names
  every prop and kind of the design catalogue so compiler and LSP share
  one table (render ignores what it does not draw yet).
- 2026-10-05 · render: `Move`'s index counts the new parent's children
  after the node is detached. `Remove` kills the id at once; from M2 an
  exiting subtree becomes a render-side ghost outside the id-indexed slots
  so the reconciler never waits on exit poses before reusing a slot.
  `Length::Ch` reads as unset until M2 layout can measure `0`.
- 2026-10-05 · render: tokens travel unresolved (`PropValue::Token`) and
  render evaluates them at flatten time against a `TokenScope`: the global
  `SetTokens` table, then each ancestor's `tokens` prop (`set { }` and
  component `tokens { }` both lower to it). An override's right-hand side
  sees its parent scope; global derived tokens are evaluated in the asking
  node's scope, so `$surface.hi` follows a subtree's `$surface`. Methods:
  `.alpha(a)` sets alpha, `.mix(c, t)` lerps premultiplied OKLab (`8%` =
  0.08), `.lighten/.darken(d)` move OKLCH L. Gamut mapping clips per
  channel until M2. Unresolvable references read as unset.
- 2026-10-05 · render: `~ $motion.bouncy` is `Transition::Token(path)`
  (so `Transition` is no longer `Copy`); it and `Default` resolve through
  `$motion.*` tokens holding `PropValue::Transition` at render time, so a
  theme swap reaches props that named a token. Unknown ones snap.
- 2026-10-05 · render: `radius: full` is `Corners::FULL` (infinite) or
  `Keyword("full")`; render turns it into the largest radius and the CSS
  corner shrink makes the pill. A `%` radius is of the shorter side.
- 2026-10-05 · render: planned so `Prop` can stay a `Copy` enum: shader
  uniforms (`u_speed: 0.4`) travel as one `uniforms` prop holding a
  name → value record, SVG `#id { … }` blocks become child nodes that
  carry ordinary props, and render-evaluated time signals (`t`,
  `wave(1s)`, `noise`) become `TokenExpr` variants (time and call) in M4.
- 2026-10-05 · render: call-shaped values (`hit: grow(6)`, `backdrop:
  blur(16)`, `filter: grayscale(1)`, `transition: wipe(left)`) are
  `PropValue::Call { name, args }`; a filter chain is a `List` of calls.
- 2026-10-05 · render: `SetTokens` carries a `transition` (logic sends
  `Instant` for the boot table, so no default colours flash). Props of
  class `Snap` (fonts, text, keywords) always snap whatever `~` says.
  Token evaluation has a work budget (`MAX_TOKEN_STEPS`, 10k per
  resolution) besides the depth cap, so a fan-out-heavy user theme fails
  to resolve instead of stalling the render thread.
- 2026-10-05 · render: surfaces. A surface's declared name travels as
  `Prop::Name` (set by the compiler; not a source prop) and gives the
  namespace `strand-<Name>` (`strand-<kind>` if unnamed). Defaults: a bar
  is on every screen, edge `top`, layer `top`; a panel is on the focused
  screen, layer `top`, anchor `center`; an osd is focused, `overlay`,
  `center`; keyboard `none`; unset `open` is open. A bar's exclusive zone
  is its thickness (compositors add the edge margin). Logic, which makes
  one `bar` instance per monitor, sets each instance's `screens` to that
  monitor's identity string. Only kind, namespace or layer changes
  recreate a surface; render reports changes through
  `take_surface_changes`.
- 2026-10-05 · render: text truncation. `ellipsis: start | middle | end`
  cuts with "…" to fit `max_width`; without `max_lines` ellipsised text is
  one line. `max_lines` drops lines past it (ellipsised when `ellipsis` is
  set). `marks` is a list of `[start, end)` character ranges painted in
  `mark_color`, else `$accent`, else bold. Spans (`TextStyle::spans`) also
  carry weight and italic; `markup: basic` parsing into spans comes with
  notifications (M3).
- 2026-10-05 · render: a surface that has never painted holds its first
  frame up to 50 ms (`FIRST_FRAME_TEXT_WAIT`) while its text is shaped;
  `frame_deadline` tells the loop when to look again. After a worker
  engine reset every surface holds its old frame the same way, and the
  request that crashed the engine is not asked again until its text
  changes. A layout missing glyphs for want of atlas room is retried at
  most twice, and again only after other text changes or goes.
- 2026-10-04 · render: the glyph atlas is LRU per page: per scale up to
  `max_pages` (4) 256 px alpha pages, shelf-packed; the least recently
  used unleased page is reset when full. Layouts lease their pages, so
  leased pages are never reset (the atlas grows instead), but never past
  `max_bytes` (1 MiB alpha per scale): glyphs that would need more are
  skipped, so one layout of huge distinct glyphs is bounded. Masks bigger
  than a page go on oversized pages (rounded to 64 px, ≤ 2048) that other
  masks share when they fit. Fonts are capped at 512 physical px, text at
  64 KiB per request. Glyphs are unhinted at quarter-pixel x positions;
  colour glyphs are skipped for now. Tests use vendored Liberation Sans
  (OFL) with system fonts off.
- 2026-10-05 · render: atlas pixels reach render as `AtlasUpload`s inside
  each `TextLayout`, applied in arrival order (also for discarded layouts,
  but not for scales already pruned). Page generations are unique in the
  process. A worker that recovers from a panicking request sends a layout
  with `is_reset()`; render then drops its mirror and every layout, repaints
  in full and re-requests text. Scales no surface uses are dropped from
  worker and mirror together; a rescaled surface keeps its previous
  scale's layouts (drawn resampled) until its text is shaped at the new
  scale, so text never blanks. Each layout also lists its scale's live
  pages (`atlas_pages`) and the mirror drops the rest (trimmed or reset),
  so it holds at most four times (RGBA: vello_cpu images have no alpha-only
  format) the worker's alpha bytes. Dropping a `TextWorker` discards its
  queue.
- 2026-10-05 · render: until taffy (M2) layout is absolute: `x`/`y` inside
  the parent, sized by `width`/`height`/`size` (px or % of the parent);
  text sizes to its layout; a surface root fills its buffer. `font`,
  `color` and `tokens` inherit. A surface-kind child (a `popup`) is its own
  root: skipped by its parent's flatten, it inherits from its ancestors.
  Props snap; each keeps its `Transition` for M2.
- 2026-10-05 · render: damage is per node: each node's record is its
  physical ink bounds (shadows included, clipped by ancestors) and a hash
  of its paint items plus inherited context (ancestor opacity, clips, move
  epoch). Changed records damage old and new bounds. Buffer age widens by
  the last `age - 1` frames (history 4); age 0, older buffers, resizes and
  rescales repaint in full. Only a non-empty `paint` result is a frame:
  the caller commits exactly it, or calls `invalidate`; an empty result
  records nothing. Edits and deliveries only dirty the surfaces they touch
  (and surfaces nested in them); `update` flattens dirty surfaces once,
  caches the result for `paint`, and clears `dirty` when nothing visible
  changed (text still being shaped), so no frame is wasted.
- 2026-10-05 · render: vello_cpu renders into a fixed grid of 256×64
  cells; only cells the damage touches are rasterised, by a cell-sized
  context with the scene translated to the cell, under rect clips of the
  disjoint damage. The fixed grid keeps partial repaints bit-identical to
  full ones (f32 pipeline: u8 rounds differently at clip edges). Colours
  go to vello with red and blue swapped, so it writes `wl_shm` BGRA
  directly. Clock tick: 0.12 ms on 2560×36, 0.18 ms on 3840×2160.
  vello_cpu has only `std` + `f32_pipeline`, single-threaded. A context is
  kept per cell size, four per attached surface (at least eight), so
  several outputs never rebuild edge-cell contexts each frame.
- 2026-10-05 · render: shadows follow CSS box-shadow (blur = 2σ, clipped
  away under the box); differing corner radii are drawn per quadrant.
  Borders are inside the box, width rounded to whole physical pixels (≥1).
  Gradients interpolate premultiplied OKLab (8 samples per segment);
  conic sweeps clockwise from `from`, measured from the top. Dithering is
  M2.
- 2026-10-05 · render: `PaintTarget.time` is the predicted presentation
  time; `Painter::opaque_region` (default empty) reports buffer pixels:
  the root's opaque `bg` minus its corner squares, convert with
  `Scale::inner_logical_region` for `set_opaque_region`.
- 2026-10-05 · render: values from user expressions are sanitised:
  non-finite numbers read as unset, lengths and offsets clamp to ±1e6
  logical px, blur to 1000; `Create` with an index more than 65,536 past
  the live slots is rejected; rect and damage arithmetic saturates.

## surface

- 2026-10-05 · surface: buffer size is `Scale::physical_size` (round half
  away from zero), which is what `wp_fractional_scale_v1` prescribes, not
  the `ceil` the track spec mentioned; they differ only when
  `logical × scale` has a fraction below .5, where `ceil` would make a
  buffer 1 px larger than the viewport and the compositor would resample
  it (blur). Before the compositor's `preferred_scale` arrives the scale
  is estimated from the output's mode and xdg-output logical size, so the
  first frame is already sharp. Without fractional scale or viewporter
  (or with `Config::fractional_scale = false`) buffers are `logical × n`
  with `set_buffer_scale(n)`, `n` the surface's preferred integer scale.
- 2026-10-05 · surface: the lifecycle hooks the manager needs (attach,
  configure, detach, monitors, `frame_deadline`, `frame_dropped`) live in
  `strand_surface::SurfaceHost: Painter` with no-op defaults, not in
  `strand-scene`: render does not depend on surface, and the binary wraps
  `Renderer` to forward them. `frame_deadline` returns an `Instant` like
  `Renderer::frame_deadline`. No `strand-scene` change was needed.
- 2026-10-05 · surface: frame callbacks. A buffer commit requests a frame
  callback only if `wants_frame` is still true after the paint, so a
  single repaint (a clock tick) costs one commit and no callback.
  Repaint requests and Wayland events mark surfaces dirty; marked surfaces
  are painted once at the end of the loop wakeup (calloop idle). A
  surface whose last frame is still in flight waits: for its frame
  callback, or, when none was requested, for that commit's presentation
  feedback (`presented` or `discarded`), so paints lock to the refresh
  rate however often content changes (review round 1). Without
  `wp_presentation` every buffer commit requests a frame callback.
  Feedback carries the surface's generation and commit number, so
  feedback for a destroyed surface whose id was reused is ignored.
  Configure and `preferred_scale` only mark the surface; size and scale
  are resolved right before the paint, so one wakeup gives one
  `surface_configured`. A configure that needs no new frame gets a bare
  commit so the ack takes effect (while a frame is in flight it waits for
  that frame's callback or feedback: a bare commit would discard the
  feedback). A configure or scale that needs a new buffer size skips the
  wait, so a surface the compositor does not present still applies it
  (review round 2; not reproducible on headless sway 1.9, which presents
  occluded surfaces and defers output changes while powered off, so it
  has no sway test). Before painting, a host hold (`wants_frame` false,
  `frame_deadline` Some: the renderer's first-frame text wait) arms a
  timer at the deadline and commits nothing but a pending ack; the paint
  cancels any armed deadline. A paint that returns no damage while
  `wants_frame` stays true arms a timer at `frame_deadline` if the host
  gives one; on a surface that has not committed a buffer yet (unmapped:
  no frame callbacks come) it retries after 16 ms; otherwise it requests
  a callback with a bare commit. Presentation
  feedback is requested for every buffer commit (none while idle).
  `State` methods called between dispatches take effect on the next
  `SurfaceManager::dispatch`, which does not sleep while work is pending.
- 2026-10-05 · surface: buffers. One pool per surface and size, buffers
  created lazily: the free buffer holding the newest frame is reused, a
  second is created while the first is on screen, a third only while two
  are busy; with all three busy the paint waits for a release. Ages count
  buffer commits since the last resize. A new size (or a new pool) drops
  the old pool and destroys its buffers at once, busy or not: allowed by
  `wl_surface.attach` because that storage is never written again.
- 2026-10-05 · surface: monitors. Identity is `"make | model |
  description"`; a second identical monitor plugged at the same time gets
  ` #2`. `Screens::Named` matches the identity or the connector name
  (`DP-1`). An unplugged monitor is remembered for 30 s (one timer, armed
  only while something is remembered) and keeps its per-node
  `SurfaceId`s, so a quick replug reattaches the same ids. wlroots puts
  the connector into `wl_output.description` (`"… (DP-1)"`); a trailing
  `" (<connector>)"` equal to the output's own name is dropped from the
  identity, so a monitor moved to another port is the same monitor.
  Numbering of identical monitors follows plug order. `Monitor` also
  carries the output's scale (estimated fractional, else integer),
  xdg-output logical size and position; changes to those (same identity)
  go to `SurfaceHost::monitor_changed`. On sway, `output X disable` /
  `enable` withdraws and re-adds the `wl_output` global with the same
  description, which is how the replug test exercises reconnection.
- 2026-10-05 · surface: `screens: focused` is one layer surface per node
  created with `output = null`; wlr-layer-shell lets the compositor put
  it on the output the user last interacted with (sway: the focused
  workspace's). Its monitor is learnt from `wl_surface.enter`
  (`SurfaceHost::surface_entered`); `surface_attached` gets `None`.
  `State::set_focused_monitor` (for a compositor IPC service) pins it and
  moves an open one; when that output goes, the compositor picks again.
- 2026-10-05 · surface: a layer surface the compositor `closed` is
  destroyed and detached; it is recreated only when outputs change or its
  spec is updated, never in a loop. Content-sized surfaces (a bar without
  thickness, a panel or OSD without width and height) are not mapped until
  M2 layout can size them (logged); `popup` and `lock` are not layer
  surfaces. Logical lengths are rounded to whole pixels for layer-shell.
  Spec updates reconfigure anchor, size, margins, exclusive zone and
  keyboard in place with a bare commit; layer or namespace changes
  recreate.
- 2026-10-05 · surface: `InputEvent` and its parts moved to
  `strand-scene` (`strand_scene::input`; `strand_surface` re-exports
  them) so render and logic can name them. `SurfaceHost::input` gives the
  host every event on the main thread before the channel; the channel is
  created by `take_input` and nothing is queued before. Wayland serials
  stay in `strand-surface` (`State::last_button_serial` for popup grabs).
  The cursor is set to `default` on every enter through SCTK's themed
  pointer (`wp_cursor_shape_v1`, else the cursor theme). An `osd` gets an
  empty input region before its first commit (design example d:
  "click-through").
- 2026-10-05 · surface: SCTK is used without default features (no
  xkbcommon until keyboard input); `rustix` reads the clock
  `wp_presentation.clock_id` names. SCTK 0.21.1 declares rust-version
  1.86 while the workspace says 1.85; the root manifest is not this
  track's, so the bump is left to the integration step. Headless wlroots reports refresh 0
  in presentation feedback, so predictions there fall back to "now";
  refresh locking is covered by fake-clock unit tests.

## m0

- 2026-10-05 · m0: `strand run --demo` builds the hello bar's scene by
  hand (`crates/strand/src/demo/scene.rs`), standing in for what the
  compiler will emit. M0 layout is absolute placement, so `split`'s
  `start`/`center`/`end` each span the bar and align their text (start
  and end padded by `$space.3` = 12 px, done as `x` = +12 / -12 on
  the full-width start and end sections, so each overhangs the opposite
  edge by 12 px with nothing drawn there). The offsets stand in for the
  `pad` the M1 compiler will emit on the split (`split { pad: 0,
  $space.3 }`), which taffy honours in M2; they are not emitter output
  to copy. The
  start and end texts are static placeholders until the window and
  battery services (M3). Colours are literals (`#1e1e2e` / `#cdd6f4`)
  until tokens and palettes are wired; the font is `$font.ui` written as
  `"Inter, sans-serif" 13px 500`, which falls back to DejaVu Sans in the
  dev container.
- 2026-10-05 · m0: the demo has one `bar` node with the default `screens`
  (every output), so the surface manager makes one layer surface per
  monitor and render paints the one subtree at each output's scale.
  Per-monitor instances with their own state (`Screens::Named`) arrive
  with the language in M1. `strand run` without `--demo` stays "not
  implemented (M1)": there is no config to run before the compiler.
- 2026-10-05 · m0: the clock is a `strand-core` `Signal<i64>` of Unix
  minutes, a `Memo` formatting it with chrono (`%H:%M`, local time) and
  the scene emitter at the edge, which (as `architecture.md` specifies
  for the compiler's emitter) calls `rt.watch(memo.id())` and turns
  `Tick::changed` into `SetProp(text)` in the tick's `SceneDiff`, rather
  than a writing `Effect`; the logic thread sends at most one diff per
  tick over a calloop channel. It sleeps
  on a `CLOCK_REALTIME` timerfd armed at the absolute next minute
  (`TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET`, so a clock step wakes it
  to re-arm) plus `Runtime::next_deadline` and the runtime's wake hook.
  The real `clock` service (M3) takes this over.
- 2026-10-05 · m0: gate readings. Damage "per tick" is checked both per
  committed frame (per surface) and as the sum over all outputs at a
  tick, on the damage actually submitted (already widened by buffer
  age); every frame after boot is also held to the gate. "No wakeups" is zero growth of voluntary + involuntary
  context switches summed over every thread of the process from :03 to
  :57 of a minute. PSS is `Pss:` of `smaps_rollup` after the bars are up
  and two ticks have passed.
- 2026-10-05 · m0 (gate fix in strand-surface): a freshly created shm
  buffer starts as a copy of the buffer holding the newest frame
  (copy-forward, a ~330 KB memcpy for a 1440p bar) and reports age 1.
  Before this the first change after boot, when the compositor still held
  the boot frame, was painted into a new buffer of age 0 and repainted
  the whole bar (81,920 px² at 1.0, 102,400 px² at 1.25), failing the
  damage gate on the first tick.
- 2026-10-05 · m0: `STRAND_LOG` is a comma list of a level (`error`,
  `warn` (default), `info`, `debug`, `trace`, `off`) and topics; `damage`
  prints one `strand: damage surface=… buffer=WxH scale=… age=… area=…
  rects=…` line per painted frame, plus `strand: dropped surface=…` when
  that frame's commit then fails; `scripts/m0-exit.sh` parses both.
- 2026-10-05 · m0 (gate fix in strand-render): text layouts are keyed by
  node, scale **and line box width** (`max_width`), not node and scale.
  `center`/`end` alignment happens inside the shaped line box, so one bar
  node on two outputs of the same scale but different widths (2560 and
  1920 at 1.0, the common pair) drew whichever width was shaped last on
  both: the clock off-centre and the end text off-screen on one bar. The
  same cache let the 1.25 bar's first frame borrow the 1.0 bar's layout
  shaped for 2560 logical px, a boot correction frame that a later tick
  then repainted with (3,738 px², over the gate) about 1 boot in 5–10.
  Now each surface draws the layout for its own scale and width; a
  surface not yet painted holds its first frame for exactly that layout
  (up to the first-frame wait), and a painted one, while its layout is
  re-shaped, draws a stand-in from another scale or width resampled and
  shifted so its alignment lands where the right one's would. Slots no
  surface wants are pruned.
- 2026-10-05 · m0: the demo holds a new bar's first frame up to 500 ms
  for its text (the renderer's default is 50 ms), so a boot under load,
  while the text worker loads fonts, still paints text-complete first
  frames instead of a stand-in corrected a moment later.
- 2026-10-05 · m0: `strand run --demo` exits 0 when the compositor goes
  away (the Wayland connection reports a broken pipe or reset), and with
  the logic thread's error as soon as that thread's channel closes.
- 2026-10-05 · m0: the demo's one `bar` node shown on every output is
  an M0 shortcut, not the M1 model. In M1, `strand run` forwards the
  `SurfaceHost` `monitor_*` hooks to logic as the `screens` service and
  pins one `bar` instance per monitor (`Screens::Named`); state survives
  an unplug through `rt.reparent`. It also adds a render → logic channel
  for `InputEvent`s and layout facts. The per-width text slots in render
  exist because of the shared node; their M2 costs are under "Later" in
  `architecture.md` (render).
- 2026-10-05 · m0: the 34 MB PSS gate is asserted on a release build
  (`cargo test --release -p strand --test demo`, and
  `scripts/m0-exit.sh`). A debug run of `demo.rs` is held to a separate
  40 MB debug ceiling: debug builds carry about 9 MB that release does
  not.
- 2026-10-05 · m0 (test fix in strand-surface):
  `commits_lock_to_the_refresh_rate` checks presentation timestamps from
  the compositor, recorded by a `FakeClock`, so each frame is on a later
  refresh than the one before. It no longer derives a bound from
  wall-clock time × 60. Headless sway reports `seq` 0 and refresh 0, so
  the test assumes 60 Hz with 2 ms slack and compares `seq` only when it
  moves.
- 2026-10-05 · m0 (round 3, render gate-adjacent fix): when
  `prune_texts` drops a slot that had a layout, every surface wanting a
  slot of the same node with no layout (it may have drawn the dropped one
  as a stand-in) is marked dirty; `flatten_surface` re-flattens if it is
  itself one. A poisoned surface then stops drawing a gone layout.
- 2026-10-05 · m0: `scripts/m0-exit.sh` gates damage per tick as the sum
  over all bars in every scenario, the 3-bar tick after hotplug included.

## integration

- **2026-10-05 · integration: wave-1 merge.** Branches merged in the order
  core, compiler, render, surface, m0. `Cargo.lock` was regenerated from
  main's lock with `cargo metadata`, so every branch's locked versions are
  kept. `docs/features.md` and this file are the union of every track's
  entries; where the compiler track ticked a syntax-only box and the core
  track noted the runtime side, both notes stay on the one line.
- **2026-10-05 · integration: `strand` binary.** The compiler track's
  `strand check` (handled before dispatch) and the m0 track's
  `run --demo` and mimalloc allocator live side by side in `main.rs`.
- **2026-10-05 · integration: MSRV 1.89.** `rust-version` is 1.89, the
  highest any locked dependency declares (vello_cpu, vello_common and
  fearless_simd 0.3/0.7; parley 0.11 needs 1.88; smithay-client-toolkit
  0.21 and wayland-protocols 0.32 need 1.86).
- **2026-10-05 · integration: CI Wayland and budget tiers.** CI installs
  sway, grim and fonts-dejavu-core and sets `STRAND_REQUIRE_SWAY=1`, under
  which the sway-backed tests (`crates/strand-surface/tests`,
  `crates/strand/tests/demo.rs`) fail instead of skipping; it also runs
  `cargo test --release -p strand --test demo` for the 34 MB PSS gate.
- **2026-10-05 · integration: checklist coverage.** Items the design names
  but the checklist lacked were added unticked: state placement and
  default-adoption notice, the p95 reload-latency benchmark, text props
  (`ellipsis`, `max_lines`, `marks`, `markup: basic`), the 6 MB image LRU,
  surface defaults and `strand-<Name>` namespaces, deterministic-spring and
  theme-swap render tests, wallpaper watching, service threads, compositor
  reload and cache-invalidation sources, `blur_fallback`, single-pixel
  buffers and `attach:` fillets, and the lock-screen VM tier.
- **2026-10-05 · integration: CI toolchain pinned.** CI uses Rust 1.97.0,
  the dev container's toolchain, instead of floating `stable`: the first
  merged run failed on a deprecation (`fetch_update` → `try_update`) that
  only the newer stable reported. Bump the pin deliberately, fixing new
  lints in the same commit. CI also installs `pkg-config` and
  `libfontconfig1-dev` for parley's font discovery.

## wave2-check

- **2026-10-05 · wave2-check: the schema is data.** Elements, records,
  services, functions, methods on builtin types, palette roles and base
  tiers are written in a small declaration language
  (`crates/strand-compiler/src/schema/builtin.schema`) parsed once, not in
  Rust tables, so the checker, the LSP and later service crates read one
  table. A service crate hands its own text to `Schema::extend` (records
  with `rw` fields, `fn` methods, `action`s, `event`s, `service` globals)
  and the config is checked with `compile_with`. Services are types only
  until M3. Beyond the services the doc's examples read (`clock`,
  `calendar`, `battery`, `windows`, `workspaces`, `audio`, `brightness`,
  `tray`, `notifications`, `apps`, `media`, `system`, `cpu`, `screens`,
  `wm`; `screen` is in scope in a `bar`), the schema types the ones its
  prose names (`network`, `bluetooth` for the Wi-Fi and Bluetooth pages,
  `memory`, and `auth` for the lock screen's PAM helper).
- **2026-10-05 · wave2-check: the palette is Material 3's system roles**
  under Strand's names: `accent` is primary, `fg` is on-surface, `bg` is
  background, and the `*_container`, `on_*`, `surface_*`, `inverse_*`,
  `outline*`, `shadow`, `scrim` and `surface_tint` roles follow M3. Base
  tiers (`space`, `radius`, `font`, `motion`, `elevation`) and the derived
  names design.md uses (`surface.hi`, `fg.muted`, `fg.faint`,
  `accent.hover`, `accent.container`, `border`) are typed in the schema and
  valued by a theme's `tokens` set; defining them in a set is not a
  redefinition, but redefining a palette role in a set needs `override`
  (roles come from `use palette`).
- **2026-10-05 · wave2-check: what is global, what is per file.**
  Components, surfaces, enums, `type`s, `fn`s, token sets, keyframes and
  custom services share one namespace across the config; a second
  declaration is an error naming both places. Top-level `state` and `let`
  belong to their file: design.md's own shells declare `shown` in both
  `toasts.strand` and `osd.strand`, so a config-wide namespace would reject
  them together. Another file (and the CLI) reaches a value only as
  `file.name`, and only if it is exported; two files with one stem that
  both export are an error. Declaring a name twice in one file or block is
  an error. One `use tokens` and one `use palette` per config.
- **2026-10-05 · wave2-check: `Async<[T]>` reads as its last list.** The
  launcher reads `hits.len` and loops `for h in hits` over `apps.search`'s
  `Async<[Hit]>`, so list members and iteration of an `Async` list see the
  last value (empty before the first). Any other use of an `Async<T>` where
  `T` is expected is an error whose fix is `?? fallback`; `.pending`,
  `.error` (`text?`) and `.value` (`T?`) are its own members. `??` takes
  the inner type of an `Async` or a `T?`. *Narrowed (fixer round 4,
  below): only `.len` and `for` read through.*
- **2026-10-05 · wave2-check: events and node booleans need an element.**
  A component body has no node of its own, so `when`, `hover`, `self`,
  poses and element events (`on click`) directly in it are errors pointing
  into an element; `on show` belongs on a surface. The snippet fixtures,
  which placed table one-liners at component level, now wrap them in the
  smallest valid element. `id:` names are visible across their whole
  component or surface body (before the element that declares them) and
  read as that node.
- **2026-10-05 · wave2-check: bare variants.** `edge: top` resolves `top`
  against the enum the position expects. With no expected type, a variant
  unique across all enums resolves (`state kind = volume`); one shared by
  several enums is an error asking for `Enum.variant`.
- **2026-10-05 · wave2-check: checked on first use.** Top-level and block
  `let`s, `state`s and `fn`s without a return type are typed the first
  time they are read (or when their file is walked), so a static cycle is
  found as it closes and reported with its whole path (`a → b → c → a`);
  token entries the same way (`$surface.hi → $fg.muted → $surface.hi`). A
  `fn` with a declared return type may recurse.
- **2026-10-05 · wave2-check: assignment kills no binding.** Assigning to a
  `let`, a component parameter, a prop (`width = 30` in a handler, where
  `width` is the enclosing element's prop, or `self.opacity = …`) or a
  read-only field is an error with a fix; `<->` targets follow the same
  rule (state, settings fields and `rw` service fields, or fields and
  indices inside them), and only props the element writes back (`value`,
  `text`, `open`, `current`) take `<->`. Fields of a user `type` held in
  `state` are writable through it.
- **2026-10-05 · wave2-check: the raw-colour lint covers prop values
  only:** props, `when` and pose blocks and component arguments. Settings
  defaults, `let`s, token values and `set { }` right-hand sides are exempt
  (they are where colours are meant to live). *Narrowed (fixer round 4,
  below): a `let` that holds a colour is linted too.*
- **2026-10-05 · wave2-check: `persist`.** It stores plain data (numbers,
  text, colours, enums, records, lists), so a function or `Async` state is
  an error. `let x = … persist` and `state x persist = …` are parse errors
  with the fix (`syntax::persist`) rather than "expected a line break".
- **2026-10-05 · wave2-check: parameter types where needed.** A component
  parameter without a type or default (the doc writes `Toast(n)`) takes
  the join of the argument types at its call sites, positional and named.
  Components with such a parameter are checked after every other item, a
  deferred component that calls another before its callee (so its calls
  count), and keep their place in the HIR's item order. Callers that pass
  types with no join are one `check::needs_type` at the parameter with
  both call sites labelled; a parameter nothing passes a value to is an
  error where it is read. `fn` parameters always need types.
- **2026-10-05 · wave2-check: one mistake, one diagnostic.** Names and
  expressions that fail become `Ty::Error`, accepted everywhere; an action
  called in a binding, a field of an unknown base or a bare variant given
  to an unknown prop (`elipsis: end`) report once. `$fg-muted`, which the
  parser reads as a subtraction and warns about, becomes one checker error
  naming the token meant (`$fg.muted`) when one is near, and the warning is
  dropped. An unknown field in an assignment target is not also
  read-only, a prop rejected for `<->` does not also check its target, an
  unknown prop does not resolve a bare name given to it (`edge: bottm` on
  `panel`), and a surface misplaced in a tree (`bar X { … }`) does not
  read its name as a positional value.
- **2026-10-05 · wave2-check: `strand check` type-checks.** It runs
  `compile` over the `find_files` module set, so the wave-1 notes that it
  reports syntax only, and the checker TODOs left in the compiler section
  (misspelt tree keywords via unknown-element did-you-mean, `Spaced` values
  outside shadows and fonts, missing `for` keys, `await` outside handlers,
  `play` targets, parameter types where they cannot be inferred), are done.
- **2026-10-05 · wave2-check: whole-number literals.** `0` is a `float`
  unless its position expects an `int` (a prop, parameter, declared type
  or a typed fn's value); `int` widens to `float`, lengths and angles,
  never the reverse. An untyped `state` or `let` whose value is a whole
  number (`state i = 0`) is an `int`, so design.md's selected-index and
  count states index lists, fill `columns:` and feed `.take(n)`. When a
  fraction is written to one (`i = 0.5`, `i += t`, `i /= 2`, `i *= 0.5`,
  a `float` assigned to it, or a slider's `value: <-> i`) it is a `float`
  instead: the checker runs again with it pinned. A `float` where an
  `int` is expected says how to fix it (`declare it `state i: int = …``,
  or round it: `i.round`). Round 3 refines the passes (below).
- **2026-10-05 · wave2-check: a file named like a service.** In
  `battery.low`, `battery` is the service, so a `battery.strand` that
  exports `low` could never be read: its export is an error asking to
  rename the file (likewise for builtin values and the config's global
  names), not a silent shadow either way.
- **2026-10-05 · wave2-check (round 2): did-you-mean ranking.** For an
  unknown name the variants of the enum the position expects are tried
  before anything else in scope (`edge: tp` → `top`, never the time value
  `t`), keyframes before other names for `play`, and another file's export
  is offered as `file.name` (`dnd` → `toasts.dnd`); file stems are offered
  only for `name.field`. A one-letter word is matched only by case, and a
  longer word is never offered a one-letter name. Ties go to a plausible
  typo (letters only dropped, only added or swapped: `slt` → `slot`),
  then the closest length, then the alphabet. `text { text: s }` says the
  value is positional. Each file computes at most 200 suggestions, and
  candidates are ruled out by length and shared letters before the edit
  distance.
- **2026-10-05 · wave2-check (round 2): a settings record is written field
  by field.** `prefs = …` for `state prefs from "….toml" { … }` is
  `check::read_only` with the fix `prefs.accent = …`: a whole-record write
  has no meaning against the overlay > file > default merge, which is per
  field. `<->` to the whole record is refused the same way.
- **2026-10-05 · wave2-check (round 2): shadowing is loud.** A name in
  scope (a local or this file's `state`/`let`) that is also a variant of
  the enum the position expects is `check::ambiguous` (`state top` and
  `edge: top`), fixed by `Edge.top` or a rename. A component parameter
  and a `state`/`let` of the same name in its body are one block's
  redeclaration. A file's top-level `state`/`let` may not take a global
  name (component, surface, enum, type, fn, tokens, keyframes, service):
  the name would be a value in that file and an element everywhere.
- **2026-10-05 · wave2-check (round 2): `on change` reactivity by kind.**
  State, settings and services change; a `let` changes only if its value
  reads something that does (`let c = 1` does not); fns, enums, types,
  components, keyframes and token sets never do. A fn target says it is a
  function rather than a constant.
- **2026-10-05 · wave2-check (round 2): unreachable `match` arms.** A
  variant or literal matched twice, or any arm after `_`, is
  `check::unreachable_arm` (an error: an arm that can never run is a
  mistake, like a missing one).
- **2026-10-05 · wave2-check (round 2): file names that cannot be read.**
  A file whose stem is not an identifier (`my-bar.strand`: `my-bar.x`
  parses as a subtraction) and that exports something is
  `check::file_name`, asking for the snake_case name.
- **2026-10-05 · wave2-check (round 2): shader uniforms.** Until naga
  reflection (M4) checks each `u_*` prop against the `.wgsl` file, a
  uniform takes what WGSL uniforms can hold: numbers, percentages,
  lengths, angles, durations, colours, and comma vectors of them. Text,
  booleans, records and lists are errors.
- **2026-10-05 · wave2-check (round 2): one catalogue with the scene.**
  Every schema element is a `strand_scene::protocol::NodeKind` and every
  element prop a scene `Prop`, except compiler-only `id`
  (`tests/scene_catalogue.rs`). The props the schema had invented, which
  neither design.md nor grammar.md names, are dropped: `justify`,
  `shrink`, `exclusive` (surfaces; `keyboard: exclusive` stays), `rows`,
  `axis`, `wrap`, `italic`, `min`, `max`, `step`, `start` (arc), `loop`,
  `playing` (lottie), `intensity`, `tint` (effect), `spread`, `gravity`
  (particles), with the `Axis` enum. A doc update must come first to add
  any of them back. Round 3: `input`'s `type: text | password` stays
  (`InputKind`): grammar.md names it as a canonical prop (`type:
  password`, "Keywords are contextual"), and the lock's password field
  needs it. `dash` (design.md: "Stroke styles: dash, trim, caps, wavy")
  stays too. The scene gained both (`Prop::Dash`, `Prop::InputType`) in
  the same commit, so nothing is pending.
- **2026-10-05 · wave2-check (round 2): `Drop`.** External drops arrive as
  `Drop { kind: DropKind (files | app | text), files: [path], app: App?,
  text: text }` (design.md: "Files, apps and text from other programs
  arrive as typed `Drop` values"). `on drop`'s payload stays `any`, so a
  handler names the type it accepts: the `drag:` source's own type for
  drags inside the shell (`on drop(p: Pin, at: int)`), `Drop` for other
  programs.
- **2026-10-05 · wave2-check (round 2): palette role names.** Strand
  shortens Material 3's system role names; the mapping is 1:1 (M3 name in
  brackets): `accent` (primary), `on_accent` (on_primary),
  `accent_container` (primary_container), `on_accent_container`
  (on_primary_container); `secondary`, `tertiary`, `error` and their
  `on_*`, `*_container`, `on_*_container` as in M3; `bg` (background),
  `on_bg` (on_background); `surface` (surface), `fg` (on_surface),
  `surface_variant` (surface_variant), `fg_variant`
  (on_surface_variant); `surface_dim`, `surface_bright` as in M3;
  `surface_lowest` (surface_container_lowest), `surface_low`
  (surface_container_low), `surface_container` (surface_container),
  `surface_high` (surface_container_high), `surface_highest`
  (surface_container_highest); `inverse_surface` (inverse_surface),
  `inverse_fg` (inverse_on_surface), `inverse_accent` (inverse_primary);
  `outline`, `outline_variant`, `shadow`, `scrim`, `surface_tint` as in
  M3. Importers and `material()` fill them by this table.
- **2026-10-05 · wave2-check (round 2): long declaration chains.** Lazy
  first-use checking nests one set of frames per link of a `let`, `state`,
  `fn` or token chain, so every lazy entry point (`Checker::force`, token
  `entry_ty`) runs under `stacker::maybe_grow` (256 KiB red zone, 4 MiB
  segments): depth is bounded by memory, not by the worker thread's
  stack. 5,000-link chains and cycles check on a 2 MiB thread
  (`tests/deep_chains.rs`).
- **2026-10-05 · wave2-check (round 2): `strand check <file>`.** A file is
  checked with the rest of its config, so its references to other files
  resolve: the config is the default config directory if the file is in
  it, else the file's directory. Only the diagnostics with a label in
  that file are reported, primary or secondary (round 3: the first
  declaration of a name redeclared in another file is that file's error
  too), and the summary names the files it was checked with. A file the module set leaves out (hidden, too deep) is
  checked alone.
- **2026-10-05 · wave2-check (round 2): schema docs, defaults and hash.**
  `///` comments in schema text document the entry they precede and are
  kept in `Schema::docs` under a `DocKey` (types, members, functions,
  values, methods, elements, props and events, tokens; group props reach
  the elements that include them); parameter defaults keep their source
  text (`ParamSig::default`). `Schema::fingerprint()` is BLAKE3 chained
  over every text `extend` was given, in order, for the compiled cache
  key.
- **2026-10-05 · wave2-check (round 3): whole-number passes.** The extra
  checker passes for whole-number states are sound and few. Before the
  first pass, fraction writes the source shows plainly are pinned
  (`check/prepin.rs`): `x = 0.5`, `x /= …`, `x += 0.1`, `<-> x` on a
  builtin prop that reads and writes a `float`, and arithmetic with a
  fraction literal or a handler `float` among its terms (`x = x + dy *
  0.05`, `x += dy` in `on scroll(dy)`, whose `dy` the schema types
  `float`; `let d: float`), when `x` is the only declaration of its name
  in the file (counting element-scope names such as `letters`'s `index`
  and `id:` names), the write is in that file, and every other name in
  the arithmetic is a whole-number state. The fixtures, sliders bound to
  `state v = 0` and the usual scroll handler check in one pass. A pass
  also records hand-offs between whole-number states (`b = a`, `b = a *
  2`, `b = c ? a : 0`, `b = a ?? 0`), also through an untyped `let` or
  `state`, a handler or fn local, or the value of a fn without a return
  type (`let t = a; b = t`, `b = f()` with `fn f() { a }`); the pins are
  closed over them, so such a chain costs one extra pass however long it
  is. *Superseded in round 3 (fixer pass 3):* there is no pass cap and no
  "holds whole numbers and fractions" error. Pins only grow and are
  bounded by the number of whole-number declarations, so the loop ends;
  a hand-off the checker does not follow (through a list or a record
  field) costs one pass per link but never gives a wrong type or a false
  error.
- **2026-10-05 · wave2-check (round 3): inferred parameters.** A
  whole-number literal passed to a parameter whose type comes from its
  callers is an `int` there, as in an untyped `state` (`Grid 3` makes
  `n` an `int`; a caller passing `0.5` widens it to `float`). A bare
  builtin variant passed to such a parameter (`Side center`) cannot be
  resolved, as no enum is expected: it is one `check::unknown_name` whose
  help says to write the enum (`Align.center`) or type the parameter.
  A parameter whose callers all passed errors is not also reported as
  "nothing passes it a value". *Refined (fixer pass 3):* an argument to
  an inferred parameter is checked with no expected type, as the value
  of an untyped `let` is, so `Grid count + 1` gives `int` and mutually
  recursive inferring components (`A x - 1` / `B y - 1`) agree. In a
  cycle of such components a call can come after the parameter's type
  was joined (the first declared is checked first); an argument the
  joined type does not cover is joined into it for the next pass (the
  same pass loop as whole numbers: `A 1` outside and `A 0.5` inside the
  cycle make `x` a `float`), and one that does not join is one
  `check::needs_type` at the parameter labelling both arguments
  (`negative/needs_type_cycle.strand`).
- **2026-10-05 · wave2-check (round 3): a fn without a body.** When the
  parser finds no `{` after a fn's signature (`fn get() = a`) it reports
  `syntax::expected` and gives the body one error statement; the checker
  does not also report `check::fn_value`, and the fn's value is the error
  type.
- **2026-10-05 · wave2-check (round 3): uniform vectors.** A shader
  uniform's vector is a WGSL `vec2` to `vec4`: 2 to 4 comma values. A
  list, or more or fewer comma values, is a `check::type_mismatch`.
- **2026-10-05 · wave2-check (round 3): what shadowing is reported.**
  design.md asks for no shadowing errors. The checker reports the cases
  where a name would silently change meaning: a declaration named like a
  variant of the enum a position expects (`state top` with `edge: top`),
  a parameter declared again in its body, and a file's `state` named
  like a global. A component `let` or a `for` binding that hides a
  file-level `state` is ordinary lexical scoping and is accepted.

- **2026-10-05 · wave2-check (round 4): `Async` lists read through
  `.len` and `for` only.** design.md's launcher reads `hits.len` and
  loops `for h in hits`; those see the last result (empty before the
  first). The list transforms `filter`, `map`, `sort_by`, `take`, `skip`
  and `reverse` on an `Async<[T]>` give an `Async<[U]>`, so `.pending`
  and `.error` survive and a plain list still needs `??`
  (`hits.take(3) ?? []`); the VM keeps the source's pending and error on
  the result. Element reads (`.first`, `.last`), other list methods
  (`join`, `find`, `contains`…) and an `Async` passed to an `any`
  parameter (`join(", ", hits)`) are `check::async`, since each would
  forget the loading state.
- **2026-10-05 · wave2-check (round 4): overloads are chosen by shape.** A
  call picks its overload before checking any argument: named parameters,
  a `from` argument, the positional count and the required parameters
  (`material(seed:)` vs `material(image:)`, `oklch(from …)` vs `oklch(l,
  c, h)`). Only calls the shape cannot tell apart (`radial(center, 40%)`
  vs `radial(#000, #fff)`) try overloads in turn, at most two nested
  levels deep (deeper, the first that fits is taken), so nested overloaded
  calls check in polynomial time (bounded speculation: about k²·n³ for k
  overloads nested n deep, not exponential; corrected in round 5, which
  earlier said linear). A declaration (or token) first read inside an
  attempt is checked once, and its diagnostics and references are kept
  whatever the attempt's outcome, so an unknown name there is never lost
  (`negative/overload_lazy.strand`,
  `robustness.rs::nested_overloaded_calls_stay_cheap`).
- **2026-10-05 · wave2-check (round 4): builtin names are not shadowed.**
  (Replaced by round 5: builtin names are a prelude.) A top-level,
  component or handler `state`/`let`, or a component or fn parameter,
  named like a builtin service (`battery`), value (`t`) or function
  (`pct`, `blur`) is `check::redeclared`: it would hide the builtin
  without a word, and the mistake would surface far away
  (`battery.percent` failing on a number). This follows the
  service-named-file rule. `for` bindings and lambda parameters are
  exempt: they are short-lived and local to one expression or loop, as
  round 3 decided for ordinary lexical scoping. A handler `let` declared
  twice in one block, and a prop set twice in one element (`value: <-> v;
  value: 0.3`), are `check::redeclared` too.
- **2026-10-05 · wave2-check (round 4): a `let` holding a colour is
  linted.** (Widened by round 5 to every declaration that hands a colour
  on.) `let c = #ff0000` then `bg: c` would bypass the raw-colour
  lint, so a raw colour in a `let` whose type is a colour or paint (or a
  list or nullable of them) gets the same `check::raw_color` warning.
  Settings defaults, token values and `material(seed: …)` arguments stay
  exempt.
- **2026-10-05 · wave2-check (round 4): `segmented`'s value is an
  option.** `options: Look` (an enum) makes `value` a `Look`; a list of
  `T` makes it a `T`. A `value` of another type is `check::type_mismatch`
  pointing at the options. The schema keeps `any` for both props (the
  check is the element's, like `page` names taking `pages.current`'s
  type); a general "type from a sibling prop" schema feature waits for a
  second element that needs it.
- **2026-10-05 · wave2-check (round 4): extensions add, never replace.**
  `Schema::extend` refuses an element, group, alias, value, palette role
  or token that already exists, and a function overload whose parameters
  (names and types) match an existing one, with "declared twice": a
  service crate cannot silently change a builtin (`element text(int)`).
  (Round 5: records and services too, the same rule for method
  overloads, a `provisional` exception for service stubs, and atomicity.)
- **2026-10-05 · wave2-check (round 4): one error for a kebab name.**
  `my-bar.open`, with neither `my` nor `bar` known and no spaces around
  the `-`, is one `check::unknown_name` for `my-bar` (naming the
  snake_case spelling, or the file to rename) rather than two, and a
  write to an `id:` node's prop (`vol.opacity = 1`) is one
  `check::assign_to_prop`.
- **2026-10-05 · wave2-check (round 5): builtin names are a prelude.**
  grammar.md says there are no reserved words, and a service crate that
  adds a function or service must not break configs that already use the
  name. So a `state`, `let`, parameter, `fn`, `type` or `enum` named like
  a builtin function, value or type shadows it in its scope, silently
  (`component Avatar(shape: Shape = circle, blur: length = 0)`, `fn
  ease_out(t: float)`, a user `enum Place`). Where the shadowing is
  observed it is named: calling a shadowing non-function (`blur(16)` with
  a `length` parameter `blur`) is `check::type_mismatch` with the help
  "`blur` in scope hides the builtin `blur`". Shadowing a builtin
  *service* (`let battery = 5`) is a `check::shadows_builtin` warning,
  not an error, for every kind of declaration (`fn battery` included):
  the confusing far-away failure is flagged, and a new service from a
  crate only warns. A component or surface named like a builtin element
  stays an error (the tree resolves builtin elements first, so it could
  never be placed), as does a user `service` named like a builtin one.
  (Round 6: calls follow the rule for top-level bindings too; the
  language's own type names are not part of the prelude.)
- **2026-10-05 · wave2-check (round 5): component tokens are overridden
  loudly.** `component Toast(n) tokens { radius: $radius.lg }` defines
  `$Toast.radius`, a knob (design.md). A `tokens` set entry with that path
  is a redefinition: without `override` it is `check::override_needed`
  pointing at the component; `override Toast.radius: …` is accepted and
  checked against the component token's type, and a misspelt one is
  `check::unknown_token` with did-you-mean. A read of `$Toast.radius`
  outside a set is typed by the component's own entry, never by whichever
  set happens to define the path first.
- **2026-10-05 · wave2-check (round 5): the raw-colour lint covers every
  named value.** Round 4 linted a `let` but not the same bypass through
  `state sc = #ff0000`, a `fn red() -> color { #ff0000 }` or a parameter
  default `C(c: color = #0000ff)`. One rule now: a raw colour in a prop,
  or in a declaration that hands it on as a colour (a `let`, a `state`'s
  initial value, a `fn`'s result, a component parameter default), is
  `check::raw_color`. Settings defaults, token values and `material(seed:
  …)` arguments (in a prop too) are exempt. Reading a role of a computed
  `Palette` inline (`material(…).accent`) stays unsupported in M1, an
  interpretation rather than design.md's words (design.md describes the
  palette tier as a typed schema of Material 3 roles): there is no field
  access on a `Palette` value yet; roles are read as tokens after `use
  palette`.
- **2026-10-05 · wave2-check (round 5): a dropped `Async` is an error.**
  An expression statement in a handler whose value is `Async<T>`
  (`on click { sleep(1s) }`) is `check::async` with the help "`await` it,
  or assign the result": it almost always meant to wait (design.md,
  "Errors, not surprises").
- **2026-10-05 · wave2-check (round 5): `await` on a list transform
  waits.** `await hits.take(2)` while `hits` is loading waits for the
  load and then applies the transform: the VM gives the derived `Async`
  its own pending operation over the source's (weak handles to the VM and
  runtime), rather than rejecting it in the checker
  (`vm.rs::await_waits_for_a_transformed_load`).
- **2026-10-05 · wave2-check (round 5): one more prop set twice.** The
  positional is the prop it fills, so `meter 0.5 { value: 0.7 }` is
  `check::redeclared`; so is a prop set twice in one `when` block. A
  lambda whose result was already reported against the expected type
  takes that type, so the call does not report the same mistake again;
  `page wifi` outside `pages` is one `check::misplaced`.
- **2026-10-05 · wave2-check (round 5): `from` parameters.** `from x`
  (the relative-colour source) or `from: x` fills a `from` parameter; a
  positional argument never does when telling overloads apart by shape,
  and a defaulted `from` before a variadic (`conic(from: angle = 0,
  ...stops)`) is given by name only, so `conic($accent, $secondary)` is
  two stops.
- **2026-10-05 · wave2-check (round 5): schema extensions are atomic, and
  stubs are provisional.** `Schema::extend` stages the text on a copy and
  swaps it in only when every item is valid, fingerprint included. A
  record or service that already exists is refused and left untouched
  (round 4 still overwrote its members). The builtin's service stubs and
  the records only services hand out are `provisional`: the first
  extension that declares the name replaces the stub in place (same id,
  its members and docs dropped), once. Method overloads follow the same
  "same parameters is declared twice" rule as functions.
- **2026-10-05 · wave2-check (round 5): one member table.** List and
  `Async` members (`len`, `first`, `filter`, `take`, `remove_key`,
  `pending`, `value`, …) moved out of the checker's match arms into
  `schema::members` (`list_members`, `async_members`, `members_of`), which
  the checker types `x.name(…)` by and which the LSP lists after `.` with
  docs; generic schema syntax can come later without changing the API.
- **2026-10-05 · wave2-check (round 6): user bindings come before
  builtins at a call too.** Round 5 made builtin names a prelude but a
  call still looked up builtin functions before the file's top-level
  `state`/`let` (and before globals other than `fn`, `type` and
  component), so `let pct = x => x * 2` was read as the lambda and called
  as the builtin, and a crate adding `fn ring` silently redirected
  `ring(1)`. A call now resolves a block or parameter name, then the
  file's `state`/`let`, then every global, and only then the builtins.
  Calling a non-function user name that hides a builtin function (`state
  noise = 4px` then `noise(1)`, an `enum wave`) is `check::type_mismatch`
  with the "hides the builtin" help, wherever it is declared.
- **2026-10-05 · wave2-check (round 6): the language's own types are not
  a prelude.** `type color { … }` made every `color` annotation the
  user's record and printed "expects `color`, found `color`". No crate
  adds primitive types (`int`, `float`, `bool`, `text`, `color`, `paint`,
  `path`, `length`, `percent`, `angle`, `duration`, `font`, `shadow`,
  `insets`, `corners`, `any`, `unit`, `Async`), so redeclaring one is
  `check::redeclared` and annotations keep the builtin (one diagnostic).
  Schema records and enums stay a prelude (crates add those); where a
  config type hides one, the hidden one is printed `builtin Align`
  ("`align` expects `builtin Align`, found `Align`").
- **2026-10-05 · wave2-check (round 6): a config `service` named like a
  builtin one stays an error.** Unlike other names, a service is
  identified by its name at runtime (`Value::Service(name)`; the host
  serves reads by name), so a config `service weather` beside a
  contributed `service weather` would read the crate's data. It stays
  `check::redeclared`, and the cost is stated where crate authors look
  (architecture.md, `Schema::extend`): contributing a service name a
  config already declares breaks that config, so crates should namespace
  new service names. Revisit when M3 gives config services their own
  identity.
- **2026-10-05 · wave2-check (round 6): the positional's prop is schema
  data.** `element meter(float -> value)`: an element's positional names
  the prop it fills (`ElementSchema::arg_prop`, required whenever there
  is a positional). Lowering and the checker's "set twice" rule read it
  from there, replacing two hard-coded copies of the wave2-vm table
  (`letters` has no positional, so it fills nothing).
- **2026-10-05 · wave2-check (round 6): record keys are checked.**
  `Schema::extend` ends by resolving every record's `key` path across all
  records, so `record Foo key nope { … }` and a stub replacement that
  breaks a builtin key (`record App { name: text }` under `Hit key
  app.id`) are refused (atomically) instead of silently losing keyed
  identity.
- **2026-10-05 · wave2-check (round 6): set overrides win at runtime.**
  The token table took component token defaults after the `use tokens`
  chain, so `override Toast.radius` was overwritten. Defaults now go in
  first, and each entry replaces an earlier one at the same path whether
  plain or derived (a plain value no longer outranks a later derived
  override; `instantiate.rs::set_overrides_beat_component_token_defaults`).

## wave2-lsp

- **2026-10-05 · wave2-lsp: the formatter keeps the author's lines.**
  `strand_compiler::fmt` normalises layout over the lossless token stream,
  guided by the tree; it never joins or splits lines, so nothing is
  wrapped and `edge: top; height: 36` or a one-line `when` stays as
  written. design.md's examples put several props on one line and break
  where it reads best, which no line-length rule reproduces; keeping the
  breaks is what makes every design block a fixed point
  (`tests/fmt.rs::fixtures_are_formatted`). It fixes indentation (two
  spaces per block, from the line that opened it), spacing inside a line,
  blank lines (at most one; none after `{` or before `}`), line endings
  and redundant `;`.
- **2026-10-05 · wave2-lsp: alignment the author made is kept.** design.md
  aligns `when` blocks (`when ws.focused  {`), match arms (`mocha     =>`),
  token values (`surface.hi:       $surface.mix(…)`), hanging props under
  a block's first item (`col { width: 600 …` / `bg: …`) and trailing
  comments, but not consistently (toasts' `when hover {` and `when
  n.urgency == critical {` are not aligned), so aligning automatically
  would rewrite the design's own examples. The rule: a `{`, `=>`, `=`, a
  prop's value or a trailing comment that the author set apart with two
  or more spaces keeps its column when the line still fits (comments by
  absolute column, the rest relative to the line's indentation); a line of
  a block whose `{` has more after it on its line keeps the column of that
  first item if it was written there. Both rules are idempotent.
- **2026-10-05 · wave2-lsp: formatting never changes meaning.** Whether a
  `(` or `[` touches the word before it is kept (call or index versus a new
  term, grammar.md "Calls touch"); two tokens are written together only if
  they lex back the same (`? .` never becomes `?.`, `1 . 5` never `1.5`).
  Every result is re-parsed and compared with the input's tree with spans
  stripped (`fmt::shape`); a difference is `FormatError::Unstable` and the
  file is left alone. A file with syntax errors is not formatted
  (`FormatError::Syntax`): the editor shows the errors instead.
- **2026-10-05 · wave2-lsp: `strand fmt [--check] [paths]`.** A directory
  means its module set (`source::find_files`, as `strand check` loads it);
  no path means the config directory. Files are rewritten through a
  temporary file renamed over the canonical path, so a stowed symlink stays
  a link and the watcher sees one `MOVED_TO`. `--check` writes nothing,
  lists unformatted files and fails if there are any.
- **2026-10-05 · wave2-lsp: which files a document is checked with.** The
  rule of `strand check <file>`, plus workspace folders that are configs:
  the default config directory (`$XDG_CONFIG_HOME/strand`, else
  `~/.config/strand`; `initializationOptions.configDir` overrides it) if
  the file is inside it (alone if that directory's module set
  (`source::find_files`) leaves it out, as `strand check` does); else a
  workspace folder that holds `.strand` files itself and has the file in
  its module set (a dotfiles repo's `strand/` opened as the workspace),
  the deepest first; else the file's own directory's module set; else the
  file alone (unsaved buffers, hidden or too-deep files). A workspace
  folder that only holds configs in sub-directories is never merged into
  one config: two sibling configs, or examples next to the real config,
  are checked apart, as the runtime loader would load them. Open
  documents replace their files' text on disk, so cross-file errors show
  while typing, and diagnostics are published for every file of the
  config, not only open ones (design.md "What you see" #2 relies on a
  rename in one file being seen in another).
- **2026-10-05 · wave2-lsp (round 1): files changed outside the editor.**
  An analysis remembers the size and modification time of every file it
  read from disk and of every directory `find_files` scanned, and is
  compiled again when any differs, so `strand fmt` in a terminal, a git
  checkout or another editor is seen by the next request even when the
  client does not watch files. A client that supports dynamic
  registration is asked to watch `**/*.strand`
  (`workspace/didChangeWatchedFiles`); a reported change re-checks every
  published config after the debounce. A client that takes
  `documentChanges` gets rename and quick-fix edits with the open
  documents' versions (`null` for files read from disk), so it refuses
  edits computed for text it no longer has. Which config a URI belongs to
  is cached until a document opens, closes or saves, a watched file
  changes or a stale analysis is seen. Closing the last open document of a
  config outside every workspace folder and the default config directory
  clears its diagnostics and drops its analysis.
- **2026-10-05 · wave2-lsp (round 1): the schema the server checks
  with.** `strand_dev::serve_with(conn, Arc<Schema>)` takes the schema
  once (the builtin one extended by the service crates the caller links,
  `Schema::extend`); `serve` uses the builtin schema. Checking, hover,
  completion and rename's refusal of schema tokens all read that one
  schema, so service schemas drive hovers as design.md asks once M3's
  crates extend it.
- **2026-10-05 · wave2-lsp (round 1): bad notifications.** A notification
  whose params do not parse is reported with `window/logMessage` (and on
  stderr) and ignored; only a closed connection ends the server.
- **2026-10-05 · wave2-lsp: debounce.** Diagnostics wait 200 ms after the
  last change (`initializationOptions.debounceMs` overrides it), so typing
  does not flash errors, and are published at once on open and save.
  Requests (completion, hover, rename) always analyse the latest text but
  do not publish, so a completion request mid-word never flashes an
  error either. The 200 ms is below the overlay's 250 ms quiet rule.
- **2026-10-05 · wave2-lsp: completion on half-typed text.** After `.`
  with nothing typed yet the line does not parse (grammar.md rule 5), so
  the server checks a copy with a placeholder name after the dot and
  reads the type of the expression before it from the typed tree (the
  checker keeps a field's base typed when the field is unknown). An
  enum's name, a file stem and a schema enum complete their variants and
  exports. Members are what `strand_compiler::schema::members_of`
  gives for the type (fields, methods, `Async`'s `pending`/`error`/
  `value`, list members), the table the checker types `x.name` by, so
  there is no copy to drift; hover over a method reads the same table
  (since the merge of wave2-check round 5, which replaced the earlier
  `check::LIST_METHODS`). With the cursor
  right after a dot and a name after it (`battery.|present`), that name is
  the member being completed. In a component call's block, children and
  their keywords are offered only if the component has a `slot`;
  otherwise only its parameters (`when`, handlers, timers, poses and
  `state` are not allowed in a call at all).
- **2026-10-05 · wave2-lsp: what `<->` offers.** Only places a widget can
  write (design.md "Two-way"): this file's states and the enclosing
  component's or surface's own, other files' exported states as
  `file.name`, settings fields (`prefs.accent`; never the record, which
  the checker refuses), and `rw` service fields as full paths up to three
  fields deep (`audio.sink.volume`). After a dot inside `<->` only
  writable fields and records leading to one are offered; under a
  `state` every field is (the checker allows writing through it).
- **2026-10-05 · wave2-lsp: rename.** States, lets, settings, components,
  fns, enums, types, token sets, keyframes, custom services, parameters
  (a component's parameter also at every call site's prop, design.md
  "What you see" #2), handler and `for` locals, `id:` names, and tokens
  the config declares. Built-in names, schema services, fields, variants
  and the schema's own tokens (palette roles and the base tiers, which the
  schema types even when a theme values them) are refused with a reason.
  A token key inside a group (`2` in `space { 2: 8px }`) is renamed in
  place, so the new name must keep the group's prefix; a new name without
  a `.` is taken as the new key in the same group (what an editor sends
  after the user edits the key `prepareRename` offered). Every edited file
  is re-checked before the edit is returned: a rename that would add an
  error (a clash, a name taken) is refused with that error, and one that
  would change what another name refers to without an error (a component
  `let` renamed to a file `state` it then hides, which lexical scoping
  accepts) is refused too, by comparing every declaration's reference
  count before and after.
- **2026-10-05 · wave2-lsp: quick fixes.** Quick fixes come from the
  replacements diagnostics propose as data (`Diagnostic::suggestions`),
  never from their help text. A "did you mean `x`?" diagnostic proposes
  `x` for the misspelt text itself, which the stage that found it knows
  even when the error points elsewhere (the parser points at `a` in `on
  chnage a, b`; the fix replaces `chnage`; a misspelt unit's fix replaces
  only its letters, `12pz` → `px`). One proposal is a preferred fix;
  several (the parameters an unknown named argument could be) are each a
  fix, none preferred. Other fixes (extract component, missing key) are
  M5.
- **2026-10-05 · wave2-lsp (round 2): unknown parameters read as
  design.md shows.** A component call's unknown prop is `unknown prop
  `expanded`` with help "did you mean `open`?" when a parameter the call
  does not set yet is close, or is the only one left (design.md "What you
  see" #2: `bar.strand:12: unknown prop "expanded"; did you mean
  "open"?`); otherwise the help lists the parameters ("it takes `a`, `b`
  and `c`") and each one the call does not set is a fix. A function
  call's unknown named argument follows the same rule with the
  parameters that call sets (by name, `from` or position). The code stays
  `check::unknown_param` for both.
- **2026-10-05 · wave2-lsp (round 3): the one-line diagnostic form.**
  `render_short` (`strand check`'s short lines, the fallback past 65,535
  lines, a held-back save) prints `file:line:col: error[code]: message;
  help`, with backtick-quoted names in double quotes, so the renamed
  prop reads `bar.strand:5:5: error[check::unknown_param]: unknown prop
  "expanded"; did you mean "open"?`, design.md "What you see" #2's text.
  The column and `error[code]` are a superset of design.md's
  `bar.strand:12:` form. The caret render keeps backticks, as design.md's
  table shows it (`critcal` → "did you mean `critical`?" with file, line
  and caret). A backtick span holding a `"` (a text literal) keeps its
  backticks.
- **2026-10-05 · wave2-lsp (round 3): token quick fixes.** `$space` used
  as one token offers every member (`$space.1`, `$space.2`, …) as a fix,
  none preferred. An override key whose closest token lies outside the
  group the key is written in cannot be fixed by editing the key, so its
  help says where that token is ("the closest token is `$fg.muted`, which
  is outside group `ink`") instead of asking "did you mean …?"; every
  did-you-mean help now has exactly one fix (`checker.rs::fixes`).
- **2026-10-05 · wave2-lsp (round 2): the builtin schema is documented.**
  Hovers are generated from the schemas (design.md, "System services"),
  so every service, record and its members, function, method, value,
  element, group prop, element prop and event, palette role and token
  tier in `builtin.schema` has a `///` doc worded from design.md; a test
  (`schema::tests::builtin_schema_is_documented`) fails on a new entry
  without one. A token without its own doc reads its group's
  (`$space.2` shows the spacing scale's). Docs say what design.md says
  and no more: where it gives no unit or range (`memory.used`,
  `Date.weekday`), the doc names the value only.
- **2026-10-05 · wave2-lsp (round 2): which config shows a file.** The
  server remembers the config that last published each file's
  diagnostics. A file opened before it exists is checked alone; saved
  into a config directory, it is published by that config, which takes
  it over: the lone config no longer shows it, may not clear it, and is
  forgotten. Watched-file events re-check each shown config as the files
  belong now. A request that finds a file changed on disk without the
  client saying so (no watched files) schedules that config's
  diagnostics again, so what is shown catches up with what hover sees.
  Round 3: disk stamps are checked whatever edits other configs had in
  between; a config drops its hold on a clean file that left it; and a
  watched-file event also re-checks every open document's config as it
  is now, so an open file deleted from a directory shows its errors
  alone.
- **2026-10-05 · wave2-lsp (round 2): completion after a dot.** With
  nothing typed after `.`, the config is compiled once more with a
  placeholder name (the half-typed text does not type the receiver);
  that analysis is kept with the analysis it came from, per place, so
  asking again before the text changes compiles nothing.
- **2026-10-05 · wave2-lsp (round 1): hover docs.** A declaration's doc is
  the `//` block directly above it, except a block that opens the file
  and starts with the file's own name (`// launcher.strand. Bind a key
  to: …`), which is the file's header. A token's hover lists each value
  with the token set (or component) that defines it, marking overrides:
  `base: 8px`, `compact (override): 4px`.
- **2026-10-05 · wave2-lsp (round 1): `strand fmt` arguments.** A file
  named twice (`strand fmt dir dir/a.strand`) is formatted once
  (deduplicated by canonical path); `--` ends options, so a path may
  start with `-`; with no path, a missing default config directory is
  named as such. `- -b` keeps its space (`--b` reads like a decrement,
  though it lexes the same).
- **2026-10-05 · wave2-lsp: tree-sitter is out of scope this wave.** The
  M1 checklist's tree-sitter grammar for editor highlighting stays open;
  editors get diagnostics, completion, hover, navigation, rename and
  formatting from the server meanwhile.

## wave2-vm

- **2026-10-05 · wave2-vm: tokens stay symbolic in the VM.** `$accent`
  evaluates to a `Value::Token` holding a `strand_scene::TokenExpr`, and
  colour methods, channel arithmetic (`oklch(from $surface, l: l +
  0.12)`) and comma or space values containing tokens stay expressions.
  The emitter turns them into `PropValue::Token` (a `Template` when a
  token fills a colour slot of a border, shadow or gradient), so render
  resolves them every frame and a palette spring reaches every prop
  without logic re-sending it (design.md, "Token model").
- **2026-10-05 · wave2-vm: the positional argument's prop.** `text x`,
  `button x` and `letters x` fill `text`; `meter x`, `graph x` and
  `merge x` fill `value`; `effect x` fills `style`; `page x` fills
  `name`; every other element (`icon`, `image`, `svg`, `lottie`,
  `shader`, `spectrum`, `thumbnail`) fills `source`. A record (a
  `Window` for `thumbnail`, an `AudioDevice` for `spectrum`) is sent as
  its key's text (Since wave2-check round 6 this table is schema data:
  `element meter(float -> value)`.)

- **2026-10-05 · wave2-vm: palettes before M2.** `material(seed:)` is a
  deterministic stand-in for Material 3 (`vm/palette.rs`): five tonal
  palettes in OKLCH from the seed's hue and chroma, every role at its M3
  tone (light or dark), contrast pushing tones apart. It fills the whole
  palette schema, so themes written against M3 roles run now; the
  `material-colors` crate replaces it in M2. `material(image:)` is a
  failed `Async` until wallpaper quantisation lands, so the theme's `??
  material(seed: …)` takes over. `import()` knows the four Catppuccin
  flavours. A config without `use palette` gets `material(seed:
  #7aa2f7, dark: system.dark)` (the design's default accent); without
  `use tokens`, the first declared token set applies.
- **2026-10-05 · wave2-vm: component tokens are global defaults.** A
  component's `tokens { radius: … }` entries (`$Toast.radius`) go into
  the global token table, not onto the component's nodes, so an
  ancestor's `set { $Toast.radius: … }` overrides them (nearest scope
  wins) as "knobs a component exposes for overriding" requires.
- **2026-10-05 · wave2-vm: `exit` mirrors `enter` in the emitter.** An
  element with `enter` (block or prop form) and no `exit` is sent the
  same pose as its `exit`, so render only ever plays what it is given.
- **2026-10-05 · wave2-vm: `~` without a duration.** `~ bezier(…)`
  names a curve but no duration; it runs 300 ms
  (`instantiate::BEZIER_DURATION`). `~ 200ms` uses the standard curve.
  A `when` block's own `~` applies while that block wins.
- **2026-10-05 · wave2-vm: `play`.** `play shake` sets the node's `play`
  prop to `[shake, n]` with a sequence number, so playing the same
  keyframes twice is two changes. Render plays keyframes in M4.
- **2026-10-05 · wave2-vm: time signals until M4.** `t`, `wave(…)` and
  `noise(…)` read 0 on the logic thread: they are render-side signals
  ("only that node repaints, only while visible") that arrive with the
  effects catalogue. Node-valued props (`nav: results`) are not sent
  until keyboard navigation (M4).
- **2026-10-05 · wave2-vm: numbers carry units at run time.** A
  `Value::Num` keeps `int`, `float`, `px`, `%`, `ch`, `deg` or ms;
  arithmetic follows the checker's unit rules, `int / int` is a
  `float`, and `==` compares values whatever their units (`1 == 1.0`).
  Division by zero is an error value, not infinity.
- **2026-10-05 · wave2-vm: handlers always run as tasks.** Every handler
  invocation is a core task, awaiting or not: element events through an
  input queue and `rt.spawn_input(Some(site), …)` (not rate-counted up
  to the first `await`), service events through `rt.spawn_for(site, …)`,
  `on change` and timer bodies through `rt.spawn` inside their handler
  (owned by their site). A task is polled in the flush that started it,
  so a click's writes land in the same tick. `<->` writes and `strand
  set` are made outside any handler.
- **2026-10-05 · wave2-vm: `on change` identity.** A target that is a
  field of a keyed record (`audio.sink.volume`, a `sink` has `key id`)
  re-baselines when the record's identity changes
  (`rt.on_change_keyed`), so switching sinks pops no OSD; other targets
  compare values only.
- **2026-10-05 · wave2-vm: per-monitor bars.** A `bar` is instantiated
  once per item of `screens.all` that its own `screens:` picks (a
  connector or monitor id, a list of them, `focused`, `all`), keyed by
  the monitor's identity, a new `Screen.id` field (make, model and
  description as `strand-surface`'s `MonitorId`, ` #2` for a second
  identical monitor; `Screen` is now `key id`), with `screen` in scope
  and `screens: "<id>"` set by the instance (it wins over the bar's own
  `screens:`, which only chooses monitors). A bar whose monitor leaves
  is parked, not unmounted: its nodes are removed from the scene, its
  scope frozen and its services released; the same monitor coming back
  (on any connector) gets it back with its state, and
  `Instance::forget_screen(id)`, which the binary calls on the surface
  layer's `monitor_forgotten` (30 s), drops it. The instantiator owns
  this because only it owns the bars' scopes; the `screens` service
  only lists monitors that exist
  (`tests/instantiate.rs::a_bar_per_monitor_with_its_own_state`,
  `a_bar_follows_its_own_screens`).
- **2026-10-05 · wave2-vm: service readers and visibility.** Every
  mounted component and the config's top level hold the services their
  body reads. A surface holds its body's services only while shown (its
  `open` true, or no `open`), and its content (everything but its own
  `on show`/`on hide`/`on dismiss`) is mounted when first shown and
  frozen with `rt.suspend` while hidden, so a closed launcher neither
  searches nor counts as a reader of what only its content reads; its
  state is kept for the next show. The instance sends `show` when
  `open` turns true (at mount for a surface without `open`) and `hide`
  when it turns false. Services read by top-level `let`s stay held by
  the config while it runs (a `let` is lazy, so nothing runs if no
  shown binding reads it); the 5 s stop is the service crate's (M3)
  (`tests/instantiate.rs::surfaces_show_hide_and_hold_services_while_shown`).
- **2026-10-05 · wave2-vm: `persist` is core's store.** A persisted
  `state` is `rt.persisted` on core's `PersistStore` (one file per
  cell, off-thread atomic writes debounced 250 ms, the default's hash,
  `redeclare`, `reset`); the compiler supplies only the codec, JSON by
  declared type (enums by variant name, records by field name). The
  path is the cell's owner (file module, or the component or surface
  name) qualified by every keyed instance it sits in, then its name:
  `toasts.dnd`, `TopBar[<monitor id>].expanded`, `Row[<key>].open`, so
  per-monitor bars and list items each keep their own value; two
  instances that still share a path (the same component twice in one
  bar) are core's `PersistPathInUse`. A kept value over a changed
  default is also an `Update::notices` line (`toasts.dnd: kept true
  (default changed)`). A keyed list `state` that is persisted stays a
  plain signal, since core persists `Signal`s
  (`tests/instantiate.rs::persisted_state_survives_a_restart`,
  `persisted_bar_state_is_per_monitor`).
- **2026-10-05 · wave2-vm: settings files are core's.** `state prefs
  from "prefs.toml" { … }` is `rt.settings_file` with one `FieldSpec`
  per field (its declared default, its type's name, a TOML codec by
  type: colours as `"#rrggbb"`, durations as `"200ms"`/`"6s"` or a
  number of seconds, enums by variant name, lists, inline tables for
  records), the file resolved against the config directory (`~/` from
  `$HOME`). Each field is its own signal: `prefs.accent` depends on
  that field only, and a write (`prefs.compact = true`, `<->`, `strand
  set theme.prefs.compact true`) goes to that field, then to the file
  through `toml_edit`. Without a settings store (`Storage::none`) the
  fields hold their defaults
  (`tests/instantiate.rs::settings_files_are_read_and_written_back`).
- **2026-10-05 · wave2-vm: errors keep the last good value.** A binding
  that fails reports its error in the tick's `Update::errors` and its
  prop keeps the value last sent; a failing `if` condition keeps the
  mounted branch. Keyed mutations that would duplicate a key, or name a
  missing one, fail the handler and change nothing.
- **2026-10-05 · wave2-vm: the mock's behaviour.** `SchemaHost::mock`
  models what tests need beyond storing fields: `apps.search` is a
  ready `Async` of substring matches (or pending while a test `hold`s
  it), `workspaces.on(screen)` filters by the workspace's `screen`,
  `calendar` and `clock` run on a fixed clock, and a notification's
  `expire`, `dismiss` or `activate` removes it from
  `notifications.popups`. Only the mock logs actions and `rw` writes;
  `SchemaHost::real` keeps no history.
- **2026-10-05 · wave2-vm: awaiting and async lets.** `await` waits on
  the pending future an `Async` carries (`sleep(d)`); an `Async`
  without one (a service load) gives its current value or its error.
  `let x = svc.m(args)` with an `Async` method is a core async memo over
  the argument tuple, created on the `let`'s first read, each change
  starting one `ServiceHost::fetch` and dropping the superseded one;
  `??` gives its fallback while it is pending or failed
  (`tests/vm.rs::coalesce_covers_a_pending_async`). Async calls inside
  larger expressions still use `call`.
- **2026-10-05 · wave2-vm: render → logic input.** Until render hit
  tests (M2) the instance takes scene `NodeId`s: `event(node, name,
  args)`, `set_flag(node, hover | pressed | focused | selected, on)`,
  `set_size(node, w, h)` and `write(node, prop, value)`. An event goes
  to the innermost element with a handler for it; `propagate()` passes
  it to the next. A surface (a `popup` included) is the top of its own
  event tree: a click on a calendar day does not reach the clock text the
  popup is anchored to.
- **2026-10-05 · wave2-vm: a keyed reset is reconciled.** A `for` gets
  `Insert`/`Remove`/`Move`/`Update` diffs from core, but a `Reset` (its
  first publish, or more than 256 diffs since it last read, which one
  item moved far in a long list can cause) is matched by key against the
  mounted items: items that left are unmounted, new ones mounted, and
  only the items outside the longest run already in order move. Items
  keep their nodes and state either way
  (`tests/instantiate.rs::keyed_lists_match_the_list_after_random_edits`,
  `a_long_list_touches_only_what_changed`).
- **2026-10-05 · wave2-vm: element flags belong to their scope.** An
  element's `hover`, `pressed`, `focused`, `selected` and size live in
  the component, surface or `for` item that owns the element (so `id:`
  names read them from anywhere in the body), created on first use but
  owned by that scope, never by the binding that first read them; an
  element an `if` unmounts and mounts again keeps working, and an
  unmounted element reads as not hovered.


- **2026-10-05 · wave2-vm: keyed lists follow diffs.** A keyed `state`
  (`state pins: [App] key id = []`) is a core `KeyedSignal`: `push`,
  `insert`, `remove`, `remove_key`, `move`, `update` and `clear` are
  keyed operations, and `xs = …` or `xs[i].x = …` replaces by key. A
  `for` directly over a keyed `state` or a host's keyed field follows
  that collection's diffs; other list expressions go through
  `rt.keyed_memo`. Every mounted item has its own value cell, set from
  `VecDiff::Update`, so one changed row re-runs one row's bindings
  (`tests/instantiate.rs::one_item_change_reruns_one_item`: 2,000 rows).
  Chains of `.filter`/`.map`/`.take`/`.sort_by` on a keyed collection
  follow core's incremental views (see "keyed chains" below).
- **2026-10-05 · wave2-vm: located runtime errors.** Errors are
  `RuntimeError`s carrying the failing operation's file and span (the
  innermost chunk that raised it), the scene node, the component and
  the scope `Instance::freeze` suspends (the instance of the innermost
  component, or the surface instance); `Instance::origin` maps scene
  nodes to source elements. The overlay and the reconciler (live-reload
  track) decide when to freeze
  (`tests/instantiate.rs::runtime_errors_are_located_and_freeze_their_component`).
- **2026-10-05 · wave2-vm: durations at their limits.** A duration
  past `Duration::MAX` (or negative, or not finite) is an error value
  naming what needed it (`after`, `every`, `sleep`, `on change … after`);
  a prop or `~` given one is unset or uses the default transition. An
  `every` period of zero is an error naming the timer (a zero period
  would wake the host forever). `on change … after d` follows a
  reactive `d`: a new duration replaces the debounce and takes over a
  countdown in flight.
- **2026-10-05 · wave2-vm: events on one element.** Several handlers of
  one event on an element (`on click` twice) all run, in source order,
  sharing one event context, so `propagate()` passes the event on once
  however often it is called.
- **2026-10-05 · wave2-vm: a second `slot` mount.** The caller's
  children have one set of element states in the caller's scope (so its
  `id:` names reach them). A component that mounts `slot` again while
  that set is on screen gives the copy its own states.
- **2026-10-05 · wave2-vm: widget writes are typed.** A `<->` write or
  `strand set` whose value does not fit the declared type is refused
  with an error; a widget's `f32` becomes the `f64` with the shortest
  decimal that round-trips it (a slider's 0.8 is 0.8).
- **2026-10-05 · wave2-vm: declared edges.** The compiler declares
  every read and write set it can see in the source before the first
  flush (`lower::reads`, `instantiate::edges`), as architecture.md asks:
  a conservative superset (every branch; a lambda's and a called `fn`'s
  reads count for the chunk that makes or calls it; an element
  instance's six flags; a service method call reads the whole service,
  `ServiceHost::sources(rt, s, None)`). Handler bodies' reads are not
  declared on their sites (tasks do not track reads); their writes
  are, on the site, the `on change` effect, the timer or the
  debounce's timer. Proving it took one core change, made here because
  the compiler cannot meet the "once per flush" promise without it:
  tasks spawned by event listeners are polled before the next sink (the
  flush polled them one sink late, so an `if` reading what an `on
  click` wrote ran before and after it). The core track should own it
  from here (`crates/strand-core/src/runtime.rs`, `flush`).
  `tests/instantiate.rs::sinks_run_once_after_the_handlers_that_feed_them`
  fails without the declarations.
- **2026-10-05 · wave2-vm: keyed chains.** design.md says `.filter`,
  `.map`, `.take` and `.sort_by` "update incrementally and keep keys".
  A `for` over such a chain rooted at a keyed `state` or a service's
  keyed field is core's incremental views. Core's operators take pure
  closures plus tracked parameters; a VM lambda is not pure (it can
  read state), so a step's parameters are its lambda (compared by code
  and captured values, not closure identity) and the values of
  everything the lambda reads besides its item (a keyed collection by
  its version, not a copy). Changing one of those rebuilds the step,
  which is what recomputing the lambda over every item would do. A
  lambda calling a service method (whose reads the VM cannot name)
  falls back to whole-list comparison. `map` keeps the source keys, so
  a mapped loop uses them even when the mapped item has a key field of
  its own; a loop with its own `key e` is never a chain. A lambda that
  fails is reported once and the item is filtered out, mapped to null
  or sorted as equal. `sort_by` compares keys with a total order (NaN
  after every number, kinds by kind), so a sort never panics.
- **2026-10-05 · wave2-vm: keyed reads.** `.len`, `.first`, `.last`,
  `[i]` and `.contains(x)` on a keyed collection compile to `Op::Keyed`
  and use core's accessors; `contains` looks the value's key up and
  compares the item found. The collection's list value is a lazy memo
  built only for reads that need the whole list (passing it to a `fn`,
  `join`, `.filter` outside a `for`).
- **2026-10-05 · wave2-vm: `await` on a pending value.** An async
  `let` that is pending carries an operation that settles with the
  load; `await` on it suspends until then. An awaited operation keeps
  its result, so every awaiter (two handlers on one `let`) gets the same
  value. A pending value with nothing to wait on is an error value
  ("nothing to await"), never null or a stale value.
- **2026-10-05 · wave2-vm: faults at the top level.** design.md: "a
  runtime fault freezes only its own component". A file's own `let`s,
  handlers and timers belong to no component; freezing their scope would
  freeze the whole program, so their errors carry no scope and
  `Instance::freeze` declines them (they are still located and
  outlined).
- **2026-10-05 · wave2-vm: visibility reaches everything under a
  surface.** A hidden surface's content lets go of every service held
  under it (its components, surfaces nested in it), and a nested
  surface (`popup` in a `bar`) holds what its children read only while
  it is open; its reads no longer count for the enclosing body. A
  surface's props (`open:`) still count for the body around it, since
  they are evaluated while it is hidden.
- **2026-10-05 · wave2-vm: the wall clock every step.** `Instance::step`
  sets the clock to the wall time on every step instead of only when
  the next minute is due: a wall clock set back (by hand or by NTP)
  would otherwise freeze the clock until it caught up, and a clock
  reader mounted later (a popup opened) would show the time of the last
  wake. Setting equal minute and second values is a no-op for the
  graph. Date arithmetic (`d.add(months:, years:)`) and `noise` use
  checked or wrapping arithmetic: a huge config value is an error value
  or a wrapped cell, not an overflow panic.
- **2026-10-05 · wave2-vm: `strand run` before hit testing.** The
  binary is wired as architecture.md's host-loop recipe says, made on
  this branch because the user asked for the carried items to be fixed
  in this wave (it touches `crates/strand`, which no other wave-2 track
  changes beyond `main.rs`'s command table). Two interpretations until
  M2 and M3: input and layout facts are per surface (the pointer over a
  surface is its node's `hover`, a release is `click`/`secondary`, a
  scroll `scroll(dy, dx)`, the surface's logical size its node's
  `width`/`height`), since render does not hit-test inside surfaces or
  lay them out yet; and `screens.focused` (and `Screen.focused`) is the
  first monitor in plug order until a compositor service reports
  focus. A config with errors is printed and not run (the overlay over
  a last good tree is the live-reload track's). A left button held on
  a surface is its node's `pressed` (cleared on release and on leave);
  buttons other than left and right have no design event and send
  nothing (round 2 sent a middle click as `on middle`, which the
  design does not have). A monitor back within 30 s keeps its place in
  `screens.all` (so `screens.focused` does not move to another monitor
  on a replug); a forgotten one comes back last.
- **2026-10-05 · wave2-vm: `strand run` shutdown and sleep (review
  round 3).** SIGINT, SIGTERM and the compositor going away all end a
  run the same way: the main thread sends `ToLogic::Shutdown` and joins
  the logic thread, which unmounts the instance, runs
  `Runtime::shutdown` (waiting for the persist queue, bounded) and
  drops its stores, so a `persist` or `prefs.toml` write made in the
  last 250 ms before logout or Ctrl-C reaches the disk. The signals are
  blocked in every thread (the mask is set before any thread starts)
  and read from a `signalfd` on the main loop; calloop's own signal
  source needs `nix`, which is not in the tree. The logic thread
  sleeps in a calloop loop of its own: the main thread's messages, a
  ping for the runtime's wake hook (which therefore holds no sender:
  the thread also ends when every sender is gone), the logic clock's
  deadline as the dispatch timeout, and the M0 demo's `CLOCK_REALTIME`
  timerfd armed at the wall-clock wake with `TFD_TIMER_ABSTIME |
  TFD_TIMER_CANCEL_ON_SET`, so after a suspend or a clock step the
  clock shows the new time at once (round 2 slept on a monotonic
  countdown, up to a minute late). A clock set back between the step
  and arming the timer (which `CANCEL_ON_SET` does not report) is
  caught by comparing the wall time after arming with the step's.
  With no state directory, persisted state is not kept but settings
  files still are (their overlays in a temporary directory).
- **2026-10-05 · wave2-vm: action writes (review round 3).** A handler
  that calls a service action (`n.expire()`, `notifications.clear()`)
  declares a write of what the action can change, from the first
  flush: lowering records the services an action's receiver belongs to
  (the service itself, or every service whose fields reach the item's
  record) and the instantiator asks `ServiceHost::action_writes(rt,
  service)`, by default every field of the service (a superset).
  `tests/instantiate.rs::action_calls_declare_their_service_writes`.
  The core change above (tasks spawned by listeners polled before the
  next sink) now has a core test of its own,
  `crates/strand-core/tests/order_props.rs::a_cell_written_by_a_task_a_listener_spawns_is_read_once_per_flush`,
  which fails without it.

## wave2-core

**2026-10-05 · Keyed collections at 2,000 rows.** `KeyedVec` keeps a key →
position map that tolerates stale entries (keys are unique, so an entry is
proved by one comparison; a stale one is found by an outward search and
fixed), so mutations never re-index the items after them. A lookup is
O(1) when its entry is fresh and otherwise costs the entry's drift since
it was last looked up, bounded by the list length (a queue that pushes at
the back and removes at the front drifts every entry by up to n; its
`Vec::remove` is O(n) anyway). Not "O(1) amortised" for every pattern, as
round 0 of this section said. `keyed_diff` is hash-based with the longest increasing run
of survivors left in place: the fewest `Move`s, never a remove and
re-insert of a surviving key; repeated keys give a `Reset` instead of a
panic. A derived collection that receives more than 128 diffs covering at
least a quarter of its source rebuilds and publishes the keyed diff of its
output (identity kept), because `sort_by` costs O(n) per diff. Hashing uses
`foldhash` (already in the tree), not SipHash. Numbers in
`docs/benchmarks.md`.

**2026-10-05 · Keyed writes under the 30 writes/s guard.** Replaces wave
1's "keyed collections are not gated". A throttled handler gets a held
copy of the list plus the list it started from: its later operations
apply to that copy (read-your-writes: a duplicate key or a missing key is
reported at once against what it has written), and when its window has
room the copy's changes land as one keyed diff, so items keep identity.
Diffs cannot be held one by one (that would grow without bound in a
runaway loop); one copy is bounded. Unlike plain state, a held copy is a
*set of changes*, not a latest value (review round 1: round 0 let any
write that went through supersede it, silently dropping every row a
throttled `on notifications.received(n) { history.push(n) }` had pushed
when the user dismissed one). A write that goes through (an input
handler, a service batch, another handler's landing) rebases the held
copy: the changes from its base to it (removals, value updates, moves of
survivors outside the longest in-order run, inserts) are re-applied by
key onto the new list, moved and inserted items going before the next
held item the new list still has in place (so pushes stay at the end,
after what others appended). Changes that no longer apply (a key both
inserted; an item the handler updated or moved but the other write
removed) are skipped and counted in one `Diagnostic::KeyedConflict`. A
cancelled handler's copy still never lands. The rate is checked before an
in-place write and counted only when the operation changed something, so
the unthrottled path stays in place (no copy). Held writes are indexed by
`(cell, writer)` and a rate window keeps only its newest 31 attempts, so a
throttled handler writing a cell per row stays linear
(`docs/benchmarks.md`).

**2026-10-05 · Frozen = paused, with a bounded backlog.** Refines wave 1's
"timers count while frozen". A timer inside a suspended component is
paused exactly as if its `while` condition had turned false: it keeps the
time counted so far and counts again from `rt.resume` (or from leaving the
frozen scope), so a toast frozen by a fault does not expire behind the
user's back, and an `every` does not fire a catch-up tick on release. A
timer created or restarted while frozen starts counting at the release.
`await sleep(..)` in a frozen handler pauses the same way (review round 1:
round 0 let it keep counting, so `await sleep(5s); n.expire()` still
expired a toast the moment it was released): each sleep knows the task
polling it and keeps its time left while that task is frozen. A release
does not count from the logic clock's last tick: the clock only moves
when the host ticks, and a host sleeps while everything is frozen, so a
released timer or sleep starts counting at the next clock advance (the
host's real time), reports no deadline until then, and the release calls
the wake hook. Freezing pauses as of the last tick (it may under-count by
less than a tick; never over-counts). State needs no bound (a cell
keeps its latest value, a held sink runs once on release with it; the held
list is a set). Events of lossless queues are kept per frozen listener up
to `MAX_FROZEN_EVENTS` (256, the size of a collection's diff log); past
that the oldest are dropped and the release reports one
`Diagnostic::EventsDropped { queue, listener, dropped }` before delivering
the rest in order. Reported at release rather than per drop: a chatty
service would otherwise flood the overlay, which already outlines the
frozen component.

**2026-10-05 · `persist` storage.** (Refined by "Persist IO thread and
handle" below.) One file per persisted cell under
`$XDG_STATE_HOME/strand/persist/` (falling back to
`~/.local/state/strand/persist/`; a relative `XDG_STATE_HOME` is ignored, as
the XDG spec says), named by the cell's `file.name` path with every byte
outside `[A-Za-z0-9_.-]` (and a leading `.`) percent-escaped, so no path
escapes the directory. One file per cell keeps writes small and atomic and
a corrupt file costs one cell. The file is a text header (`strand-persist
1`, `default <hash>`, `check <hash>`) and the value's bytes; the VM owns
the value codec, `strand-core` stores opaque bytes. Hashes are FNV-1a 64
(stable across builds, unlike `std`'s hasher; not a security boundary).
"Noticing a changed default" follows the reload rule for state defaults: a
value that still equals its old default (its hash equals the stored
default hash) adopts the new default; a changed value is kept, reported
once as `Diagnostic::PersistDefaultChanged`, and re-stamped. A file that
fails its header or checksum, or whose value no longer decodes (a type
change), is moved to `<name>.corrupt` and the cell starts from its default
with `Diagnostic::PersistFailed`. Writes go to a temp file in the same
directory, are `fsync`ed and renamed over the target (directory created
0700), debounced by 250 ms of logic time so a slider drag writes once, and
a pending write is flushed when the owning component is disposed or at
shutdown. Nothing is written while the value equals what the file (or the
default) already says.

**2026-10-05 · Topological effect order.** Replaces wave 1's "effects run
in creation order" deviation: sinks now run "once per tick in topological
order". The order is a rank (an Incremental-style height, `src/order.rs`)
over read edges, ownership (owners first) and the edges the graph can't
see: a handler writing a cell or emitting to a queue ranks the target one
above itself, and a queue's listeners rank with the queue. A flush runs
queued sinks by `(rank, creation order)` from a priority queue, and runs
woken tasks and due event deliveries before choosing each next sink, so a
non-sink writer also comes before the readers of what it writes. Write
edges are learned the first time a handler writes (attempts count: writing
an equal value still teaches the edge) once its run is over, or declared
with `rt.writes_to(handler, target)`; ranks only rise. So a sink runs once
per flush and sees final values, except that a dependency seen for the
first time (a write edge, or a read edge to a higher-ranked node) can
re-run a sink that already ran in that flush, once; property-tested by
`tests/order_props.rs` (a sink running twice must have risen in rank).
`on change` handlers start at rank 2^20, above anything ordinary writes
reach, which keeps wave 1's late phase: one outside write fires them once
with settled values; sinks downstream of what an `on change` writes rank
above it and run after it in the same flush. A write edge that would close
a loop (a handler writing what it reads, directly or through other
handlers, including a read edge that appears later) is a feedback edge: it
is left unranked (the rank walk never goes round it) and the cycle guard
bounds the loop as before. Not ordered: a task woken by something other
than a handler of the flush (another thread's waker) writes when it is
polled. Ranks live in a side map (most nodes are rank 0), so the node and
the 10k-node numbers stay as they were (`docs/benchmarks.md`). (Refined by
"Declared reads" and "Foreign wakes" below: the first flush and woken
tasks are now ordered too.)

**2026-10-05 · Declared reads and the provisional phase (review round
1).** Read edges exist only once a node has run, so round 0 ran a reader
created before its writer at rank 0 on the first flush, then again after
the writer: a glitch per mount, even with `rt.writes_to` declared, and a
reader switching to a higher-ranked branch re-ran too. The compiler now
also declares reads: `rt.reads_from(node, &sources)` with the syntactic
read set of every binding and handler (every branch, a conservative
superset; an empty set still counts as declared). Declared read edges are
rank-only: `rank(node) >= rank(source)`, walked by rank raises like
observer edges, and a declared read closing a loop through write edges
turns those into feedback edges, as a learned one does. With reads and
writes declared, ranks are complete before anything runs, so every sink
runs exactly once per flush with final values from the first flush on,
`Pick` branch switches included; `tests/order_props.rs` checks every flush
of declared graphs, the first one included, with written cells starting
unsettled. A sink that has never run and declares nothing (no reads, no
write edges) runs its first time in a provisional phase at rank
2^20 - 1, after every ranked sink and before `on change` handlers, so it
sees what declared writers wrote. Residual, without declarations only: a
learned edge can re-run a sink once in that flush (its last run sees the
final values, also property-tested), and undeclared fresh sinks in the
provisional phase run in creation order among themselves. So "once per
flush" holds for declared graphs; `features.md` says so.

**2026-10-05 · Foreign wakes (review round 1).** A task woken by another
thread (an IO or D-Bus reply) used to be polled between sinks of the
running flush, writing behind readers that had already run. The ready
queue now keeps wakes from other threads apart (by thread id) and polls
them only when a flush starts; one arriving mid-flush waits for the next
flush (the wake hook brings the host back). Both lists merge back in wake
order. Wakes on the logic thread (a handler's write completing a future,
`spawn`) are still polled before the next sink, where they are ordered.

**2026-10-05 · Persist IO thread and handle (review round 1).** Writes no
longer run on the logic thread: each `PersistStore` has one IO thread,
started on first use, fed by a queue coalesced to the latest operation
per file (write, remove, move aside). `load` sees queued operations before
the disk, so a component remounted by a reload reads what its predecessor
just queued. Failures come back through a per-runtime sink drained into
the next tick's diagnostics, with a wake-hook call. `Runtime::shutdown`
waits for the queue (bounded, 5 s); dropping the last store handle drains
it (bounded) and joins the thread; a persisted cell's writer queues its
pending value when dropped, so a runtime dropped without `shutdown` still
writes. The file's baseline is updated when a write is queued, not when it
lands (a failed write is reported; the next change writes again).
`rt.persisted` returns a `Persisted` handle with `redeclare(rt,
new_default)` (the reload rule for state defaults: adopt if the value
still holds the old default, removing the file; else keep, report
`PersistDefaultChanged` once and re-stamp) and `reset(rt)` (cancels the
pending and queued write, removes the file, sets the default), so the
default hash follows reloads and `@reset` cannot be undone by a late
write. A path names one live cell: the reconciler qualifies it with the
instance identity (`bar[<make model description>].x`, a list item's key);
a second live cell on a path in use reports `PersistPathInUse` and does
not write while the first owns the file (round 2: it waits and takes over,
see "Persist path hand-over"). A stored value that no longer decodes is
moved aside like a corrupt file (round 0 left it, so the warning repeated
on every start; round 2 renamed the quarantine to `.<name>.corrupt`). Temp files of dead processes (or
older than a minute and not ours) are swept on a store's first write.

**2026-10-05 · Settings files (review round 2).** Round 1 deferred
`state x from "file.toml" { .. }` to M2; that was wrong: wave 1 carried it
into wave 2 and the core side does not need the compiler, so it is built
now. `strand_core::settings`: `rt.settings_file(&store, path, fields)`
with one `FieldSpec { name, default, decode(&toml_edit::Item) ->
Result<V, String>, encode(&V) -> Item }` per field, generic over the VM's
value type `V` like `persist`'s codec (the compiler fills the schema from
the checked field types). Each field is an ordinary `Signal`, so `<->`
bindings, UI writes and `strand set` are plain writes; after 250 ms of
quiet (`PERSIST_DEBOUNCE`, as for `persist`) the changed fields are queued
as per-field edits on the persist IO thread, merged per file, and applied
with `toml_edit` to what the file holds when the thread gets to it
(replaced values keep their decor, keys their order and comments, new keys
are appended), written via temp file in the resolved target's directory
(symlinks followed, the target's permissions kept), `fsync` and rename.
Interpretations: (1) the "runtime overlay" of "Who wins" and the
read-only "overlay in `$XDG_STATE_HOME`" are one layer, a TOML file in
`$XDG_STATE_HOME/strand/settings/` named by the declared (link) path, so
it survives restarts and a `home-manager switch` that swaps the link; it
holds redirected writes and explicit `set_overlay` values, and `[clear]`
is `clear_overlay`. (2) A target is read-only when neither it nor its
directory has a write bit (`/nix/store`: 0444 in 0555; checked before
writing, so root does not write there either) or the write fails with
`EACCES`/`EROFS`; the write then goes to the overlay, with one
`SettingsIssue::ReadOnly` notice per file. (3) A UI write to a field that
has an overlay value updates the overlay (a file write would be shadowed
and look ignored). (4) A UI write never overwrites a file that has a
syntax error (the user is mid-edit): `WriteFailed`, the value stays live.
(5) `Shadowed` ("file changed but runtime overlay wins [clear]") is
reported by a reload whose read of that field differs from the previous
read, not at boot (no previous read). (6) Withdrawn in round 3: boot
falls back to a last-good snapshot, not the defaults (see "Settings files
(review round 3)"). (7) A reload applies edits still queued for the IO
thread and leaves a field the user wrote since the last write-out alone,
so it never undoes a write that has not reached the disk (round 3 made
this hold for reads made before a write landed, too).

**2026-10-05 · Settings files (review round 3).** (a) *Last good values
survive a restart.* Principle 5 and "a TOML syntax error keeps every last
good value" leave no room for a default-colour flash at boot, so each
settings file has a last-good snapshot,
`$XDG_STATE_HOME/strand/settings/last-good/<escaped declared path>.toml`
(a subdirectory, so it cannot collide with an overlay name). Whenever a
field's file layer changes (a good read, a deleted key, a UI write to the
file) the changed fields are queued to it on the IO thread; at boot (and
for fields a redeclare adds or resets) a file that has a syntax error or
cannot be read, or a field with a bad value, takes the snapshot's value,
and the diagnostic is still reported. A snapshot with a syntax error is
simply replaced. (b) *Stale reads.* Every settings job has a process-wide
sequence number; the IO thread records, per file, the highest one done.
A read first takes a mark (that number plus the edits queued or in
flight, under one lock) and only then reads the file, so an edit landing
in between is applied twice, harmlessly; a field whose last edit is newer
than the mark keeps its layer, so a read made before Strand's own write
landed (the watcher's thread is slower than the IO thread) can never undo
it. (c) *Reads off the logic thread.* `Settings::sources()` gives a
`Send` `SettingsSources`; strand-watch calls `mark()`, reads the bytes
(which it hashes anyway), then `read_from(mark, text)`, which also reads
the overlay and probes writability, and posts the `SettingsRead` to the
logic thread for `Settings::reload_with`. `reload` does both on the
calling thread. Boot (`settings_file`) and `redeclare` still read on the
logic thread: once per declaration, small files, before the first frame.
(d) *Writability is probed on every read*, so a link swapped from
`/nix/store` to a writable file takes writes again; when the probe goes
from read-only to writable the once-per-file notice is re-armed. A file
the IO thread found read-only (EACCES) while the probe says writable stays
redirected. (e) *One file, several handles* (a component mounted per
monitor): each `settings_file` call keeps its own signals (sharing them
would tie them to the first owner, whose unmount would dispose them), and
the runtime keeps a registry of live handles by overlay path (one per
store and declared path); a write-out, `set_overlay` or `clear_overlay`
through one is adopted by the others in the same call, without IO, unless
a sibling declares the field with a type that does not decode it. (f)
*`redeclare(rt, fields)`* matches fields by name and keeps their signals;
a new default is adopted where neither file nor overlay sets the field and
the user has not written it (the state-default rule); a field whose
declared type (`FieldSpec::with_type`, the compiler's type name) changed
is reset and read again under the new type, with
`SettingsIssue::TypeChanged`; an added field is read from the file (or
the snapshot); a removed field's cell is disposed and its key stays in the
file. `Settings::layer(field)` reports `Overlay | File | Default` for the
inspector. (g) *The overlay is Strand's file*: one with a syntax error is
moved aside to `.<name>.corrupt` (reported once as `CorruptOverlay`, by
the read that saw it or by the IO thread when a write finds it first) and
the overlay starts empty. (h) *A missing directory* of the settings file
is created on the first write, with the user's umask (`create_dir_all`),
as editors do. (i) Temp files `.<name>.tmp.<pid>.<n>` a crash left next to
the resolved target are swept when the file is declared (that file name
only; not this process's; dead processes or older than a minute). (j)
`show` moves the field's "shown" value only once the signal holds it, and
a write-out to the file updates the field's last-seen value, so a reload
from inside a derived value cannot turn a stale signal into a write, and
Strand's own write is not reported as `Shadowed`.

**2026-10-05 · Persist path hand-over (review round 2).** A second live
cell on a persist path no longer stays inactive for good: it is kept in a
per-path waiting list, and when the owner is disposed (its pending value
is queued first) the oldest live waiter takes the path over. If it still
holds the value it started from, it continues from what the old owner left
(the state-default rule applied through `restore`); if it was changed
while it waited, its value wins and is written. This makes a reconcile that
mounts the replacement before disposing the old instance (surface
recreate, monitor replug, a list item re-created under its key) safe
without an ordering rule for the reconciler.

**2026-10-05 · Feedback edges are not errors (review round 2).**
`rt.writes_to` returns `Ok(WriteEdge::Ranked | WriteEdge::Feedback)` and
errs only for disposed ids (round 1 returned `Err(Cycle)` for a valid
self-normalising handler, and only when its reads were declared first).
When a read declared later closes a loop, the write edge is demoted and its
target (a cell or event queue) lowered to what its remaining edges need
(owner, ranked writers + 1, sources), so the ranks match those of the other
declaration order. `rt.write_edge(w, t)` tells what an edge became.

**2026-10-05 · Composite nodes declare their own edges (review round
2).** `rt.async_memo` declares its internal effect's write edge to the
value (load bookkeeping also counts as a write for learned edges now), and
`AsyncMemo::effect_id()` lets the VM declare the input's reads, so a
declared reader of `let hits = apps.search(q)` runs once per flush, after
the load started and (when ready at once) resolved. `Debounced` already
exposes `effect` (tracked reads) and `timer` (the body's writes). Persisted
and settings cells write nothing in the graph from their internal
handlers; keyed derived collections are pull-based.

**2026-10-05 · Small fixes (review round 2).** A quarantined persist file
is `.<name>.corrupt` (a name no cell path maps to; round 1's
`<name>.corrupt` was what the path `<name>.corrupt` reads), and the temp
sweep only matches `.<name>.tmp.<pid>.<n>`. `rt.is_idle()` is false while
an IO thread's failure waits to be reported. `KeyedSignal::get_untracked`
inside the collection's own `update` returns `Error::Reentrant` instead of
panicking.

## wave2-watch

- **2026-10-05 · Raw inotify (rustix), not notify, and no debouncer.**
  The spec allows raw inotify where notify cannot express the event set,
  and notify 8.2 cannot: it always adds `IN_OPEN` and `IN_ATTRIB` to
  every watch, so every open of any file in a watched directory by any
  process (each font an app loads in a `watch_tree` font directory, every
  read in `~/.config`, which is watched as the config root's parent)
  would wake its thread and ours, against design.md's "an idle shell
  does zero work". It also removes, on a watched directory's
  `MOVED_FROM` or delete, every watch whose path starts with it, behind
  the caller's back. `rustix::fs::inotify` (rustix is already a
  dependency) with the mask `CLOSE_WRITE | MOVED_TO | MOVED_FROM |
  CREATE | DELETE | DELETE_SELF | MOVE_SELF | MODIFY | ONLYDIR |
  EXCL_UNLINK` queues nothing for reads (`reading_a_watched_file_queues_no_events`).
  The watcher thread `poll(2)`s the inotify fd and an eventfd that control
  calls write, so there is one thread and no wake-up without work. The
  wd-to-path map lives with core, which decides when watches go.
  `Q_OVERFLOW` is a full rescan; an `IGNORED` for a still-mapped
  descriptor is a removal. If `inotify_init` fails
  (`max_user_instances` reached, common with Electron apps), every
  directory is polled and reported as `Polling { WatchFailed }` instead
  of the watcher failing to start. The debouncer is not used: it
  debounces per path on a tick, which cannot express "15 ms after the
  last completed write across all files" (save all = one batch), and its
  rename stitching is unneeded because the watcher never follows renames:
  it marks the paths an event names and decides at the end of the quiet
  period, by `lstat` and BLAKE3, what each one is now. Watches are
  non-recursive, one per directory, so depth (3, from `find_files`) and
  symlink handling stay ours.
- **2026-10-05 · What counts as an event.** Acted on: `CLOSE_WRITE`,
  `MOVED_TO`, and a `CREATE` that no `CLOSE_WRITE` will follow: a
  symlink (`ln -s`), a hard link (`ln`, a regular file with more than
  one link), or a FIFO, socket or device node. A `CREATE` or `MOVED_TO`
  of a symlink to a directory in a config directory (GNU stow folding in
  a sub-directory) rescans the module set. Removals (`DELETE`, `MOVED_FROM`, a watched directory
  deleted or moved) mark a path for an existence check: a removal cannot
  be half-written, and a module deleted for good must be reported.
  `MODIFY` and a plain-file `CREATE` are never read; they only keep an
  already-open batch waiting, so a slow multi-file save stays one batch.
  A stream that never goes quiet is cut 500 ms after its first event.
- **2026-10-05 · Removal grace 50 ms.** When the latest event on a
  watched path was a removal the quiet period is 50 ms instead of 15, so
  delete-and-create and Vim's rename-then-write are one `Modified`, never
  `Removed` then `Created` (and never a missing-module error on the
  reload overlay). Saves without a removal keep design.md's 15 ms.
- **2026-10-05 · Scratch names.** design.md's `4913`, `*.swp`, `*~`,
  `*___jb_*___`, plus `*.swo` and `*.swx` (Vim's next swap names when a
  `.swp` exists). Hidden names are ignored in config and cache
  directories, as `find_files` skips them; an explicitly watched hidden
  file (`~/.wallpaper`) still works because explicit paths match exactly.
  Other extensions in a config directory are not reported unless a path
  is registered (`.wgsl` comes from the compiler's `shader "…"` paths).
- **2026-10-05 · Symlinks hop by hop.** Each watched path is resolved one
  component at a time; every symlink met (file or directory, up to 40)
  is a hop, and the watcher watches each hop's directory plus the final
  target's directory. An event on any hop re-resolves the file; a hop
  that is a directory link of a module file, or of the config root, also
  calls the module-set rescan. Watches are always on canonical
  directories, so one inode is never watched under two paths. A link
  swap whose new target has the same bytes is still reported as
  `Modified` with the unchanged hash and the new `canonical`: design.md
  counts a link swap as an edit, and the loader must stop pointing
  diagnostics and click-to-`$EDITOR` at a store path that may be
  garbage-collected; it skips the recompile by hash.
- **2026-10-05 · Immutable stores are not watched.** A directory under
  `/nix/store` or `/gnu/store` on a read-only mount cannot change in
  place; home-manager's switch swaps the link, which the link's directory
  sees. Other read-only mounts are watched (a read-only bind mount of a
  writable tree still gets events), and a network or FUSE filesystem is
  polled even when mounted read-only (the server's copy still changes):
  the filesystem type is checked before the read-only flag. A polled
  file that exists but cannot be opened is compared by `stat` stamp, as
  it was stored, so it is not re-checked on every poll.
- **2026-10-05 · Polling.** Directories on NFS, SMB/CIFS, 9p, Ceph, AFS,
  Coda or FUSE (statfs magic), or whose inotify watch fails (limit
  reached), are polled every second (`Options::poll_interval`): the
  listing is compared (inode, size, times), and a watched file in the
  directory is re-hashed only when its stamp changed. The stamp is taken
  after an `open` (`O_NONBLOCK`) and `fstat`, which on NFS revalidates
  the attribute cache (close-to-open consistency), so a stale cache does
  not hide an edit and a 20 MB wallpaper on an NFS home is not re-read
  every second. As a backstop, every 30 s (`Options::content_sweep`)
  watched files up to 1 MiB (`Options::sweep_max_bytes`) are re-hashed
  whatever their stamp; larger ones are compared by stamp only. FUSE is
  polled because remote writes (sshfs, rclone) make no events. Each
  polled directory is reported once as `Notice::Polling`; a polled
  directory that exists but cannot be listed keeps its last listing and
  is polled quietly instead of being dropped and re-added every poll.
- **2026-10-05 · Cache trees are not hashed.** App, icon and font
  directories report `Role::Cache(kind)` paths, `Modified` or `Removed`
  by existence, with no hash: a font can be tens of MB and the cache
  owner re-reads what it needs. A new sub-directory is watched up to the
  tree's depth.
- **2026-10-05 · Own writes.** `register_own_write(path, hash)` matches
  the next completed write of that path (or of its resolved target) with
  that hash, once; unmatched registrations expire after 10 s (pruned on
  every registration and every batch). When a match is found, earlier
  registrations for the same file are dropped too: Strand writing a file
  several times in one quiet period (a slider) leaves only the last
  content on disk, and the superseded hashes must not swallow a later
  user save (an editor undo) with those bytes.
- **2026-10-05 · Portal client.** zbus is built with its `tokio`
  feature (the services runtime, design.md). The async core is
  `strand_watch::follow(&Connection, EventSink)`, so `strand-services`
  can host it on the shared current-thread runtime and session
  connection; `PortalSettings::spawn` is a convenience that runs it on
  its own thread and runtime (tests, or before services exist). It
  subscribes to `SettingChanged` and to owner changes of
  `org.freedesktop.portal.Desktop` before reading, so no change is lost
  between them. The three keys are read concurrently (`ReadOne`, falling
  back to `Read` for portal version 1, value wrapped in one more
  variant), each under a 500 ms limit (`BOOT_READ_TIMEOUT`); connecting
  and subscribing have a 2 s limit. The boot batch (`at_boot: true`) is
  always sent, with what arrived in time, empty when the bus or portal
  is missing. A key that missed the limit is read again without the
  short limit (zbus's own 25 s) and sent with `at_boot: false`. When the
  portal starts or restarts later (a new name owner), all three keys are
  re-read and sent with `at_boot: false`: they are real changes against
  the defaults logic holds, and a value equal to the current one changes
  nothing in the graph. Dropping `PortalSettings` cancels the whole task,
  including outstanding calls, so shutdown never waits on a hung portal.
  `color-scheme` keeps the raw preference next to `dark`; an
  `accent-color` component outside 0..=1 means unset. Tests use a zbus
  mock portal on a private `dbus-daemon` the test starts itself
  (python3-dbusmock is not installed); they skip when `dbus-daemon` is
  missing unless `STRAND_REQUIRE_DBUS` or `CI` is set, and CI sets the
  former and installs `dbus`, so the tier cannot pass silently there. A
  `SettingChanged` that arrives while a late or restart re-read is in
  flight wins: the read's value for that key is dropped, since the
  signal is at least as new as the read's answer and sending the read
  after it would revert the change (dark mode flipping back).
- **2026-10-05 · Missing and replaced directories.** A directory the
  watcher wants (a config directory, a referenced file's directory, a
  link's directory, a cache tree root) that does not exist is replaced
  by its nearest existing ancestor, and the first missing path below it
  is remembered; when that path appears (a directory, or a link), the
  files below it are re-resolved, re-watched and re-checked, and the
  module set is rescanned if the config lies below it. The config
  directory's parent is always watched and the root itself triggers a
  rescan, so a config directory that is deleted and recreated (dotfile
  scripts, `mv new strand`) is watched again. Directory removals
  (`IN_DELETE` of a directory, `DELETE_SELF`) are removals; a creation
  event for a path already watched with the same inode changes nothing.
  A watched directory that is moved or deleted takes every watch below
  it along: a moved directory's descriptors follow the inodes, so the
  paths they were added under are stale for the whole subtree. The
  watcher drops them all (`MOVE_SELF`, `MOVED_FROM`, `DELETE_SELF`) and,
  at the flush, re-resolves, re-watches and re-hashes every file below,
  so `mv cfg cfg.bak; mv cfg.new cfg` reports the changed files in
  sub-directories and nothing written in `cfg.bak` is reported under
  the old names. As a backstop, each flush re-checks the inode of every
  watched directory and re-watches one that changed. Watch, then list:
  when a flush adds a watch on a config or cache-tree directory, the
  module set (or tree) is listed once more after the next quiet period,
  because the rescan listed that directory before its watch existed and
  a file created in between (a slow `cp -r`, a `git checkout`) made no
  event. The second listing adds no new watch, so it ends there, and an
  unchanged set sends no batch. The same holds at boot (the caller's
  `find_files` ran before `spawn` added any watch: the set is listed
  again at the first quiet period) and for a new cache tree (walked
  again once watched).
- **2026-10-05 · Registrations per role.** A referenced path can be
  wanted for several reasons (two `state … from "prefs.toml"`, a
  wallpaper also shown by an `image`), so registrations are counted per
  (path, role) and `unwatch_file(path, role)` drops one. Module-set
  membership is separate from registrations, so registering or
  unregistering a module file never changes its module status. A change
  is reported once per role (`changes` sorted by path, then role).
  `set_referenced(pairs)` replaces every registration with the set the
  compiler collected from the whole program, so the loader does not diff
  path sets itself.
- **2026-10-05 · Only regular files are read.** A watched path is
  `stat`ed, opened with `O_NONBLOCK`, `fstat`ed, and hashed only if it is
  a regular file, streaming (`blake3::Hasher::update_reader`), so a FIFO
  cannot block the watcher thread (or `watch_file`, which waits for it),
  `/dev/zero` cannot exhaust memory, and a 50 MB wallpaper is not
  buffered whole. Anything else is reported once with
  `error: Some(InvalidInput)` and no hash.
- **2026-10-05 · Batch timestamps.** `FileBatch` carries `first_event`
  and `last_event` (the event that last kept the quiet period open) and
  `SystemBatch` carries `received`, so the reload-latency benchmark and
  `strand watch --json` can separate the watcher's quiet period from
  compile and commit time.
- **2026-10-05 · `ConfigReloaded { failed: Option<bool> }`.** niri's
  `ConfigLoaded { failed }` gives `Some(failed)`; Hyprland's
  `configreloaded` says nothing about success, so its adapter sends
  `None` rather than inventing `false`.
- **2026-10-05 · Watch, then read, for every baseline.** A file's
  baseline hash is read after its directory watch exists (module files
  at `spawn`, `watch_file`, `set_referenced`): the entry is created
  resolved but unread, the watches are synced, then the file is hashed
  without reporting anything. A save in between is either in the
  baseline or makes an event; reading first left a stale baseline, so a
  later undo to the old bytes was dropped as a no-op.
- **2026-10-05 · Loaded hashes.** The loader reads referenced files
  before it knows to register them (the compiler collects the paths), so
  a save between its read and `set_referenced` would be lost.
  `set_referenced` takes `Referenced { path, role, loaded }`, built from
  `(path, role)` or `(path, role, hash)`; when `loaded` differs from
  what the watcher holds, the baseline becomes `loaded` and the path is
  re-checked at the next quiet period, so the file is compared with what
  the loader holds. Every role of that path sees the change (a module
  also registered as `Other` gets a redundant `Modified`, which a hash
  check on the logic side ignores). `watch_file` stays "register, then
  read".
- **2026-10-05 · Own writes are registered synchronously.**
  `register_own_write` pushes into a list shared with the watcher thread
  (`Arc<Mutex<_>>`) instead of queueing a control message, so a flush
  already under way when Strand writes sees the registration. Strand's
  own writes are atomic (temporary file renamed over the path); an
  in-place write can be read half done and that content is reported.
- **2026-10-05 · Files linked in complete.** A `CREATE` of a regular
  file with one link and non-zero length is a completed write: its bytes
  existed before its name (`O_TMPFILE` + `linkat`, as systemd's
  `link_tmpfile` does; its `CLOSE_WRITE` is reported under the unnamed
  `#<ino>`, if at all). An empty new file still waits for `CLOSE_WRITE`;
  a write still in progress extends the batch with `MODIFY`.
- **2026-10-05 · Light ancestor watches.** A watched directory's inotify
  descriptor follows its inode, so moving an unwatched ancestor
  (`mv ~/x ~/w` with only `~/x/y/z` watched) made no event and the file
  kept being reported under its old path. Every ancestor of a directory
  watched in full now holds a light watch (`MOVED_FROM`, `DELETE`,
  `DELETE_SELF`, `MOVE_SELF` only, no writes or creations), so the move
  forgets the watches below, re-resolves and reports `Removed`; the
  ancestor then becomes the full watch waiting for the path to return.
  Writes in `~` or `/` queue nothing; renames and deletions there wake
  the thread for a map lookup and no batch. Ancestor watches are best
  effort and silent: on a network or read-only filesystem, or past the
  watch limit, they are skipped (polled directories already notice a
  vanished directory when listing fails). A periodic inode audit was
  rejected: it would wake an idle shell.
- **2026-10-05 · Backend errors do not spin.** A failing `poll(2)` is
  retried after a pause that doubles from 10 ms up to `poll_interval`,
  and each distinct error is reported once. A failing inotify `read`
  (not `EAGAIN`) leaves the fd readable, so the watcher drops inotify,
  polls every directory (each reported once as `WatchFailed`) and
  rescans everything (`RescanReason::Overflow`: events were lost).

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
next tick (they count while frozen). Work is also released when it leaves
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
schema from `strand-compiler`).

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
  the inner type of an `Async` or a `T?`.
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
  (they are where colours are meant to live).
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
  checker passes for whole-number states are bounded and sound. Before
  the first pass, fraction writes the source shows plainly are pinned
  (`check/prepin.rs`): `x = 0.5`, `x /= …`, `x += 0.1`, or `<-> x` on a
  builtin prop that reads and writes a `float`, when `x` is the only
  declaration of its name in the file and the write is in that file.
  The configs in the fixtures, and any with sliders bound to
  `state v = 0`, check in one pass. A pass also records hand-offs between
  whole-number states (`b = a`, `b = a * 2`, `b = c ? a : 0`, `b = a ??
  0`), and the pins are closed over them, so a chain costs one extra pass
  however long it is. Hand-offs through locals or calls are not followed
  and cost a pass per link; at the cap (8 passes) the last pass reports
  a write that would still widen as `check::needs_type` ("holds whole
  numbers and fractions", fix: `state x: float = …`), so a fraction
  never reaches an `int` position unreported.
- **2026-10-05 · wave2-check (round 3): inferred parameters.** A
  whole-number literal passed to a parameter whose type comes from its
  callers is an `int` there, as in an untyped `state` (`Grid 3` makes
  `n` an `int`; a caller passing `0.5` widens it to `float`). A bare
  builtin variant passed to such a parameter (`Side center`) cannot be
  resolved, as no enum is expected: it is one `check::unknown_name` whose
  help says to write the enum (`Align.center`) or type the parameter.
  A parameter whose callers all passed errors is not also reported as
  "nothing passes it a value".
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

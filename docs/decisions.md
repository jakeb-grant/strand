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
  CREATE | DELETE | DELETE_SELF | MOVE_SELF | ONLYDIR | EXCL_UNLINK`
  (plus `MODIFY` in config directories only; see "`MODIFY` only in config
  directories") queues nothing for reads (`reading_a_watched_file_queues_no_events`).
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
  `MODIFY` and an empty plain-file `CREATE` mark a write in progress on
  that path: the file is not read until its `CLOSE_WRITE` or `MOVED_TO`
  (or its removal), however long the writer pauses. They do not move an
  open batch's quiet period (revised in review round 5: see "The quiet
  period runs from completed writes"). A stream of completed writes that
  never goes quiet is cut 500 ms after its first event; a file still
  being written at that cut is held, not read (see "Writes in progress
  are held").
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
  missing unless `STRAND_REQUIRE_DBUS` is set. Making CI set it (and
  install `dbus`) is a change to `.github/workflows/ci.yml`, which this
  track does not own: it is requested from the integrator, and until it
  lands the tier may skip in CI. A
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
  the old names. As a backstop, a full rescan (overflow, `strand
  reload`) re-checks the inode of every watched directory and re-watches
  one that changed. Watch, then list:
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
  `set_referenced(pairs)` replaces the loader's registrations with the
  set the compiler collected from the whole program, so the loader does
  not diff path sets itself. It never touches `watch_file`
  registrations (see "Two registration sets").
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
  in-place write is held until it is closed, but a crash mid-write
  would leave a half file on disk.
- **2026-10-05 · Files linked in complete.** In a config directory (the
  only watches with `MODIFY`, review round 5), a `CREATE` of a regular
  file with one link and non-zero length is read like a completed write
  unless a `MODIFY` for it follows: its bytes may have existed before its
  name (`O_TMPFILE` + `linkat`, as systemd's `link_tmpfile` does; its
  writes and `CLOSE_WRITE` are reported under the unnamed `#<ino>`, if
  at all). A plain `open(O_CREAT)` + `write` writer whose creation is
  read after its first write looks the same at `CREATE`, but the kernel
  queues its `MODIFY` for the name right behind, which marks the write
  in progress and holds the file until `CLOSE_WRITE`. An empty new file
  waits for `CLOSE_WRITE` from the start. In other content directories
  no `MODIFY` tells the two apart, so such a creation is held like a
  write in progress (see "`MODIFY` only in config directories").
- **2026-10-05 · Light ancestor watches.** A watched directory's inotify
  descriptor follows its inode, so moving an unwatched ancestor
  (`mv ~/x ~/w` with only `~/x/y/z` watched) made no event and the file
  kept being reported under its old path. Every ancestor of a directory
  watched in full now holds a light watch (`MOVED_FROM`, `DELETE`,
  `DELETE_SELF`, `MOVE_SELF` only, no writes or creations), so the move
  forgets the watches below, re-resolves and reports `Removed`; the
  ancestor then becomes the parent watch (names only) waiting for the
  path to return.
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
- **2026-10-05 · Writes in progress are held (review round 4).** The
  watcher keeps the paths with a write in progress (`MODIFY`, or an
  empty file created, and no `CLOSE_WRITE`, `MOVED_TO` or removal since).
  At a flush, a touched file whose resolved path is among them is not
  read: it is held, and looked at again at every later flush, so a
  delete-and-create or Vim save whose writer stalls mid-write (past the
  50 ms removal grace, past the 500 ms cut) is one batch with the full
  content. A held file adds no busy loop: the next deadline is its
  `CLOSE_WRITE` (an event) or the stall limit. A writer that neither
  writes nor closes for `Options::stalled_write` (5 s; a program keeping
  the file open) has the file read anyway, with
  `Notice::StalledWrite(path)`; a lost `CLOSE_WRITE` (overflow) ends the
  same way. Polled directories have no `MODIFY` and compare content as
  before.
- **2026-10-05 · Two registration sets (review round 4).** Each watched
  path counts the loader's registrations (`set_referenced`, replaced
  whole after each reload) and ad-hoc ones (`watch_file` /
  `unwatch_file`) separately, and is watched while either (or module
  membership) holds it. The wallpaper is a runtime settings value
  (`prefs.wallpaper`, changed by `strand set`), not a path the compiler
  collects, so its owner registers it with `watch_file`; replacing all
  registrations on every reload would silently drop it (design.md:
  "Both the wallpaper path and its symlink target are watched").
- **2026-10-05 · Parent watches (review round 4).** The config root's
  parent (`~/.config`), the directories holding a symlink on the root's
  or a cache tree's way, and the nearest existing ancestor that stands in
  for a missing directory are watched for names only: `CREATE`,
  `MOVED_TO`, `MOVED_FROM`, `DELETE`, `DELETE_SELF`, `MOVE_SELF`. Every
  app writing its own file in `~/.config` would otherwise wake the
  watcher on each `write(2)` (`MODIFY`) and close, which the move to raw
  inotify was meant to stop. A directory wanted for several reasons gets
  the strongest kind (full, then completed, then parent, then ancestor).
  The directories of a referenced file's symlinks stay content watches
  (without `MODIFY` since review round 5): a save that replaces the link
  with a plain file (delete and create) writes there.
- **2026-10-05 · Content-only batches leave the watches alone (review
  round 4).** A flush re-syncs the watches only when the structure may
  have changed: a full rescan, a module-set or cache-tree rescan, a
  watched directory gone or replaced, a missing directory appearing, or
  a touched file that now resolves elsewhere. A batch of content edits
  (the common save) costs the files it hashes, not the number of watched
  directories (a 3000-directory icon tree added ~23 ms per save before).
  A re-sync stats only directories not yet watched; the per-directory
  inode audit runs on full rescans only
  (`content_only_flushes_do_not_resync`).
- **2026-10-05 · Own writes across aliases (review round 4).** Within one
  flush, an own-write match is a fact about the resolved file: once one
  registered path matched `(canonical, hash)`, every other registered
  path that resolves to the same file with the same hash (a symlink
  alias, a `..` spelling, which `paths::absolute` does not normalise) is
  own too. `register_own_write` resolves the path before taking the lock
  the watcher thread takes per hashed file, so a slow `readlink` on NFS
  never stalls a flush.
- **2026-10-05 · Which thread may block (review round 4).**
  `watch_file`, `set_referenced` and `watch_tree` block until the
  watcher thread has synced the watches (and, for a new path given no
  hash, read its baseline: watch, then read). They are for the loader,
  the compile worker, boot and service threads, never the logic thread.
  A path given with the hash of what the caller read is not read at
  registration: its baseline is that hash and the watcher compares the
  file at the next quiet period on its own thread. For the logic thread,
  `watch_file_loaded(path, role, hash)` and `unwatch_file` return at
  once (a control message), and `register_own_write` takes only a short
  lock. `watch_tree` still walks the tree before it returns: deferring
  the walk would lose files created in a sub-directory between the
  return and its watch (the re-walk lists directories, not files), and
  its callers (boot, the apps/icons/fonts services) are not on a hot
  path.
- **2026-10-05 · `MODIFY` only in config directories (review round 5).**
  `IN_MODIFY` is queued once per `write(2)`, and the watcher drains its
  fd as soon as `poll` wakes, so the kernel's merging of identical
  events rarely applies: any process writing in a directory watched with
  it wakes the thread once per write. Only the config's module
  directories (Strand's own) keep it. Every other content directory (a
  referenced file's directory and each symlink hop's, so `~` for a
  home-manager `~/.background-image`, `~/Pictures`, `~/Downloads`; every
  app, icon and font tree, whose entries are never read) is watched for
  completed writes and names: `CLOSE_WRITE | MOVED_TO | CREATE` and the
  removal events. A download, `.xsession-errors` or a package upgrade
  there now costs one wakeup per file created or closed
  (`tests/wakeups.rs`: 4500 small writes next to watched files wake the
  thread a handful of times, against about 2600 with `MODIFY`;
  `writes_queue_events_only_in_config_directories`). Throttling the fd
  after `MODIFY`-only drains was rejected: one inotify fd serves the
  config directories too, so pausing it would delay config saves, and a
  continuous writer would still wake the thread every pause. What
  `MODIFY` did there is covered otherwise: an in-place write is not
  touched until its `CLOSE_WRITE`; an empty new file is held from its
  `CREATE`; a new single-link file that already has bytes when its
  `CREATE` is read (`Raw::Created`: a writer that wrote first, or a file
  linked in from `O_TMPFILE`) is held until its `CLOSE_WRITE`, as no
  `MODIFY` will say which it is. A cache-tree entry created that way is
  reported at once (it is never read). The remaining gap: a file in such
  a directory that is touched for another reason (a full rescan) while
  being rewritten in place can be read mid-write; its `CLOSE_WRITE`
  reports the final content in a later batch.
- **2026-10-05 · Stalls are measured by the file, not by events (review
  round 5).** Without `MODIFY`, an active writer makes no event between
  its creation and its close, so "no event for `stalled_write`" alone
  would read a 6 s download half done. A held file is read as stalled
  only if, in addition, its modification time is unchanged since the
  flush that held it; a changed one renews the hold for another
  `stalled_write`. One `stat` per held file per 5 s; a file linked in
  from `O_TMPFILE` into such a directory (its `CLOSE_WRITE` comes under
  `#<ino>`) is read 5 s after it stops changing, with
  `Notice::StalledWrite`
  (`a_new_file_without_modify_is_held_while_it_changes`).
- **2026-10-05 · The quiet period runs from completed writes (review
  round 5).** design.md: 15 ms after the last *completed* write. A
  `MODIFY` no longer moves an open batch's quiet period: a watched file
  that a process keeps open and appends to (a `service … from file`
  status file) stretched every unrelated batch to `max_delay`
  (`a_file_written_continuously_does_not_delay_a_save`,
  `busy_never_moves_the_quiet_period`). Holding a file mid-write at the
  flush already keeps half-written content out. A held file whose write
  ended (`CLOSE_WRITE`, `MOVED_TO`, removal) has no deadline of its own:
  that event put it back in the open batch, which waits for its coalesce
  or removal grace like any other, so a stalled save followed by "save
  all" is one batch and a held file deleted and created again is one
  `Modified` (`a_save_all_after_a_stalled_write_is_one_batch`,
  `a_held_file_deleted_and_created_again_is_one_modified`,
  `a_held_file_removed_waits_for_the_removal_grace`).
- **2026-10-05 · Events keep the mask they were queued under (review
  round 5).** A directory's watch kind can change while events queued
  under the old mask are unread (a wallpaper written into `~/.config`, a
  names-only parent watch, and then registered). Before a mask changes,
  the queued events are read and classified under the old kind, and
  handled before newer ones. A creation seen by a names-only watch is
  complete as far as that watch can tell (it queues no `CLOSE_WRITE`), so
  it is never held waiting for one
  (`events_keep_the_mask_they_were_queued_under`).
- **2026-10-05 · The IPC socket is not a watch source (review round 5).**
  design.md lists "CLI and IPC: Unix socket and D-Bus" among the change
  sources, and an early draft of the thread table gave the socket to the
  watcher thread. It belongs to the `strand` binary instead, served on
  the logic loop (calloop source), because every request it carries
  (`strand reload`, `strand watch`, and M5's `get | set | toggle | watch
  | call`) reads or writes live state or calls into the program, which
  only logic may touch; routing it through `strand-watch` would add a
  thread hop and a second request path for no gain. `strand reload`
  reaches the watcher through `Watcher::rescan()`. strand-watch therefore
  has no IPC `ChangeEvent` slot; its sources are inotify, polling, the
  portal Settings client and the compositor-event slot
  (`ChangeEvent::Compositor`, filled by M3 adapters).
- **2026-10-05 · The bus filters portal namespaces (review round 5).**
  `follow` subscribes with an `arg0='org.freedesktop.appearance'` match
  rule (`receive_setting_changed_with_args`), so the bus daemon drops
  `SettingChanged` for every other namespace (GNOME's backend emits one
  per exposed gsettings key) instead of routing it to the services
  runtime; the client-side namespace check stays as a backstop
  (`other_namespaces_are_dropped_by_the_bus`, which reads every message
  the bus routes to the client's connection).
- **2026-10-05 · A file written during its read is not reported (review
  round 6).** A flush reads files whose last event was a completed write,
  but the next write can begin while it reads: an in-place save's
  `O_TRUNC` sets the size to zero before its `MODIFY` is queued (on ext4
  freeing the old blocks in between takes milliseconds, and under load
  the writer can be descheduled there), so a read can see an empty or
  partial file with no event yet to say so. After hashing, the flush
  takes `SEEK_DATA` on the same descriptor (ext4, btrfs and tmpfs take
  the inode lock for it, which a truncation holds until its `MODIFY` is
  queued), takes the stamp again from that descriptor, and then drains
  the inotify queue. A file whose stamp moved, whose modification time
  is less than one quiet period (15 ms) old, or that an event drained
  then names (a `MODIFY`, creation or removal; outside the config
  directories also a `CLOSE_WRITE` or `MOVED_TO`), or any file after an
  overflow, is left out of the batch with its baseline unchanged: held
  while its write is in progress, read again at a later quiet period
  otherwise (`a_write_during_the_read_is_not_reported_until_closed`,
  `own_writes_in_place_in_a_tight_loop_are_never_torn`). The
  modification-time rule covers filesystems that take no lock for
  `SEEK_DATA`, and overwrites without truncation, whose `MODIFY` is
  queued after the write returns. It is waived for a file due for
  `max_delay` (below, review round 7). Corrected in round 7: an earlier
  version of this paragraph said a completed save is a quiet period old
  by the time it is read, which is false for a batch cut at `max_delay`.
- **2026-10-05 · A file rewritten without pause is read within
  `max_delay` (review round 7).** The round-6 rules put a file rewritten
  more often than every 15 ms (a live-preview tool, a settings file
  rewritten during a slider drag, a status file replaced by a script)
  off for ever: at every cut its last write was under 15 ms old, and
  putting it off opened a new batch with a new 500 ms bound. That broke
  design.md's "a batch never stays open more than 500 ms". Now a file
  put off remembers the first event that made it due; the open batch is
  cut for it 500 ms after that event (but not sooner than 15 ms after
  the flush that put it off, so it is not re-read in a busy loop), and
  from then on a read of it is taken even if its modification time is
  recent, provided the other checks pass: stable stamp, and no drained
  `MODIFY`, creation or removal (outside the config directories, no
  drained `CLOSE_WRITE` either). Inside a config directory, a drained
  `CLOSE_WRITE` or `MOVED_TO` with no `MODIFY` no longer refuses the
  read: every write there makes a `MODIFY`, so none overlapped it, and
  the newer version that event announces is read in the next batch (the
  path is pending again). A rename-over can never be torn: the read
  descriptor holds a complete inode, old or new
  (`a_module_rewritten_in_place_every_5_ms_is_reported`,
  `a_module_renamed_over_every_5_ms_is_reported`,
  `a_watched_file_rewritten_in_place_every_5_ms_is_reported`,
  `a_file_rewritten_without_pause_is_read_within_max_delay`). The batch
  that finally reads a put-off file reports, as `first_event` and
  `last_event`, the events that made it due, not the flush that put it
  off: `sent − last_event` stays the watcher's share of save-to-pixels
  for the reload-latency benchmark, which should use `last_event`.
- **2026-10-05 · File age is measured after the read (review round
  7).** The 15 ms rule measured a file's age from the flush's `now`,
  taken before the rescan, the watch updates and the hashing of every
  other file, and took a modification time after that `now` as "not
  recent": a truncation landing between the two passed. Age is now
  measured from the wall clock right after the read (or the flush's
  `now` if later, which only a test passes), and a modification time up
  to 1 s in the future counts as recent (the clock stepped back a
  little); beyond that, or in a polled directory whose server clock may
  run ahead for good, a future time is not recent
  (`a_write_after_the_flush_began_is_not_read_too_fresh`). What remains:
  outside the config directories writes make no event, so the guarantee
  there rests on the stamp comparison, the 15 ms rule and a drained
  `CLOSE_WRITE`. An in-place writer there that truncates, writes part,
  and then pauses for more than 15 ms before the read starts (and does
  not close before the drain) can be read torn; its `CLOSE_WRITE` then
  re-reads the file and reports the whole content in the next batch.
- **2026-10-05 · A `MODIFY` holds the file (review round 6).** A file
  written in place whose writer keeps it open and stops writing was
  only marked busy: nothing scheduled it, so it was never read with
  `Notice::StalledWrite` as `Options::stalled_write` promises. Now the
  first `MODIFY` also holds every watched file at that path, so it is due
  `stalled_write` after the writer's last write and read then with the
  notice, and again (normally) once closed. In a config directory every
  write is an event, so the hold skips the modification-time check that
  directories without `MODIFY` need (and reads no `stat` per write)
  (`a_stalled_in_place_writer_is_read_with_a_notice`,
  `a_stalled_in_place_write_is_read_with_a_notice`).
- **2026-10-05 · `O_TMPFILE` files outside the config (review round 6).**
  A file made with `O_TMPFILE` and linked in (`linkat`) is created with
  its bytes. In a directory watched without `MODIFY` it is held as being
  written, and its `CLOSE_WRITE` comes only under its unnamed `#<ino>`.
  `IN_EXCL_UNLINK` drops that event (the file has no name), so the file
  waited 5 s and came with a false `StalledWrite`. Completed-write
  watches no longer set `IN_EXCL_UNLINK`, and a `CLOSE_WRITE` named
  `#<digits>` ends the write of each name in that directory with that
  inode (one `stat` per file being written there). Config directories
  keep `IN_EXCL_UNLINK`: there a writer still holding a deleted module
  would otherwise mark the new file at that name as being written with
  each `MODIFY`. Outside them, the cost is that a deleted file's writer
  closing it reports a `CLOSE_WRITE` under its old name, which re-reads
  whatever file has that name now
  (`a_file_linked_in_from_o_tmpfile_is_reported_outside_the_config`).
  A file closed before it is linked in sends its close before its
  creation and still waits for `stalled_write`.
- **2026-10-05 · Module-set diagnostics travel with the batch (review
  round 6).** `ModuleSet` carries `Discovery::errors` (as text) and
  `too_deep`. When a runtime rescan's lists differ from the previous
  ones, the batch carries `Notice::ModuleSet { errors, too_deep }` with
  the complete new lists, so a module that became a dangling link or an
  unreadable directory reaches the loader's overlay on the same channel
  as the module's `Removed` (`module_set_errors_are_forwarded`).
- **2026-10-05 · The cost of `MODIFY` in config directories (review
  round 6).** Every `write(2)` to any file in a config directory wakes
  the watcher thread, including files Strand never reads (a log or cache
  kept in `~/.config/strand`). The event is dropped at once, but a tight
  writer pays for it: in review, 1.46 million one-byte writes in about
  1.2 s cost the watcher about 85 % of a core while saves were still reported in
  16 ms. This is the price of never reading a half-written module.
  Files other than modules should live outside the config directory
  (`$XDG_STATE_HOME`, `$XDG_CACHE_HOME`); a settings TOML written a few
  times per second costs nothing noticeable.
- **2026-10-05 · The tight-loop own-write test is bounded (review
  round 6).** `own_writes_in_a_tight_loop_are_never_reported` failed
  twice under load in review, and did not fail again here in about 50
  runs (40 alone, 8 within the whole suite), with three or four busy
  loops loading the machine. Each of its writes queues five events (create,
  modify, close, moved-from, moved-to). Up to 3,000 writes, at about 5
  events each, come near the kernel's default 16,384-event queue, so a
  watcher thread starved for most of the loop overflows it. The
  overflow's rescan batch, correct but without changes, failed
  `no_batch`. The loop now stops at 1,000 writes (5,000 events), and the
  test checks that no batch carries a change (a change-less rescan
  batch is allowed), naming the body behind each reported hash.

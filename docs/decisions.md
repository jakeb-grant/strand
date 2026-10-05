# Decisions

Interpretations of `design.md` where it is ambiguous. One short paragraph
per decision, grouped by track.

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

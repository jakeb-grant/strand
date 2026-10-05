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
ends quiescent). A learned rank (Incremental-style heights) would remove
the re-runs; not needed while shell effects that write are rare `on change`
handlers.

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
* 0.05 }` at 60 Hz is the user, not a loop. Writes are counted per tick,
not per call (many writes in one handler run coalesce; read-your-writes
holds); writing the current value is not counted. The 31st attempted write
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
cycles).

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
counted. Reload's "remaining time rescaled" is `Timer::rescale_from(old)`.
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
sort; the order is then unspecified but keeps every key.

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
listeners and tasks inside stop running (work that comes due is held),
state is kept, memos stay readable, and the runtime can be idle. Events
delivered while frozen are not seen by its listeners (a frozen component
ignores input). `rt.resume(scope)` (the fixing reload) runs held work at
the next flush; timers that came due fire at the next tick.

**2026-10-05 · Service lifecycle.** No observed/unobserved hook on cells:
the compiler knows which service paths each component reads, so the VM
reference-counts services per mounted, visible component (acquire on
mount and show, release in the scope's `on_cleanup` and on hide), and the
services layer applies the 5 s stop delay. Graph observation would see
memos read lazily by the emitter, not visibility.

**2026-10-05 · Left for wave 2.** `persist` storage with a default hash and
settings files are not in `strand-core` yet (they need file IO and the
schema from `strand-compiler`).

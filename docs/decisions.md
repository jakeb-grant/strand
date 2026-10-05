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
timer conditions) run once per flush in creation order, which puts owners
before the nodes they own and is the topological order for sinks (sinks
never depend on each other). An effect re-triggered by a later effect's
write in the same flush runs again; past `MAX_RUNS_PER_FLUSH` (16) runs it
is a runtime cycle: the flush reports `Error::Cycle` with the path
`effect -> cell -> ... -> effect` and defers the rest to the next tick, so
a loop can't freeze the logic thread.

**2026-10-05 · What "changed" means.** `Tick::changed` lists watched nodes
whose value differs from the value last reported, so a value written and
restored within one tick is not reported. Effects are not given that
guarantee: an effect may re-run when a direct input was written and
restored within the tick (it sees the same, consistent values). Keeping a
copy of every input per effect would cost memory on every node.

**2026-10-05 · Errors are values.** Memo closures return `Result<T,
Error>` and the error is stored as the memo's value (cut-off still
applies). Stale handles give `Error::Disposed`; a write inside a memo is
`Error::WriteInDerived` (derived values are pure); handler and effect
errors land in `Tick::errors`. Observers of a disposed node re-run and see
`Disposed`.

**2026-10-05 · Write-rate monitor.** ">30 writes per second to one cell
from one handler" counts ticks, not calls: many writes to one cell in one
handler run coalesce to one (and read-your-writes holds). The 31st tick
within a sliding second warns once (`Diagnostic::WriteRate` naming
`handler -> cell`) and defers the write; later writes replace it (latest
value wins) and it lands when the window has room. Any write that goes
through supersedes a deferred one. Writes from outside handlers (services,
CLI) are not counted. Keyed collections are not gated: their diffs can't be
coalesced to a latest value; revisit if a collection loop shows up.

**2026-10-05 · Echo suppression.** `write_tagged` applies a local write
and returns the `Generation` to send. `receive(value, tag)` ignores the
echo of any pending write (by tag, or by value when the protocol has no
tags, as with D-Bus `PropertiesChanged`) while newer writes are in flight.
When the service acknowledges the last write with a different value
(clamping), that value is applied. A value matching no pending write is an
outside change: it is applied and clears pending writes.

**2026-10-05 · Events.** Each event is delivered once to every listener
alive at delivery time, in emission order, during the next flush; events
emitted by listeners are delivered in the same flush.

**2026-10-05 · Timers and `on change`.** `after T while c` counts only time
during which `c` holds, fires once, and schedules nothing while paused.
`every T while c` does not replay missed periods after a long gap (it fires
once and re-phases). A reactive duration keeps the time already counted.
Reload's "remaining time rescaled" is `Timer::rescale_from(old)`: the new
timer keeps the fraction of its period the old one had counted. `on change
x` never fires for the first value; `on change x after T` restarts its
countdown on every change (debounce). Time is the logic clock the host
passes to `tick(now)`; `next_deadline()` tells it when to wake.

**2026-10-05 · Handlers.** A handler is a `Future` owned by a node and
polled during the flush when woken; no async runtime is needed. Disposal
drops it (cancelled at its current `await`, reported as
`Diagnostic::Cancelled`); a dropped `sleep` unschedules itself. Wakers are
thread-safe and call an optional wake hook so a service reply on another
thread can wake the logic loop.

**2026-10-05 · `Async<T>`.** `x ?? fallback` is `Async::or`: the value if
there is one (kept while pending and after an error), else the fallback.
Each load gets a `RequestId`; a response to a superseded request is
ignored. A load cancelled by unmount leaves the cell as it was (the cell
usually unmounts with it).

**2026-10-05 · Keyed collections.** `move` is spelled `move_key` in Rust
(keyword). Keys come from a key function (`key app`); `update` that changes
the key field is an error and is reverted. `VecDiff::Move { from, to }`
removes at `from` and inserts at `to` in the shortened list. `sort_by` is
stable (ties keep source order). Operator closures are pure; the values
they capture (`!dnd` in a filter) come through a tracked `params` closure
(`filter_with` etc.), and a params change rebuilds and publishes a keyed
diff against the previous output, never a bare reset, so identity
survives. Each collection keeps the last 256 diffs; a consumer further
behind gets a `Reset`.

**2026-10-05 · Left for wave 2.** `persist` storage with a default hash and
settings files are not in `strand-core` yet (they need file IO and the
schema from `strand-compiler`).

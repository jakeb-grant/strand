# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, token evaluation per frame, layout, damage, paint, presentation | Wait on the logic thread, run handlers, evaluate bytecode |
| Logic | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program | Touch Wayland or pixels |
| Compiler worker | `strand-compiler` | Parse, check, lower changed modules off-thread | Mutate live state (it hands a compiled `Program` to logic) |
| Text worker | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: a painted surface keeps drawing its last layout (or a realigned stand-in from another scale or width) until the new one arrives |
| Watcher | `strand-watch` | inotify, portal, IPC socket | Parse files (it sends paths and hashes) |
| Persist IO (one per `PersistStore`) | `strand-core` | Atomic writes of persisted cells, settings-file edits, settings overlays and last-good snapshots; reports each file it is about to change to `PersistStore::on_written` | Run on the logic tick or block logic (failures come back as diagnostics in a later tick) |
| Services | `strand-services` | tokio current-thread runtime; PipeWire and toplevel get their own threads | Block logic: they send state diffs and events |

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s (`strand-scene`) and layout facts
(`self.width` for container queries). No locks are shared across threads on a
hot path.

## Crate graph

```
strand-scene      shared vocabulary: ids, geometry, colour, scene protocol, Painter
  ^   ^   ^
  |   |   strand-surface   (layer-shell, shm, damage submit, input, frame timing)
  |   strand-render ── strand-text
  strand-core ── strand-compiler ── strand-dev (LSP, inspector)
     ^
     strand-services, strand-watch
strand (binary) wires everything.
```

`strand-scene` has no heavy dependencies; it is what lets render and surface
be built and tested without the language, and the language without pixels.

## Contracts

### `strand-scene`

- **Geometry**: `Rect { x, y, w, h }` in physical pixels as `i32`/`u32`,
  `Size`, `Point`, and a logical-pixel `f32` variant; `Scale` is the
  fractional scale as numerator/120 (`wp_fractional_scale_v1`).
- **Colour**: `Color` stored as straight-alpha sRGB `f32`, with exact
  conversions to OKLab/OKLCH; interpolation for springs happens in OKLab.
- **Damage**: `Damage` is at most 8 `Rect`s; adding a ninth merges the pair
  whose union grows area least. `Damage::area()` is what the M0 exit
  criterion (≤2,000 px² per clock tick) is measured on.
- **Painter** (render implements, surface calls):

  ```rust
  pub struct PaintTarget<'a> {
      pub pixels: &'a mut [u8],   // ARGB8888 premultiplied, little-endian (wl_shm)
      pub size: Size, pub stride: u32, pub scale: Scale,
      pub age: u8,                // buffer age: 0 = unknown contents, 1 = last frame, ...
      pub time: Duration,         // predicted presentation time (wp_presentation clock)
  }
  pub trait Painter {
      /// Paint everything that changed for `surface` and return the damage,
      /// already widened to cover the buffer's age and clipped to the buffer.
      fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage;
      /// True while something is dirty or a spring or time signal on this
      /// surface is unsettled; the surface manager requests frame callbacks
      /// only while true.
      fn wants_frame(&self, surface: SurfaceId) -> bool;
      /// Fully opaque part of the last painted frame, in buffer pixels.
      fn opaque_region(&self, surface: SurfaceId) -> Damage { Damage::new() }
  }
  ```

  Buffer-age rule: a non-empty `paint` result is a new frame and the
  caller must commit that buffer with exactly that damage; an empty result
  means nothing was drawn or recorded, so the caller does not commit (or,
  if it commits anyway, does not count it). `age` counts commits of that
  surface. If a painted buffer cannot be committed, call
  `Renderer::invalidate(surface)`. `PaintTarget::new` sets `time` to zero;
  the surface manager sets it (`.at(t)`) and tests pass fixed values so
  springs sample deterministic timestamps. `opaque_region` is in buffer
  pixels; `wl_surface.set_opaque_region` takes surface-local logical
  coordinates, so convert with `Scale::inner_logical_region`, which rounds
  inward and never claims a translucent pixel. The input region (shadows
  grow the buffer but not the input region) joins this trait with M2
  layout.

- **Input**: `InputEvent` (`PointerEnter`/`Leave`/`Motion`/`Button`/`Axis`
  with `ButtonState`, `AxisDelta`, `AxisSource`) in surface-local logical
  pixels, per `SurfaceId`. `strand-surface` produces it; render hit-tests
  it on the main thread and forwards node events to logic. Wayland
  serials stay in `strand-surface`.

- **Scene protocol** (logic → render, one batch per tick): `SceneDiff`
  holding ordered `SceneOp`s over a retained tree: `Create { id, kind,
  parent, index }`, `Remove { id }` (render plays `exit` before unmounting),
  `Move { id, parent, index }`, `SetProp { id, prop, value, transition }`,
  `SetTokens { table, transition }` (logic sends `Instant` for the table
  it boots with; later swaps spring palette roots from M2). Node ids are generational; a removed id is dead
  at once (logic may reuse the slot with a new generation in the same
  diff). `Move`'s `index` counts the new parent's children after the node
  is detached. Prop values are typed (`Length`, `Color`, `Paint`, `Text`,
  `Shadow`, ...); comma shorthands (`margin: $space.2, $space.2, 0`,
  `radius: 14, 14, 0, 0`) may arrive as a `List` of 1–4 values, expanded
  like CSS, and call-shaped values (`hit: grow(6)`, `filter:
  grayscale(1)`, `backdrop: blur(16)`, `transition: wipe(left)`) are
  `PropValue::Call { name, args }`. A surface's declared name (`bar Top`)
  is `Prop::Name` (`Text`), set by the compiler. `transition` is `Default` (the token spring for that
  prop class), `Token(path)` (`~ $motion.bouncy`), `Spring { .. }`,
  `Duration { .. }` or `Instant`, matching `~` in the language;
  `TokenScope::transition` resolves the first two through `$motion.*`
  tokens at render time (props of class `Snap` always snap). `Create`/`Move` take `parent: Option<NodeId>`
  (`None` for surface roots) and `PropValue::Unset` reverts a prop to its
  default. Render maps a surface root to Wayland surfaces with
  `Renderer::attach_surface(SurfaceId, NodeId)`. A surface-kind node
  created under a parent (a `popup` in a `bar`) is its own root: it
  inherits tokens, colour and font from its ancestors but paints only on
  its own surface. `radius: full` is `Corners::FULL` (infinite radii) or
  `PropValue::Keyword("full")`; a `%` radius is of the shorter side.
  Token-bound values travel unresolved as `PropValue::Token(TokenExpr)`
  (`$path`, colour methods `alpha`/`mix`/`lighten`/`darken`,
  `oklch(from …)` with channel arithmetic, and `Template` for composite
  values whose colours are tokens, such as `border: 1, $border`), also
  nested inside a `List`, `Pose` or `Call` (`pad: 0, $space.3`), which
  `TokenScope::resolve` resolves in place. The
  `TokenTable` sent by `SetTokens` holds plain values (palette roots,
  scales, fonts, `PropValue::Transition` springs for `$motion.*`) and
  derived tokens as expressions; render evaluates references at flatten
  time, every frame, so only palette roots need to spring. Logic still
  resolves which theme applies. Subtree overrides (`set { $x: … }` and a
  component's `tokens { }`, as `Toast.radius`) are the `tokens` prop
  holding a `PropValue::Tokens` table; render resolves through a
  `TokenScope` chain (global table, then each ancestor's override,
  nearest first). An override's right-hand side sees its parent scope
  (`set { $surface: $surface.alpha(0.5) }` is not a cycle) and global
  derived tokens are evaluated in the asking node's scope, so they stay
  derived inside the subtree. `enter`/`exit` are props whose
  value is a `PropValue::Pose` (prop/value pairs) or a preset keyword.

- **Surfaces**: `SurfaceSpec` (in `strand-scene`) is what a surface-kind
  node asks of Wayland: kind, name (namespace `strand-<Name>`), edge,
  anchor, layer, keyboard, margin (`Insets`), requested logical width and
  height, `screens` (`All`, `Focused`, or `Named` monitor identities, which
  logic uses to pin each per-monitor `bar` instance), `open` and `attach`,
  plus `exclusive_zone()` and `needs_recreate()` (kind, namespace or layer
  changed). Render resolves specs through the node's token scope after
  every `apply`: `Renderer::surface_spec(node)` reads one, and
  `Renderer::take_surface_changes()` returns `(NodeId, SurfaceChange)`s,
  `Created(spec)`, `Updated { spec, recreate }` or `Removed`, in order;
  token changes that move a resolved value count as updates.

- **Render loop** (the binary wires this; surface calls `Painter`):
  0. After each `apply`, drain `take_surface_changes()` and hand them to
     the surface manager: create a layer surface (or xdg_popup, lock
     surface) per matching output for `Created`, reconfigure or recreate
     it for `Updated`, destroy it and `detach_surface` for `Removed`.
  1. Spawn the text worker with `TextWorker::spawn_with_waker(config,
     Some(waker))`, where the waker pings the main calloop loop, and
     build `Renderer::new(TextBackend::Worker(worker))`.
  2. When the ping fires, call `Renderer::update()`: it collects
     delivered layouts and marks the surfaces they change dirty. Without
     this, changed text reaches the screen only with the next unrelated
     event.
  3. Call `apply(diff)` once per logic tick with the `SceneDiff` received.
  4. On output and configure events call `attach_surface`,
     `configure_surface(size, scale)` (before the first paint, so text is
     shaped ahead of it) and `detach_surface`; these also free per-scale
     atlases and text no surface uses.
  5. Request a frame callback while `wants_frame(surface)`, and in it call
     `paint`. Commit only a non-empty result, with exactly that damage
     (`damage_buffer`) and the converted `opaque_region`; if the commit
     fails, call `invalidate(surface)`. Text still being shaped does not
     keep `wants_frame` true: the delivery does, through step 2. Text
     layouts are per (node, scale, line box width), since `center`/`end`
     alignment happens in the line box. A surface that has never painted
     (or whose text worker restarted) holds its first frame for its own
     layouts, up to the first-frame wait (default 50 ms, set with
     `Renderer::set_first_frame_wait`; the demo uses 500 ms): if
     `frame_deadline(surface)` is `Some(t)`, arm a timer for `t` and check
     `wants_frame` again then. A surface that has painted draws, while its
     layout is re-shaped, a stand-in from another scale or width,
     resampled and shifted so its alignment lands where the right one's
     will; layouts no surface wants are pruned.

- Later (render, planned with taffy and size springs in M2):
  - Single-line text that neither wraps nor truncates should be shaped
    once without a width bound, with the start/center/end offset applied
    at flatten time, and keyed by (node, scale) only. Keying it by the
    exact `max_width` bits, as now, means a box whose width springs
    re-shapes every frame. It also means a reconfigure paints a stand-in
    frame and then a correction frame per text, even when the two are
    pixel-identical (1.7–3.3k px² on sway, never on a tick). Only
    wrapping or truncating text needs a layout per width.
  - `flatten_surface` rebuilds the map of every delivered layout and
    prunes text across all surfaces on each call, which is O(surfaces ×
    texts) per surface. Before popups and launchers share the main
    thread, scope the map to the surface's root, or keep it per node
    and update it in `deliver`, and prune once per update.

### `strand-core`

A fine-grained push-pull graph, generic over value types
(`T: Clone + PartialEq + 'static`): `Signal` (state), `Memo` (lazy derived),
`Effect` (observer at the edge, e.g. the scene emitter), batching into ticks,
generational handles where a stale read returns an error. Keyed collections
publish `VecDiff`s and incremental `filter/map/take/sort_by` keep keys.
`Async<T>` keeps its last value and exposes `pending`/`error`. The dynamic
`Value` used by the VM is defined by `strand-compiler`, not here.

How consumers drive it (wave 1, see `crates/strand-core/src/lib.rs`):

- One `Runtime` per logic thread, passed as `&Runtime`; closures receive it.
  Reads return `Result<T, Error>`; memo closures return `Result<T, Error>`.
- The host loop calls `rt.tick(now)` (advance the logic clock, fire timers,
  then `flush`) and sleeps until `rt.next_deadline()` or the wake hook
  (`rt.set_wake_hook`, also called when a flush leaves a woken task for the
  next one and when `rt.resume` re-queues held work or leaves a timer
  overdue); `rt.is_idle()` (nothing queued for a flush) with
  `next_deadline() == None` means idle. Each `Tick` carries `errors` and
  `diagnostics` (write-rate, cancelled handlers, zero periods) raised since
  the previous tick, for the overlay and `strand watch --json`.
- The scene emitter calls `rt.watch(prop_memo.id())` per bound prop and,
  each tick, turns `Tick::changed` (watched ids whose value changed, in
  creation order) into `SetProp`s; `for` loops read a collection
  `Snapshot` and turn `diffs_since(last_version)` into keyed
  `Create`/`Remove`/`Move` ops. A `for` over a plain list expression
  (`calendar.days(month)`, `n.actions`, an `Async` list) goes through
  `rt.keyed_memo(key_fn, |rt| list)` (or `memo.keyed(rt, key_fn)`,
  `async_memo.keyed(..)`), which diffs each new list by key: no writing
  effect, no second copy in the emitter.
- Mounting a component runs inside `rt.scope(..)`; unmounting is
  `scope.dispose(rt)`, which drops its nodes, timers and handlers.
- The reconciler keeps identity across reloads with `rt.reparent(id,
  new_owner)`: a component moved from `start` to `end`, or a surface's
  state kept across a monitor unplug, moves to its new owner before the old
  one is disposed (keyed cells keep their diff log, so items keep identity).
  A runtime fault freezes one component with `rt.suspend(scope)` (effects,
  listeners and tasks stop, timers pause as if their `while` were false,
  state kept; service events are kept for its listeners up to
  `MAX_FROZEN_EVENTS` each, the oldest dropped and counted in
  `Diagnostic::EventsDropped`, also when the listener is disposed instead
  of resumed; input events dropped; `await sleep(..)` inside it pauses too)
  and the fixing reload calls
  `rt.resume(scope)`, or moves the live state out with `reparent` and
  disposes the frozen scope (held work is released either way). Released
  timers and sleeps count again from the host's next `tick` (the logic
  clock stands still while the host sleeps), and the release calls the
  wake hook so the host ticks. Reloaded
  timers take over the old countdown with `new.rescale_from(rt, old)`
  (`Debounced::rescale_from` for `on change … after`).
- Handlers: listeners, timers and `on change` handlers each get a handler
  site (`rt.site_of(handler)`) owning the tasks their bodies `rt.spawn`;
  disposing the handler (reload restarting changed handler code, or
  unmount) cancels them at their `await` and reports `Cancelled`. Nodes a
  handler creates belong to its component. A VM starting one coroutine per
  event creates a site with `rt.handler_site()` (disposed when the handler
  is replaced) and starts each with `rt.spawn_for(site, fut)` for
  graph-triggered events (service events, `on change`: the 30 writes/s guard
  sees one handler) or `rt.spawn_input(Some(site), fut)` for external input
  (`on click`, `on scroll`, `on activate`: not rate-counted up to the
  task's first suspending `await`; a loop it then runs is counted). Input event
  queues are `rt.input_events()`; `<->` writes from widgets are made
  outside any handler and are not counted either. Timers are
  `rt.after/every(_dyn)`, `on change` is `rt.on_change(_after)` or, for
  service paths, `rt.on_change_keyed(key, ..)` with the path's object as
  key (no firing on a sink switch); `on change` handlers run after the
  tick's other effects settle, so they fire once per outside write.
  Sinks run in a computed topological order (wave 2): ranks over read
  edges, ownership and the write edges handlers make. The compiler must
  declare both kinds of edge it lowers, before the first flush:
  `rt.reads_from(node, &sources)` with the syntactic read set (every
  branch: a conservative superset) of every binding (memo, derived
  collection) and handler (effect, timer condition, `on change` tracked
  expression, listener), even an empty one, and `rt.writes_to(handler,
  target)` for every assignment and `emit`. With both, every sink runs
  exactly once per flush with final values, the first flush (boot,
  reload mounts) included. Undeclared edges are learned when first seen,
  which can re-run a sink once in that flush; a sink that has never run
  and declares nothing runs after all ranked sinks on its first run.
  `rt.rank(id)` exposes the rank. `writes_to` returns
  `Ok(WriteEdge::Ranked | WriteEdge::Feedback)` and errs only for a
  disposed id: a write edge that closes a loop (a self-normalising `on
  change x { if x > 10 { x = 10 } }`, two handlers normalising each other)
  is a feedback edge bounded by the runtime cycle guard, not a static-cycle
  error, and the outcome and ranks do not depend on whether reads or writes
  are declared first (`rt.write_edge(w, t)` tells what an edge became).
  Composite nodes: `rt.async_memo` declares its internal effect's write
  edge to the value itself; the VM declares the input's read set on
  `memo.effect_id()` and readers declare `memo.id()`. For `on change …
  after T` (`Debounced`), the tracked expression's reads go on `d.effect`
  and the body's writes on `d.timer`. Listeners and woken tasks are
  ranked like sinks: an emit is handed to every live listener of the
  queue as soon as the flush sees it, and each listener is delivered at
  its own rank (each gets its events in emit order; listeners of one
  queue are not delivered together), a task at its own or its writer's
  rank, so a listener declared to read a cell a handler (an effect, or
  another listener of the same event) writes in the same flush sees the
  final value, and the readers of what a listener writes run after it;
  at one rank, woken tasks run before listeners, listeners before sinks.
  A listener's body and a task's polls are tracked without subscribing:
  a read not declared on the listener (for a task, on its handler) is
  learned (it ranks the handler from then on) and reported in strict
  mode. Tasks woken from other threads (IO and D-Bus replies) are polled
  at the start of the next flush, never between sinks.
  Reload writes: the reconciler adopts a changed `state` default (and
  makes any other reload-driven change to a live cell) with
  `signal.set_reloaded(rt, v)`, not `set`: the value changes as usual, but
  every `on change` / `on_change_after` / `on_change_keyed` handler
  downstream of the cell takes it as its new baseline in the next flush
  instead of firing, and a debounce is not restarted ("`on change` fires
  on changes, never at boot or reload"). `Persisted::redeclare`, the
  persist hand-over to a waiting cell, `Persisted::reset_reloaded`
  (`@reset`) and `Settings::redeclare` use it; the overlay's `[reset]`
  (`Persisted::reset`) and file reloads of a settings file are ordinary
  writes. For a keyed collection the reload write is
  `xs.replace_all_reloaded(rt, values)` (a keyed diff, by key, as a
  reload write). A load an `rt.async_memo` starts because of a reload
  write lands as a reload write too, whenever it resolves; a value a
  handler (timer, listener) copies from a reloaded cell into another
  cell is an ordinary write.
  Service events are `EventQueue`s. Keyed collection writes from
  graph-triggered handlers are rate-guarded too (wave 2): a throttled
  handler writes to a held copy (with the list it started from) whose
  changes land as one keyed diff, re-applied by key onto whatever other
  writes (input handlers, service batches, other handlers) did meanwhile,
  so the emitter only ever sees `VecDiff`s and no push is lost
  (`Diagnostic::KeyedConflict` counts changes that no longer apply). The
  VM reads collections without copying through `xs.with(rt, |v| ..)`
  (tracked), `xs.with_untracked(rt, ..)` and `xs.get_key(rt, &k)`; a
  `KeyedVec` clone (`get_untracked`) held across a write makes that write
  copy the items and the key index.
  Service `rw` writes use `write_tagged(value, send)` (throttled writes are
  held, then sent) and reports come back through `receive`. `let x =
  svc.call(input)` returning `Async` is `rt.async_memo(input, fetch)`, a
  read-only `AsyncMemo`.
- Service lifecycle (start on first reader, stop 5 s after the last leaves
  or goes invisible) is driven by the VM, not by graph observation: the
  compiler knows which service paths each component reads, so the VM
  acquires them on mount (and when shown) and releases them in an
  `rt.on_cleanup` of the component scope (and when hidden).
- Read-only graph introspection for the inspector, `strand watch` and the
  LSP: `rt.sources/observers/owned(id)`, `rt.site_of(handler)`.
- `state x = d persist` is `rt.persisted(&store, path, d, encode, decode)`
  (wave 2): `store` is one `PersistStore::from_env()` per process
  (`$XDG_STATE_HOME/strand/persist`, one file per cell path; its writes go
  through one persist IO thread, so `fsync` never stalls a tick), and the
  VM supplies a stable byte codec for its `Value`s. `path` names one live
  cell: the cell's `file.name` path plus its instance identity when the
  component has several, `bar[<make model description>].expanded` for a
  `bar` on every monitor (the monitor identity of `strand-surface`), or
  the item key for state on list items (`list[<key>].x`); any bytes are
  allowed, the store escapes them. A second live cell on a path in use is
  reported as `Diagnostic::PersistPathInUse` and waits: it does not write
  while the first owns the file, and takes the path over when the first
  is disposed (the reconciler may mount a replacement before disposing
  the old instance): if it still holds the value it started from, it
  continues from the old owner's last value (flushed first), else its own
  value is written. It returns
  a `Persisted` handle: the cell's `Signal`, the `Restore` decision
  (default, stored, adopted new default, kept over a new default, failed),
  and the calls the reconciler makes: on a reload that changes the
  declared default, `persisted.redeclare(rt, new_default)` (adopt if the
  value still holds the old default, else keep it, report once and
  re-stamp; returns `Redeclared`), for `@reset` at reload
  `persisted.reset_reloaded(rt)` and for the overlay's `[reset]`
  `persisted.reset(rt)` (both cancel a pending or queued write, remove the
  file and set the default; the first is a reload write that `on change`
  handlers take as their baseline, the second the user's write). An
  adopted default and a hand-over to a waiting cell are reload writes too
  (`Signal::set_reloaded`). Warnings arrive as
  `Diagnostic::PersistDefaultChanged` / `Diagnostic::PersistFailed` (write
  failures in a later tick, with a wake-hook call; `rt.is_idle()` is false
  while one waits to be reported). A write is never lost to the debounce:
  unmount, `rt.shutdown()` and dropping the last `Runtime` handle read the
  cell's live value and queue it, even when the owner went in the same
  tick as the write; `rt.shutdown()` waits (bounded) for queued writes.
  A write that fails is written again on the cell's next change or
  capture. Files of instance-qualified paths (a `[` in the path:
  `list[<key>].x`, `bar[<monitor>].x`) that no cell has claimed for 90
  days (`PERSIST_RETENTION`), and quarantined copies that old, are
  removed when the store is dropped at exit and once a day while it runs
  (`PERSIST_SWEEP_INTERVAL`), so per-key files do not pile up; a plain
  declared path (`bar.level`) never expires, however long its component
  stays unmounted; `PersistStore::save`/`remove` are for offline tools (a
  live cell on the path does not see them). `state xs: [T] key f = [...]
  persist` is `rt.persisted_keyed(&store, path, default_keyed_vec, encode,
  decode)`: `encode` writes the list's values, `decode` returns them as a
  `Vec<T>` and the list is rebuilt with the default's key function; the
  `PersistedKeyed` handle (`cell: KeyedSignal`, `restored`) has the same
  `redeclare` (taking the new default `KeyedVec`), `reset_reloaded` and
  `reset`, all applied by key.
- Strand's own writes, for the watcher (wave 2):
  `persist_store.on_written(|w: &OwnWrite| ..)` (also on
  `SettingsStore`: one observer slot per IO thread, so setting it on
  either replaces the other) runs on the persist IO thread (and on the
  caller of `PersistStore::save`/`remove`, which must then not call
  `save`, `remove` or `sync`; a panic removes the observer) for every file it is
  about to replace or remove, with `w.path` (as queued: the declared
  settings path, an overlay, a snapshot or a cell file), `w.target` (symlinks
  followed, canonical directory) and `w.content` (the exact new bytes, or
  `None` for a removal), after the temp file is complete and before the
  rename makes it visible. The binary hashes `content` (BLAKE3) and hands
  the hash to `strand-watch` as pre-registered for `target`, so the
  watcher's no-op check stops there.
- `state prefs from "prefs.toml" { accent: color = #7aa2f7; … }` is
  `rt.settings_file(&settings_store, path, fields)` (wave 2,
  `strand_core::settings`): `settings_store` is
  `persist_store.settings()` (overlays in `$XDG_STATE_HOME/strand/settings`,
  writes on the persist IO thread), `path` the file resolved against the
  config directory, and `fields` one `FieldSpec::new(name, default,
  decode, encode)` per typed field, where `decode(&toml_edit::Item) ->
  Result<V, String>` checks the field's type and `encode(&V) ->
  toml_edit::Item` writes it back (`strand_core::settings::toml_edit` is
  re-exported so the VM uses the same version). It returns a `Settings<V>`
  handle: `signal(name)` is the field's ordinary `Signal` (UI writes, `<->`
  bindings and `strand set prefs.compact true` write it; the write is saved
  through `toml_edit` after 250 ms of quiet, keeping comments, spacing and
  order, following symlinks, temp file plus rename in the target's
  directory); `reload(rt)` is what the watcher calls when the file changes
  (each field checked on its own, a syntax error keeps every last good
  value, a deleted key springs back to its default; edits not yet on disk
  and unsaved UI writes are never undone); `set_overlay(rt, name, v)` /
  `clear_overlay(rt, name)` (the overlay's `[clear]`) manage the runtime
  overlay, which wins over the file, which wins over the default. A
  read-only target (no write permission, `EACCES`, `EROFS`: `/nix/store`)
  gets its writes in the overlay instead. Reports are
  `Diagnostic::Settings(SettingsNotice { file, field, issue })` with
  `SettingsIssue::{Syntax, Unreadable, BadValue, Shadowed, ReadOnly,
  WriteFailed, CorruptOverlay, TypeChanged}`; `Shadowed` displays as
  `accent: file changed but runtime overlay wins [clear]`. Round 3 adds:
  a last-good snapshot per file (`last_good_path()`, under
  `settings/last-good/`) that a broken file falls back to at boot;
  `layer(name) -> SettingsLayer::{Overlay, File, Default}` (inspector
  provenance); `redeclare(rt, fields)` for live reload of the declaration
  (fields matched by name keep their signals; a new default is adopted
  only where nothing set the field; a changed `FieldSpec::with_type` type
  resets that field; added fields are read, removed ones disposed); and
  several handles on one declared file (one per mounted instance) adopt
  each other's writes in the same tick. Watcher hand-off: see
  `strand-watch` below.

### `strand-compiler`

`syntax` (lossless lexer and parser with spans and recovery), `check` (names,
types, did-you-mean), `lower` (bytecode), `vm` (evaluates bytecode against
`strand-core` signals), `reconcile` (old program + new program → identity map
→ `SceneDiff` and state migration). One crate serves runtime, `strand check`
and the LSP. The grammar is specified in `docs/grammar.md`.

Public interfaces other crates and later stages build on:

- **Files** (`strand_compiler::source`): `FileId(u32)` indexes a
  `SourceMap` (`add(name, text) -> FileId`, `get(id) -> Option<&SourceFile
  { name, text: Arc<str> }>`). A `Span { start, end }` is a byte range
  inside one file; `(FileId, Span)` locates text across the config.
- **Parsing** (`strand_compiler::syntax`): `parse(file: FileId, src: &str)
  -> Parse { file_id, file: ast::File, tokens: Vec<Token>, diagnostics }`.
  Never panics. `tokens` is the lossless token stream (trivia included) for
  semantic highlighting, formatting and keyword spans the tree does not
  store. Spans live on `Item`, `Stmt`, `Expr`, `Block`, `Ident` and the
  other wrapper nodes; payload structs use their wrapper's span. Trees are
  at most 256 levels deep (`docs/grammar.md`, "Error recovery"), so passes
  may recurse over them on a 2 MiB stack.
- **Lowering into `strand-core`** (the VM's obligations): declare every
  read set with `rt.reads_from(node, &sources)` (all branches, also when
  empty) and every assignment and `emit` with `rt.writes_to(handler,
  target)` as nodes are created, so effects run once per flush in
  topological order from the first flush on; read keyed collections with
  `with`/`with_untracked`/`get_key` instead of holding `KeyedVec` clones
  (derived collections, `KeyedMemo`, have the same three reads over an
  Rc-shared slice; they keep no key index, so their `get_key` is an O(n)
  scan, fine once per event, not inside a loop over the rows);
  check the lowering in tests: after lowering real fixtures and running a
  few flushes, `rt.stats().learned_edges` (edges nobody declared) and
  `rt.stats().reruns` (sinks run twice in one flush) are 0 unless the
  program has a feedback edge; or turn on `rt.set_strict_edges(true)` in
  the VM's and compiler's test runtimes (and in debug builds), which
  reports every learned edge once as `Diagnostic::UndeclaredWrite {
  writer, target }` / `Diagnostic::UndeclaredRead { reader, source }`, so
  any fixture fails loudly on a missing declaration;
  `writes_to` answering `WriteEdge::Feedback` is not an error (a
  self-normalising handler is valid; only a static cycle among `let`s is a
  load error); for `let x = svc.call(input)` declare the input's reads on
  `memo.effect_id()`, for `on change … after T` the tracked reads on
  `d.effect` and the body's writes on `d.timer`;
  create persisted cells with an instance-qualified path and keep the
  `Persisted` handle for `redeclare` (reload), `reset_reloaded` (`@reset`)
  and `reset` (the overlay's `[reset]`), and `rt.persisted_keyed` /
  `PersistedKeyed` for a persisted keyed collection; adopt any other
  changed `state` default (and apply `@reset` to a non-persisted one)
  with `signal.set_reloaded(rt, v)`, or `xs.replace_all_reloaded(rt,
  values)` for a keyed collection;
  node closures use their `rt` parameter or a `WeakRuntime`
  (`rt.downgrade()`), never a captured `Runtime` clone: that is an `Rc`
  cycle, so neither dropping the last handle nor a persisted cell's
  writer ever runs, and debounced values are lost silently; the binary
  calls `rt.shutdown()` on exit signals (SIGTERM, SIGINT) and on a normal
  exit, before dropping the stores;
  lower `state x from "file.toml" { typed fields }` to
  `rt.settings_file(&store, resolved_path, fields)` with one `FieldSpec`
  per field from the checked schema (the type's decode and encode over
  `toml_edit::Item`, the declared default), keep the `Settings` handle,
  call `reload` when the watcher reports the file (see `strand-watch`), call `redeclare` when a reload changes the
  declaration, pass each field's type name with `FieldSpec::with_type`,
  and give its path to the watcher. See the `strand-core` section.
- **Identity and change detection.** AST `PartialEq` compares spans, which
  shift on every edit above a node. Reload identity and "did this handler
  change" use a span-insensitive structural hash over the texts of the
  significant tokens inside a node's span (comments and whitespace
  excluded), computed from `Parse::tokens`; `reconcile` owns it.
- **Diagnostics** (`strand_compiler::diagnostic`): `Diagnostic { severity,
  code: &'static str, message, labels: Vec<Label { file: FileId, span,
  message, primary }>, help: Option<String> }`, built with
  `Diagnostic::error/warning(code, msg).with_label(span, msg)`,
  `.with_secondary(..)`, `.with_label_in(file, ..)`,
  `.with_secondary_in(file, ..)`, `.with_help(..)`, and `.in_file(file)` for
  single-file stages. `render(&[Diagnostic], &SourceMap, Style)` draws
  miette reports (labels in other files as related reports, at most 50 per
  file); `render_short(&[Diagnostic], &SourceMap)` gives one
  `file:line:col: severity[code]: message` line each, for the reload
  overlay's list and editors. `suggest`/`did_you_mean` give the shared
  near-miss logic.

### Config files

`strand_compiler::source::find_files(dir) -> io::Result<Discovery { files,
dirs, errors }>` defines the **`.strand` module set** of a config, and only
that: `.strand` files at most `MAX_DEPTH = 3` directories below the config
directory, names starting with `.` skipped (files and directories), symlinks
followed, a breadth-first walk with directories and files deduplicated by
canonical path (so each is claimed at its shallowest path), unreadable
sub-directories and dangling `*.strand` links reported in `errors` and
skipped. `dirs` lists the canonical path of every directory scanned, link
targets included. `strand check` uses it today; the loader must call it (not
reimplement it), so they never disagree on the module set. `strand-watch`
does not depend on `strand-compiler` for this: the binary calls `find_files`
and hands the watcher plain paths (`files`, `dirs`) through its constructor,
and again on every rescan through a `rescan` callback the binary supplies.
Directories past `MAX_DEPTH` that hold `.strand` files are listed in
`Discovery::too_deep` so `strand check` can warn that they are not loaded.

It is not the watch set. Per design.md ("Change sources") the watcher also
watches `.wgsl` shader files, settings TOML (`state … from "…"`), wallpaper
and other referenced paths, and the canonical target directories of linked
files and directories (`Discovery::dirs`, plus the parent directory of each
canonical file path). Paths referenced from code come from the compiler
(service and file paths it collects), not from this scan.

### `strand-text`

Request/response over a channel: `TextRequest { key, text, style, max_width,
scale }` → `TextLayout { key, size, glyph runs }`. `TextStyle` holds the
font, line height, alignment, `ellipsis` (start, middle, end), `max_lines`
and `spans` (byte ranges with weight, italic or colour: marks, markup);
a glyph run's `color` is its span's, else the node's. Glyph atlases are keyed by
scale and LRU-bounded. Render draws the last delivered layout.
Each `TextLayout` also carries the `AtlasUpload`s (alpha pixels) for glyphs
rasterised while producing it, which render applies to its mirror of the
atlas in arrival order, and leases on the atlas pages it uses, so the worker
never recycles a page a live layout draws from. Page generations are unique
in the process, so a mirror never confuses a recreated page with an old
one. Two more messages share the request channel, in order:
`cancel(key)` (skip a still-queued request; render sends it when a request
is superseded) and `drop_scale(scale)` (free that scale's atlas; render
sends it when no surface uses the scale and drops its mirror pages at the
same time, so a returning scale re-uploads its glyphs). The worker drains
its queue before shaping (and folds in newly arrived cancels before each
request) and survives a panicking request: it starts a fresh engine and
answers with an empty layout whose `is_reset()` is true, on which render
drops its mirror and every layout and re-requests its text. Each scale's
atlas is capped at `AtlasConfig::max_bytes` of alpha (1 MiB by default;
glyphs that do not fit are skipped), fonts at `MAX_FONT_PX` (512) and
text at `MAX_TEXT_BYTES` (64 KiB) per request. A layout that had to
skip glyphs for want of atlas room says so (`is_incomplete`; render asks
again a bounded number of times), and each layout lists its scale's live
pages (`atlas_pages`), so the mirror drops pages the worker trimmed.
Dropping the worker discards its queue.

### `strand-surface`

Owns the Wayland connection with smithay-client-toolkit: layer-shell surfaces
per output, a 2–3 buffer shm pool per surface with buffer age,
`damage_buffer`, `set_opaque_region`, fractional scale + viewporter, frame
callbacks only while `Painter::wants_frame`, `wp_presentation` timing, output
hotplug (monitor identity = make + model + description), and input forwarded
as `InputEvent`s. It creates and updates surfaces from the `SurfaceSpec`s
and `SurfaceChange`s render reports (render loop step 0): namespace
`strand-<Name>`, anchor from `edge` (stretched along it) or `anchor`,
`margin`, size, `exclusive_zone()`, keyboard interactivity, the outputs
`screens` selects, and mapping by `open`. Compositor-animated poses (alpha modifier, viewporter,
margins) are its job in M4.

Interface (main thread; `SurfaceManager<H>` owns the calloop `EventLoop`
and the connection):

- `trait SurfaceHost: Painter` is what it calls: `paint`/`wants_frame`/
  `opaque_region` plus no-op-default hooks `surface_attached(surface,
  node, Option<&Monitor>)` (`None` for a `screens: focused` surface the
  compositor places), `surface_entered(surface, &Monitor)` (where that
  one was shown), `surface_configured(surface, size, scale)` (once per
  wakeup, right before the first paint at that size), `surface_detached`,
  `monitor_added(&Monitor, reconnected)`, `monitor_changed` (scale,
  logical size or position; same identity), `monitor_removed`,
  `monitor_forgotten` (30 s after an unplug), `frame_deadline(surface) ->
  Option<Instant>`, `frame_dropped(surface)` and `input(&InputEvent)`
  (main thread, for hit testing). The binary implements it on a wrapper
  around `Renderer`, forwarding to `attach_surface`, `configure_surface`,
  `detach_surface`, `frame_deadline` and `invalidate`.
- `SurfaceManager::connect(host, Config)` / `with_connection(conn, ..)`;
  `Config { clock: Box<dyn FrameClock>, fractional_scale, max_buffers }`.
  `dispatch(timeout)` blocks while idle (no timers armed). Other sources
  (logic diffs, the text worker ping) go on `loop_handle()`; their
  callbacks get `&mut State<H>` and call `apply_surface_change(node,
  change)` for each `take_surface_changes()` entry and `poll()` (ask
  `wants_frame` again) or `repaint(surface)` (force a paint).
- `repaint_handle()` gives a `Send` `RepaintHandle` (a calloop channel:
  `Request::{Repaint(id), RepaintAll, Poll}`); `take_input()` creates the
  `mpsc::Receiver<InputEvent>` (events are not queued before; keyboard
  later). The cursor is set on enter (`wp_cursor_shape_v1`, else the
  cursor theme); `State::last_button_serial()` is for popup grabs.
- Frames lock to the refresh rate: after a buffer commit a surface paints
  again only after that frame's callback (requested while `wants_frame`
  stays true, or always without `wp_presentation`) or its presentation
  feedback (`presented`/`discarded`, requested for every commit). Changes
  arriving meanwhile coalesce into the next paint.
- Monitors reach logic through the binary: it forwards the `monitor_*`
  hooks as the `screens` service (design: Monitors → `screens`), from
  which logic instantiates per-monitor surfaces (`Screens::Named`).
  `Monitor` carries identity, connector, make, model, description,
  `scale`, `logical_size` and `position`.
- A painter's hold is honoured: while `wants_frame` is false and
  `frame_deadline` is `Some`, no buffer is committed (the first frame
  waits for its text); a timer at the deadline asks again, and any
  `poll()` before it does too.
- `screens: focused` is one layer surface created without an output
  (wlr-layer-shell puts it on the output the user last interacted with);
  `State::set_focused_monitor(Some(id))` (a compositor IPC service, M3)
  pins it, moving an open one. An `osd` has an empty input region
  (click-through).
- `FrameClock` (`now`, `presented`, `discarded`, `predict(surface)`) is fed
  by `wp_presentation` feedback; `PresentationClock` is the real one,
  `FakeClock` the injectable one. `predict` becomes `PaintTarget::time`.
- `MonitorId` is `"make | model | description"`, minus a trailing
  `" (<connector>)"` wlroots appends (a duplicate gets ` #2`, in plug
  order); `Screens::Named` matches it or the connector name. `SurfaceId`s
  are stable per (node, monitor), or (node, focused), while the monitor is
  remembered.
- Later (planned, so the current shape does not block them):
  - M2: a shadowed surface needs `overhang: Insets` (painter-reported or
    spec-resolved) that grows the layer size and shifts the margins while
    `exclusive_zone` stays, plus `Painter::input_region`; `LayerConfig` is
    built in one place (`placement::layer_config`) so this stays local.
  - M2/M4: surface `exit` poses need the unmap delayed until exit
    settles: render holds `Removed`/`open: false` until its exit is done
    (or a `SurfaceHost::exit_done` hook); spec changes (compositor-animated
    margins) get applied with the next buffer commit when a paint is
    pending instead of a bare commit.
  - M4: `raw_handles(surface)` (display + `wl_surface`) for GPU promotion
    on the same surface; `State::recreate_all()` for `strand reload
    --hard`.

### `strand-services`, `strand-watch`

Specified when their milestones start (M3, M1). Both only produce writes and
events into `strand-core`.

Settings files (fixed in wave 2 by `strand-core`; the Threads table holds:
the watcher reads and hashes, it never parses). On a change event the
watcher hashes the file (BLAKE3). An unchanged hash stops there, and that
includes Strand's own writes: the binary registers
`persist_store.on_written(..)` and passes the hash of each `OwnWrite`'s
`content` to the watcher as pre-registered for its `target` (and its
`path`, when that is a link), before the rename makes the content
visible, so the change event finds the hash already known. A changed hash
goes to the logic thread as (path, hash), and the logic thread calls
`settings.reload(rt)` on every `Settings` handle declared on that path.
`reload` takes the write mark before it reads, so a write of Strand's
that lands meanwhile is never undone. `Settings::sources()`,
`SettingsSources::read_from` and `Settings::reload_with` stay available
for a thread that is allowed to parse, such as the compiler worker, in
case reading on the logic thread ever shows up in a profile. The watcher
does not call them. Strand's temp files next
to a settings file are named `.<name>.tmp.<pid>.<n>` (renamed over the file:
the watcher sees `MOVED_TO` for the file itself); its scratch-name filter
should ignore that pattern, as it does editors' scratch names.

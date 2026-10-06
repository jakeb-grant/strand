# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, token evaluation per frame, layout, damage, paint, presentation | Wait on the logic thread, run handlers, evaluate bytecode |
| Logic | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program; the `strand` binary's IPC Unix socket (`strand reload`, `strand watch`, M5's `get \| set \| toggle \| watch \| call`) is a source on this loop | Touch Wayland or pixels |
| Compiler worker | `strand-compiler` | Parse, check, lower changed modules off-thread | Mutate live state (it hands a compiled `Program` to logic) |
| Text worker | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: a painted surface keeps drawing its last layout (or a realigned stand-in from another scale or width) until the new one arrives |
| Watcher | `strand-watch` | inotify directory watches and polling (one `strand-watch` thread); not the IPC socket (`docs/decisions.md`, wave2-watch) | Parse files (it sends paths and hashes) |
| Persist IO (one per `PersistStore`) | `strand-core` | Atomic writes of persisted cells, settings-file edits, settings overlays and last-good snapshots; reports each file it is about to change to `PersistStore::on_written` | Run on the logic tick or block logic (failures come back as diagnostics in a later tick) |
| Services | `strand-services` | tokio current-thread runtime (the portal Settings client `strand_watch::follow` and the compositor IPC adapters run here); PipeWire and toplevel get their own threads | Block logic: they send state diffs and events |

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s (`strand-scene`) and layout facts
(`self.width` for container queries: `Renderer::take_layout_facts`, the
laid-out sizes that changed, sent as `run::ToLogic::Layout`). No locks are shared across threads on a
hot path.

`strand run [dir]` (`crates/strand/src/run.rs`) is this wiring: the main
thread's surface host forwards the monitor hooks (`screens` as a list of
plain `ScreenInfo`s, `monitor_forgotten` as `Forget(id)`), pointer input
on the node under the pointer (`Renderer::hit`'s chain: `hover` along
it and `pressed` on the chain under a press as `Flag`,
`click`/`secondary` on the innermost node both the press and the release
were over and `scroll` on the innermost node as `Event { node, event:
NodeEvent }`, which logic bubbles to the nearest handler) and surface
sizes to the logic thread over a calloop channel (`run::ToLogic`).
`NodeEvent` is one variant per kind of input, each with its own payload
(`Click`, `Secondary`, `Middle`, `Scroll { dy, dx }`, `Activate`, `Key {
name, text, modifiers }` (a `Key` record made by the service host,
`args_with`), `Dismiss`; `name()` and the args are what `Instance::event`
takes); M4 adds `Drop { payload, at }` for `on drop(p: T, at: int)` as a
new variant of the same message. Keyboard input is routed on the main
thread (`demo/host.rs`, `Forward`): the focused node of the focused
surface gets `key(k)`; an `input`'s typing and a surface's `open: false`
(Escape, focus loss) are `ToLogic::Write { node, prop, value }`, which
logic applies with `Instance::write` (a two-way write); `nav` and list
selection become `Flag { Selected }` and `Activate` (decisions.md,
wave3-pixels). Before the logic thread
starts, `live::Worker::spawn` starts the `strand-watch` watcher (module
set from `find_files`, rescan callback calling it again), boots the
`Loader` (the boot `Outcome`: a build, or a cached last good one, or
none, with diagnostics) and starts the `strand-compile` thread, which
compiles each watcher batch and `strand reload` off the logic thread and
sends `live::FromWorker::{Loaded, Settings}` on a calloop channel; logic
sends it `Job::{Reload { hard, client }, Referenced(settings files)}`
(the `Loaded` a reload causes carries the IPC clients it answers). A
load that commits nothing but clears the last attempt's problems (a
broken save reverted to the last good text, `Outcome::cleared`) is
sent too, so the overlay closes and `strand watch` hears it; an attempt
on exactly the files the one before it read (`Outcome::repeated`: the
watcher's re-listing after a `strand reload`) repeats its problems
without compiling and is not sent again. The
persist store's `on_written` registers Strand's own writes with the
watcher. The logic thread commits each `Loaded` (`Instance::reload`,
`reload_hard`, held back while a lock is shown: the newest such load
waits, absorbing older ones, and is committed after the step that
closes the lock, with the newest attempt's held files and diagnostics;
a load committed meanwhile drops it), keeps the error overlay
(diagnostics, and reload notices with their `[reset]`, which calls
`Instance::reset`; rows about one cell replace each other)
(`overlay.rs`, external nodes; 250 ms quiet) and freezes faulting
components, and serves the IPC socket (`ipc.rs`) as sources on its loop;
it owns the runtime, `SchemaHost::real`
and the `Instance`, loops on `Instance::step`, sends each non-empty diff
on a calloop channel, and sleeps in a calloop loop of its own until a
message, the runtime's wake hook (a ping, so the hook holds no sender
and the thread ends when the main thread's senders are gone), the logic
clock's `Wake::deadline` (the dispatch timeout) or `Wake::wall` on a
`CLOCK_REALTIME` timerfd (`TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET`:
a resume or a clock step wakes it at once). SIGINT, SIGTERM (a
`signalfd` on the main loop, the signals blocked in every thread) and
the compositor going away send `ToLogic::Shutdown`; the main thread
joins the logic thread, which unmounts the instance, runs
`Runtime::shutdown` and drops its stores, so debounced persist and
settings writes reach the disk before the process exits. Layout facts
(`ToLogic::Layout`) address every laid-out node; a surface's configured
size still arrives as `Size` on its node. `STRAND_MOCK=desktop` fills the
host with a mock desktop for screenshots before M3 (`mock.rs`).

**IPC** (`crates/strand/src/ipc.rs`): a Unix socket at `$STRAND_SOCKET`
or `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock`, newline-delimited
JSON. Requests are `{"v": 1, "cmd": …}`; each is answered with one line
`{"ok": true, …}` or `{"ok": false, "error": …}`, and an unknown `cmd`
or a newer `v` is refused without closing the connection, so M5's `get`,
`set`, `toggle` and `call` are new `cmd`s on the same socket. Version 1:
`reload` (`"hard"`; answered once the reload is committed or held, with
its event; at once with `"deferred": true` in the event while a lock is
shown), `reset` (`"path"`: a state cell back to its default, as the
overlay's `[reset]`) and `watch` (`{"ok": true}`, then one event per line:
`{"event": "reload", files, committed, held, unreadable, from_cache,
classes, kept, kept_over_default: [{path, shown}], reset: [{cell, why}],
ambiguous, notices, restarted, cancelled, deferred, timing: {watch_ms,
compile_ms, commit_ms, total_ms}, diagnostics: [{severity, code,
message, help, at: {file, line, column}, labels, short}]}`,
`{"event": "notices", kept_over_default, notices}` for cells kept over a
changed default outside a reload (persisted cells at boot, a parked bar
back; with nobody watching they go into the next reload event's
`kept_over_default`) and lowering's notices, and `{"event": "fault",
message, at, frozen}`). `total_ms` runs from the watcher's last event
behind the save to the moment the diff holding the reload is sent to
render. A client whose socket cannot take its output yet gets a write
source on the logic loop until it is written (no polling); one more
than 1 MiB behind is dropped.

## Crate graph

```
strand-scene      shared vocabulary: ids, geometry, colour, scene protocol, Painter
  ^   ^   ^
  |   |   strand-surface   (layer-shell, shm, damage submit, input, frame timing)
  |   strand-render ── strand-text
  strand-core ── strand-compiler ── strand-dev (LSP, inspector)
     ^
     strand-services ──> strand-watch (EventSink, CompositorEvent; portal follow;
                                       strand-watch depends on no Strand crate)
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
  inward and never claims a translucent pixel. The input region is not a
  painter question: render puts the shadow reach into the surface's spec
  (`SurfaceSpec::overhang`) and the surface manager sets the region to
  the box inside it.

- **Input**: `InputEvent` (`PointerEnter`/`Leave`/`Motion`/`Button`/`Axis`
  with `ButtonState`, `AxisDelta`, `AxisSource`; `KeyboardEnter`/`Leave`
  and `Key { key: KeyInput }` with the xkb keysym name, the typed text,
  `Modifiers` and repeat) in surface-local logical pixels, per
  `SurfaceId`. `strand-surface` produces it; render hit-tests
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
  `PropValue::Call { name, args }`. A prop naming another node (`nav:
  results`) is `PropValue::Node(id)`. A surface's declared name (`bar Top`)
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
  `overhang` (how far shadows reach past the box, filled in by render
  from layout), plus `exclusive_zone()` and `needs_recreate()` (kind,
  namespace or layer changed). A surface without a size of its own (a
  panel, OSD or popup without `width`/`height`, a bar without a
  thickness) gets it from a content layout pass in render before the spec
  is reported. Render resolves specs through the node's token scope after
  every `apply`: `Renderer::surface_spec(node)` reads one, and
  `Renderer::take_surface_changes()` returns `(NodeId, SurfaceChange)`s,
  `Created(spec)`, `Updated { spec, recreate }` or `Removed`, in order;
  token changes that move a resolved value count as updates.
- **Layout**: taffy 0.14 on the render thread (`layout.rs`): one pass
  per surface whose layout inputs changed (paint-only props never
  relayout; `Renderer::layout_passes` counts them), boxes in surface
  logical pixels (`Renderer::boxes`), `x`/`y` applied at flatten time as
  paint offsets. `Renderer::scroll(surface, point, dy)` scrolls the
  innermost `scroll`/`list` under a point and `scroll_into_view(list,
  row)` reveals a row; a `list` lays out only the rows in view. Text is
  measured from delivered layouts (estimated until the first arrives).
- **Hit testing**: `Renderer::hit(surface, LogicalPoint) -> Vec<NodeId>`
  is the node under a surface-local logical point in the last frame (the
  topmost in paint order: later siblings over earlier ones and their
  children) and its ancestors up to the surface's root (the root alone
  where nothing is). A node is hit inside its laid-out rounded box, grown
  by `hit: grow(n)` and cut by its ancestors' clips; shadows never count.

- **Render loop** (the binary wires this; surface calls `Painter`):
  0. After each `apply`, drain `take_surface_changes()` and hand them to
     the surface manager: create a layer surface (or xdg_popup, lock
     surface) per matching output for `Created`, reconfigure or recreate
     it for `Updated`, destroy it and `detach_surface` for `Removed`.
  1. Spawn the text worker with `TextWorker::spawn_with_waker(config,
     Some(waker))`, where the waker pings the main calloop loop, and
     build `Renderer::new(TextBackend::Worker(worker))`. `Renderer::text()`
     returns the backend; `TextWorker::is_running()` is false once the
     worker thread has ended, which with the handle still held means it
     panicked (shaping panics are caught and the engine restarted).
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
     `wants_frame` again then. A surface that has painted holds a frame
     the same way while some text on it has no layout at any scale or
     width (a node just added), up to `NEW_TEXT_WAIT` = 16 ms (set with
     `Renderer::set_new_text_wait`), so a new node and its glyphs reach
     the screen in one frame; only while the surface is idle (no frame
     painted within `BUSY_WINDOW` = 34 ms, counted from when its last
     raster finished; `Renderer::set_busy_window` changes it, for tests):
     a surface in motion paints at once and the glyphs follow a frame
     later (decisions.md, wave2-exit). A surface that has painted draws, while its
     layout is re-shaped, a stand-in from another scale or width,
     resampled and shifted so its alignment lands where the right one's
     will; layouts no surface wants are pruned.

- Text is shaped once without a width bound per (node, scale) and
  aligned in its box at flatten time; only a box narrower than it asks
  for a layout of its (whole-pixel) width (decisions.md, wave3-pixels).
- Later (render):
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
  task's first suspending `await`; after it, writes count against that one
  run of the task, not the site, so one write per event after an `await`
  is never throttled but a loop inside the run is). Input event
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

`syntax` (lossless lexer and parser with spans and recovery), `schema`
(builtin elements, services, functions and tokens as data), `ty` (types),
`check` (names, types, did-you-mean) producing `hir` (the typed program),
`lower` (bytecode), `vm` (evaluates bytecode against
`strand-core` signals and services; codecs for core's persistence and
settings files), `instantiate` (mounts a
program and emits scene diffs), `reconcile` (old program + new program →
identity map → `SceneDiff` and state migration; M1 live reload). One crate serves runtime, `strand check`
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
  How the compiler meets this: `lower::reads` computes every chunk's
  syntactic read set (`Program::reads(chunk)`: `state`s, `let`s,
  settings fields, scope locals, service fields, element instances;
  the lambdas a chunk makes and the `fn`s it calls included) and
  write set (`Program::writes(chunk)`: assignment and list-mutation
  targets, and the services its action calls can change). The instantiator resolves those names to core nodes in the
  scope a chunk is mounted in (`ServiceHost::sources` names a service
  field's nodes) and declares them as each node is created: binding
  memos, `let`s (after every name of the body is bound), component
  parameters, derived lists and their effects, `if`/`match` effects,
  timers (duration and `while`), `on change` effects (tracked targets),
  listeners (empty), the token table; writes on the handler site (`on
  click`, `on svc.event`), the `on change` effect, the timer, and for
  a debounce `d.effect`/`d.timer`, for an async `let`
  `memo.effect_id()`.
- **Identity and change detection.** AST `PartialEq` compares spans, which
  shift on every edit above a node. Reload identity and "did this handler
  change" use a span-insensitive structural hash over the texts of the
  significant tokens inside a node's span (comments and whitespace
  excluded), computed from `Parse::tokens`; `reconcile` owns it.
- **Reconcile** (`strand_compiler::reconcile`): `Build { program,
  identity, hashes, sources, warnings }` is a compiled config, plain data
  (`Send`): `Build::compile(prev, SourceMap)` (or `compile_with` against a
  schema) checks and lowers it, inheriting identities from `prev`.
  `Identity` gives every element, surface, component call, `for`, `if`,
  `match`, handler and timer a `Sid` (source text through a token diff,
  then `id:` name, then position among same-kind siblings; ambiguity
  resets with `Identity::warnings()`); surfaces share one label, so a
  kind change keeps them. `Hashes` are Merkle hashes per handler and
  timer over everything they reach (`fn`s, `let`s, types, keyframes,
  custom services; `locks()` over every `lock` subtree,
  `changed_services(old)`; a group of declarations naming each other is
  hashed once as a strongly connected component, linear in the def
  graph). `Report { classes: Vec<EditClass>, kept, reset, notices,
  kept_over_default: Vec<KeptCell { path, shown }>, ambiguous,
  restarted, cancelled }` is what a reload did (`notices` is the prose;
  `kept_over_default` and `ambiguous` are the same facts as data, for
  the overlay's `[reset]`, `strand watch` and M5's inspector badges), its
  `EditClass` names the rows of design.md's edit table (`token`, `prop`,
  `node-added`, `node-removed`, `state-default`, `state-reset`,
  `handler`, `timer`, `surface`, `service`, `lock-deferred`, `hard`).
  `reconcile::loader::Loader::new(root, schema, cache_dir)` is the
  compiler worker's state: `boot()`, `changed([(path, exists)])`,
  `rescan()` each return an `Outcome { build, committed, held,
  diagnostics, sources, unreadable, from_cache, cleared, repeated,
  compile_time }`
  (`cleared`: the last attempt had errors, held or unreadable files and
  this one has none, even when nothing changed against the last good
  build): the
  largest consistent set of changed files committed, the rest held with
  the diagnostics of the whole attempt; a commit stores the sources under
  `Cache` (`$XDG_CACHE_HOME/strand/last-good/<config hash>`) keyed by
  their hashes, `COMPILER_VERSION` and `Schema::fingerprint()`, and a
  config broken at boot starts from them.
- **Diagnostics** (`strand_compiler::diagnostic`): `Diagnostic { severity,
  code: &'static str, message, labels: Vec<Label { file: FileId, span,
  message, primary }>, help: Option<String>, suggestions: Vec<Suggestion {
  file, span, replacement }> }`, built with
  `Diagnostic::error/warning(code, msg).with_label(span, msg)`,
  `.with_secondary(..)`, `.with_label_in(file, ..)`,
  `.with_secondary_in(file, ..)`, `.with_help(..)`, and `.in_file(file)` for
  single-file stages (it moves suggestions too). A suggestion is the
  replacement a diagnostic proposes, as data: `.suggest(span, x)` /
  `.suggest_opt(span, Option<x>)` / `.with_suggestion(span, x)` set the
  help to "did you mean `x`?" and propose `x` for the text at `span` (the
  misspelt word, which need not be the primary span: `on chnage a, b`);
  `.add_suggestion(span, x)` adds one choice of several (the parameters a
  call does not set yet). Editors and the overlay read `suggestions`,
  never the help text. `render(&[Diagnostic], &SourceMap, Style)` draws
  miette reports (labels in other files as related reports, at most 50 per
  file); `render_short(&[Diagnostic], &SourceMap)` gives one
  `file:line:col: severity[code]: message; help` line each, names in
  double quotes (design.md's `unknown prop "expanded"; did you mean
  "open"?`), for the reload overlay's list and editors. `suggest`/`closest` give the shared
  near-miss logic: optimal-string-alignment distance within about one
  edit per three letters, one-letter words matched only by case, no
  one-letter candidate for a longer word, ties to a plausible typo
  (dropped, added or swapped letters), then the closest length, then the
  alphabet (never hash-map order).
- **Schema** (`strand_compiler::schema`): everything the language knows
  before reading a config, as data: element kinds (typed props with
  `two_way`/`inherited` flags and sub-blocks, positional argument type,
  events with payload types, names in scope such as `screen`, `leaf` /
  `surface` / `only_in` flags), records and enums, services (global names
  bound to records whose fields carry `rw`, with `fn` methods, `action`s and
  `event`s), builtin functions with overloads (`lift` passes null through,
  `Async<T>` returns), builtin values (`t`), methods on builtin types,
  palette roles and base-tier tokens. It is written in a small declaration
  language (`schema/builtin.schema`, described in the module docs) and
  parsed once by `Schema::builtin() -> &'static Schema`. Service crates
  contribute their schemas the same way: clone the builtin, call
  `Schema::extend(text) -> Result<(), Vec<SchemaError { line, message }>>`
  (it adds and never replaces: an existing element, group, type, alias,
  value, palette role or token, or a function or method overload with the
  same parameters, is a "declared twice" error; every record's `key` path
  must name a field, re-checked across all records after the extension;
  it is atomic, so on error nothing is added and the fingerprint is
  unchanged), and check with
  `compile_with(&map, &schema)`. The builtin's service stubs, and the
  records only services hand out (`Window`, `Notification`, `Date`, …),
  are declared `provisional service` / `provisional record`: the first
  extension that declares the same name replaces the stub in place (same
  `RecordId`, so `[Window]` fields of other services see the real one; the
  stub's members and docs go) and the name stops being provisional, so a
  second contribution is "declared twice" (`Schema::provisional:
  BTreeSet<String>`). `handle record X` marks a record whose values are
  runtime handles (`Node`, `Canvas`; `RecordDef::handle`): they compare,
  but `persist` and settings files refuse them, as they refuse any record
  that declares an `action`. An element flagged `on_demand` (`popup`,
  `tooltip`, `page`; `ElementFlags::on_demand`) mounts its children only
  on demand, which the component-cycle check reads like an `if`.
  Configs see contributed names as a prelude
  their own declarations shadow, with one exception: a config `service`
  named like a builtin or contributed service is `check::redeclared`
  (services are identified by name at runtime), so a crate that adds a
  service name a config already declares breaks that config; namespace
  new service names. An element's positional names the prop it fills,
  `element meter(float -> value)` (`ElementSchema::arg_prop`), which the
  checker and lowering both read. The LSP reads the same
  table for completion and hover (M3, "service schemas drive type checking
  and LSP hover"). Members after `.` come from one table the checker
  itself types `x.name` and `x.name(…)` by: `schema::members_of(&Ty,
  &Schema, &TypeTable) -> Vec<MemberInfo { name, kind: Field | Method, ty,
  sigs, writes, doc }>` (records and services from the schema with their
  docs, builtin-type methods, and the generic list and `Async` members —
  `len`, `first`, `filter`, `take`, `remove_key`, `pending`, `value`, … —
  built for the item type; `list_members`, `async_members`,
  `ASYNC_TRANSFORMS`). `///` comments in schema text document the entry they
  precede: `Schema::doc(&DocKey) -> Option<&str>` with `DocKey::{Type(name),
  Member(type, member), Function(name), Value(name), Method(type, method),
  Element(kind), Prop(kind, name) (also `on event`, `stroke.dash` and
  scope names; group docs reach their elements), Token(path)}`;
  `RecordDef::doc` mirrors `DocKey::Type`; `Schema::token_doc(path)` falls
  back to the nearest documented group (`space.2` reads `space`). Every
  entry of the builtin schema is documented (a test enforces it). A parameter's default keeps its
  source text (`ParamSig::default: Option<String>`). `Schema::fingerprint()
  -> [u8; 32]` is BLAKE3 chained over every text `extend` was given, in
  order (the builtin first): the schema part of the compiled-output cache
  key (source hash + compiler version + schema hash), so a service crate
  that changes its schema invalidates configs compiled against the old
  one. The schema's element kinds and props are the scene's
  (`strand_scene::protocol::NodeKind`, `Prop`), checked by
  `strand-compiler/tests/scene_catalogue.rs`; `id` is compiler-only.
  Two scene props exist for the language's sake: `Prop::Dash` (`"dash"`,
  class `Effects`), the `dash` sub-prop of `stroke` (`stroke: 3, $accent
  { dash: 6, 4 }`, dash and gap lengths), and `Prop::InputType`
  (`"type"`, `Snap`), an `input`'s `type: text | password` (grammar.md);
  render stores both and draws them when strokes and inputs land.
- **Types** (`strand_compiler::ty`): `Ty` is `Error` (already reported,
  accepted everywhere), `Any`, `Null`, `Unit`, `Prim(Prim)` (`bool int float
  length percent angle duration color paint text path font shadow insets
  corners`), `Opaque(name)` (`Palette`, `Spring`, `Mask`, …), `Enum(EnumId)`,
  `EnumType(EnumId)` (an enum as a value, `options: Look`), `Record(RecordId)`,
  `List(T, keyed)`, `Optional(T)`, `Async(T)`, `Fn(Arc<FnSig>)`, `Tuple`
  (comma shorthands) and `Union` (schema props only). Records and enums live
  in a `TypeTable` (schema first, then the config's `type`, `enum`,
  settings files and custom services), with `assignable(from, to)`,
  `join(a, b)` and `show(ty)` for messages.
- **Checking**: `strand_compiler::compile(&SourceMap) -> Compiled { parses,
  program: hir::Program, diagnostics }` parses every file and checks them as
  one program (`check::check(&[Module { file, name, ast }], &Schema) ->
  Checked { program, diagnostics }` for callers holding parses already); the
  module name is the file stem. It never panics and always returns a
  program: an unresolved name or ill-typed expression becomes `Ty::Error`,
  so one mistake gives one diagnostic. Diagnostics are sorted by file and
  position, syntax and checker together. Scoping: components, surfaces,
  enums, `type`s, `fn`s, token sets, keyframes and custom services are
  global across the config's files; top-level `state`/`let` belong to their
  file and are reachable elsewhere (and from the CLI) only as `file.name`
  when exported; `state`/`let` in a tree belong to their component,
  surface or list item. Every lazily checked declaration runs under
  `stacker::maybe_grow`, so checking is safe on small worker-thread
  stacks (the LSP's, the reload compile's) however long a chain of
  declarations is. An untyped `state`/`let` holding a whole number is an
  `int` in the HIR unless a fraction is written to it (then `float`).
  Fraction writes the source shows plainly are pinned before checking;
  any found while checking re-run the checker, with the pins closed over
  the hand-offs between such states, direct or through untyped `let`s,
  locals and fn values (`Checked::passes` says how many ran; there is no
  cap, see decisions.md). An untyped component parameter has the joined
  type of its call sites' arguments, each checked as an untyped `let`'s
  value (`count + 1` is an `int`); in a cycle of inferring components a
  later call that widens it re-runs the checker with it widened.
- **HIR** (`strand_compiler::hir`), the typed, resolved program the VM
  lowering, the reconciler and the LSP consume. `Program { files:
  Vec<FileHir { file, name, items }>, defs: Vec<Def>, locals: Vec<Local>,
  types: TypeTable, tokens: BTreeMap<path, Ty>, refs: Vec<Reference> }`.
  Declarations are `DefId`s (`Def { name, kind, file, span, ty, exported,
  owner }`, `DefKind::{Component, Surface(kind), State, Settings, Let, Fn,
  Enum, Type, Tokens, Keyframes, Service}`); parameters, `for` bindings,
  event parameters, handler `let`s, element-scope names and `id:` names are
  `LocalId`s; every element has a program-unique `NodeIdx`. Items mirror the
  syntax (`Component { def, params, tokens, body, has_slot }`, `Surface {
  def, element }`, `StateDecl`, `LetDecl`, `FnDecl`, `TokenSet` with
  flattened `TokenDef { path, span, override_, value }`, `Use`,
  `ServiceDecl`, `Keyframes`, `Handler`, `Timer`, `Permit`); tree `Node`s
  are `Element { node, kind: Builtin|Component|Unknown, span, arg, id,
  props, children }`, `When`, `If`, `For { binding, iter, key, body }`
  (`key: None` when items bring their own), `Match`, `Handler { event,
  params, body }`, `Timer`, `Pose`, `Slot`, `Set`, `Selector`, `Play`,
  `State`, `Let`. An element's props are split from its children; `Prop {
  name, span, value, two_way, transition, sub, inherited }`. Every `Expr {
  kind, ty, span }` is typed; names are resolved in the kind (`Local`,
  `Def`, `Service`, `Value`, `Node` for `self`/`hover`/an `id:` name,
  `Variant`, `EnumType`, `Token(path)`), calls name their `Callee`
  (`Fn`, `Builtin { name, overload }`, `Method { receiver, name, overload
  }`, `Record`, `Value`) and arguments their parameter index. Spans are
  file-local (the file is the enclosing `FileHir`'s). `refs` lists every
  resolved name use with its `Target` (def, local, service, builtin,
  variant, token, field, element, file); `Program::reference_at(file,
  offset)` serves go-to-definition, hover and rename, and
  `Program::exports()` lists `file.name` paths for the CLI.
- **Lowering** (`strand_compiler::lower`): `lower(&hir::Program,
  &Schema) -> lower::Program`, plain data (`Send`), so the compiler
  worker builds it and hands it to logic. Every expression a binding,
  `let`, `fn`, handler or timer evaluates is a `Chunk` of bytecode (`Op`:
  a stack machine; `&&`, `||`, `??`, `?.`, ternaries, `match`, `if` and
  `for` statements are jumps, `await` is an op, assignments are `Store`
  to a `Place` and list mutations `Mutate`), with constants that are not
  run-time values. The tree around them stays a small structure the
  instantiator mounts: `Node::{Element, Surface, If, For, Match, Slot,
  State, Let, Handler, Timer, When, Pose, Set, Play}`. An `Element`'s
  props carry their scene `Prop` and schema type (the positional argument
  is the prop it fills: `text`'s `text`, `icon`/`image`'s `source`,
  `meter`'s `value`, from `ElementSchema::arg_prop`); a `<->` prop carries its `TwoWay` place. A `Body`
  (component, surface, `for` item) lists the elements it owns and the
  services it reads.
- **VM** (`strand_compiler::vm`): `Value` is the dynamic value
  (numbers with their unit, text, colours, enums, records by `RecordId`,
  lists, comma and space values, `Async`, closures, node handles, and
  token-bound values kept symbolic as `strand_scene::TokenExpr`, so
  `$surface.alpha(0.72)` reaches render unresolved). `Vm::eval(rt, chunk,
  env)` runs a chunk synchronously; called inside a core memo, every
  `state`, service field and node flag it reads is tracked, and only what
  the run read (a ternary's taken branch). `Vm::handler(rt, chunk, env,
  frame, ctx)` is a handler body as a future for `rt.spawn*`: it
  suspends at `await` and is cancelled by dropping it. Scopes are `Env`s
  (the config's root for file `state`/`let`, then component, surface,
  `for` item and branch scopes) holding `Slot::{Signal, Memo}`s; element
  instances' `hover`/`pressed`/`focused`/`selected`/size are `NodeState`
  signals in the scope that owns the element (so `id:` names work across
  a body). Errors are values: a failing binding is an `Err` in its memo
  (the prop keeps its last good value), a failing handler an `Err` its
  task returns; `fn` and lambda calls nest at most `MAX_CALL_DEPTH` (200).
  Handler frames are scoped: `Op::ScopeEnter`/`ScopeExit` around each
  block that binds locals and around each `for`, whose `IterNext` drops
  the previous iteration's locals, so a loop keeps a frame of constant
  size; a lambda captures only its free locals (`lower::Lambda::free`).
- **Time-bound values (M4 plan, not built yet).** `t`, `wave(…)` and
  `noise(…)` read 0 (`noise` once) on the logic thread today, with one
  `lower::time_signal` warning per name (`Program::warnings`, reported
  as boot-tick notices). They are to travel like tokens: `Value` gains a
  symbolic variant holding a `strand_scene` time expression (a
  `TokenExpr`-like tree over `Time`, `Wave { period }`, `Noise { seed }`
  leaves and the same arithmetic, so `t * 20deg` or `10 * wave(2s)`
  stays an expression), arithmetic on it builds the tree as
  `builtins::binary` already does for `TokenExpr`, and `convert` maps it
  to a `PropValue` variant that render evaluates per frame (`t` per
  node, from its appearance). A prop holding one is a frame-driven prop
  for render's frame scheduling; nothing else in the emitter changes.
  A time-bound value reaching logic (a handler, a comparison, `match`)
  is an error value, as a token in arithmetic without numbers is now.
- **Services** (`strand_compiler::vm::ServiceHost`): the VM's only way
  to services.
  - `restart(rt, name, record, types)` / `stop(rt, name)`: a reload
    changed (or added) / removed custom service `name`'s declaration;
    only that service restarts or stops. Built-ins never do. Both
    default to nothing.
  - `read(rt, service, field)` and `call(rt, service, method, args)`
    (`fn` methods: `clock.format`, `calendar.days`, `workspaces.on`)
    must read through the graph (a `Signal<Value>` per field) so
    bindings depend on exactly that field.
  - `sources(rt, service, field: Option<&str>) -> Vec<NodeId>`: the
    core nodes a read of `service.field` (or of the service as a whole,
    `None`: a method such as `clock.format`) depends on, which the VM
    declares with `rt.reads_from` before the first flush. A superset is
    fine; the default (none) leaves those edges to be learned on first
    run.
  - `action_writes(rt, service) -> Vec<NodeId>`: the cells an action of
    `service` can write (`n.expire()` changes `notifications.popups`),
    declared with `rt.writes_to` on every handler that calls one, so
    readers are ranked after it from the first flush. Lowering names
    the service from the receiver's type (the service itself, or every
    service whose fields reach the item's record). The default is
    `sources(rt, service, None)`, every field: a superset.
  - `read_keyed(rt, service, field) -> Option<KeyedSignal<ValueKey,
    Value>>`: a list field published as a core keyed collection
    (`notifications.popups`, `workspaces.all`). A `for` directly over
    it follows its `VecDiff`s (one new notification is one diff from
    the service to the scene) instead of comparing whole lists; `None`
    (the default) for plain fields.
  - `write(rt, service, path: &[PathSeg], value)` writes one `rw` leaf:
    `audio.sink.volume = 0.8` is `write("audio", [Field("sink"),
    Field("volume")], 0.8)`, so a service sends only what changed and a
    concurrent `muted` change is not overwritten. Hosts tag writes with
    core's `Signal::write_tagged` (generation per written cell) and
    match the service's reports with `receive`, so the echo of a write
    is ignored (`SchemaHost` writes the field's cell with
    `write_tagged`).
  - `fetch(rt, service, method, args) -> Fetch` (a boxed future):
    `let x = svc.m(args)` whose method returns `Async` is `rt.async_memo(
    args, fetch)` per mounted `let`, created on the `let`'s first read
    (a closed launcher never searches). Each change of the argument
    tuple starts one fetch and drops the superseded future (cancelling
    it); the value keeps its last result while pending. The default
    runs `call` once and is ready at once. Async calls anywhere else
    (inside a larger expression) still go through `call`.
  - `action(rt, ActionTarget::{Service, Item(&record)}, name, args)` runs
    `notifications.clear()` or `ws.focus()`; `event(rt, service,
    event) -> EventQueue<Vec<Value>>` is the lossless queue `on
    notifications.received(n)` listens to.
  - `acquire`/`release(rt, service)`: a reader count. Every mounted
    component and the config's top level hold the services their body
    reads; a surface holds its body's services only while shown (its
    `open` is true, or it has no `open`), a hidden surface's content
    (components in it, surfaces nested in it) lets go of everything it
    holds, a surface nested in another (`popup` in a `bar`) holds its
    own children's reads only while it is shown (they do not count for
    the body around it: `lower::Element::services`), and a parked bar
    (monitor unplugged) lets go of everything under it until it
    returns. The
    service starts on its first reader and stops 5 s after its last
    leaves or goes invisible: `rt` lets a host create a service's cells
    lazily on `acquire` and arm the 5 s stop with core's timers on
    `release` (also called from scope cleanup: no synchronous disposal
    there).
  - Several service crates, one host (M3 plan): `Instance::new` takes one
    `Rc<dyn ServiceHost>`, and `ServiceHost` (with `Value`, `RecordId`
    and the rest of the VM's dynamic types) stays in `strand-compiler`:
    `strand-services` depends only on `strand-core` (crate graph) and
    never sees `Value`. A service runs on its own thread (tokio, or
    PipeWire's), and core's `Runtime` is `Rc`-based and not `Send`, so a
    service never holds logic-thread cells. The split, per service
    (the `#[service]`/`#[derive(Store)]` contract features.md plans):
    `#[derive(Store)]` generates a `Send` patch type (one variant per
    field, keyed-list diffs, events) that the service thread sends over
    a channel; a logic-side store, built on the logic thread, applies
    those patches to its cells (`Signal<T>` per field, `KeyedSignal` for
    keyed lists, an `EventQueue<T>` per event). Writes, actions and
    `fn`/async methods go the other way as messages to the service
    thread; a method's result comes back as a patch that completes the
    `Async` the call returned. The adapter from those logic-side stores
    to `ServiceHost` lives on the language side: the binary (`strand`,
    which depends on both) gives each service one `ServiceHost`
    implementation that wraps its logic-side store and converts between
    its typed cells and `Value` (a `Memo<Value>` over each typed field,
    so a binding still depends on exactly that field; writes and actions
    converted back and sent to the service thread),
    and a composite host routes every call by service name to the member
    that serves it (`SchemaHost::real` answers the rest at their
    defaults, and the clock and calendar stay there), unions `next_wake`
    (earliest) and fans out `wake`. A service name belongs to exactly one
    member; `declare`d custom services go to the member that implements
    their source kind (`dbus`, `file`, `listen`, `poll`). If the
    conversion turns out to be generic over `#[derive(Store)]` (a store
    describing its fields by name), it moves into `strand-compiler`
    behind a `strand-core` trait instead; no edge from `strand-services`
    to `strand-compiler` is added either way.
  - `declare(rt, name, record)` adds a custom service; `next_wake(rt) ->
    Option<SystemTime>` and `wake(rt, now)` let wall-clock services (the
    clock) wake the host loop only at minute boundaries (seconds only
    while a binding shows them).
  - `vm::schema_host::SchemaHost` implements it with every schema
    service at its defaults; lists of keyed records are keyed
    collections. `SchemaHost::mock` (clock fixed at 2026-10-05 09:41:07
    UTC; tests `set` fields, `emit` events, read `take_actions()`,
    `take_writes()` and `readers(service)`, `hold`/`release_fetch` a
    method to keep its fetches pending) and `SchemaHost::real` (the wall
    clock and calendar in local time, other services at defaults until
    M3 service crates implement the trait; it records no action or
    write history).
- **Storage** (`strand_compiler::instantiate::Storage { persist:
  Option<strand_core::PersistStore>, settings: Option<SettingsStore>,
  config_dir }`): `Storage::from_env(config_dir)` (one IO thread for
  both), `Storage::in_dirs(state, config)` for tests, `Storage::none()`
  keeps nothing. `state x = d persist` is core's `rt.persisted(store,
  path, d, encode, decode)` with the VM's codec
  (`vm::persist::{encode_bytes, decode_bytes}`: JSON by declared type,
  enums by variant name, records by field name). The path is the
  cell's owner and name qualified by its instance:
  `toasts.dnd` (a file's state), `Clock.open` (a component's or
  surface's), `TopBar[<monitor id>].expanded` (a bar on every monitor),
  `Row[<item key>].open` (state in a `for` item); two live instances on
  one path are core's `PersistPathInUse`. The `Persisted` handles are
  kept: `Instance::reset(path)` is `@reset` (and the overlay's
  `[reset]`; a cell that is not persisted is set to its default). A keyed list `state` that
  is persisted stays a plain signal (core persists `Signal`s).
  `state prefs from "prefs.toml" { typed fields }` is `rt.settings_file(
  store, path, fields)`: the path resolved against `config_dir` (`~/`
  from `$HOME`), one `FieldSpec` per field with the declared default,
  its type name, and a TOML codec by type (`vm::persist::{decode_item,
  encode_item}`: colours as `"#rrggbb"`, durations as `"200ms"`/`"6s"`
  or seconds, enums by variant name, lists and inline tables). Each
  field is its own signal: `prefs.compact` reads (and `prefs.compact =
  true`, `<->`, `strand set theme.prefs.compact true` write) that field
  only; `prefs` alone reads as a record. Without a settings store the
  fields hold their defaults. The watcher gets the files from
  `Instance::settings_files()` and calls `Instance::reload_settings(path)`
  when one changes (core's `Settings::reload` on every mounted handle;
  the off-thread `reload_with` path is the watch track's).
- **Instantiation** (`strand_compiler::instantiate`): `Instance::new(rt,
  Arc<lower::Program>, Rc<dyn ServiceHost>, Storage)` mounts the
  program; `Instance::tick(now)` (or `flush()`) runs the core tick and
  returns an `Update { diff: SceneDiff, errors: Vec<RuntimeError>,
  diagnostics, notices, kept: Vec<KeptCell> }` (`kept`: persisted cells
  kept over a changed default, also in `notices`): one diff per tick,
  the boot one starting
  with `SetTokens { transition: Instant }`, later token tables with
  `Default`.
  - Each bound prop is one watched memo folding the base binding and
    its `when` blocks in source order (later wins; each source keeps its
    own `~` transition); `if`/`match` are effects swapping branch
    fragments.
  - `for` keeps one item per key with its own value cell (an
    `Update` diff sets that item's cell only, so one changed row re-runs
    one row's bindings). Over a keyed `state` or a host's keyed field it
    follows that collection's diffs; over any other expression it is
    `rt.keyed_memo` over the list keyed by `key e` or the item record's
    key. `Move` becomes scene `Move`s; a `Reset` is reconciled by key,
    moving only the items outside the longest run already in order.
    A chain of `.filter`/`.map`/`.take`/`.sort_by` on a keyed `state`
    or a host's keyed field (`lower::For::chain`) is core's incremental
    views (`KeyedOps::{filter_with, map_with, take_with,
    sort_by_with}`) fed by the source's diffs: each lambda runs per
    item through the VM, and what it reads besides the item (its
    captured values and the values of its read set; a keyed collection
    by its version) is the step's tracked parameters, whose change
    rebuilds that step. A lambda calling a service method falls back
    to `keyed_memo`. A `let` whose value is such a chain (or a chain on
    another such `let`; `lower::Program::let_chains`) is one shared view
    (`Slot::View`: the `KeyedMemo` and a lazy list memo), built once the
    body's keyed `state`s are bound: `let shown = notifications.popups
    .filter(…).take(5)` then `for n in shown` and `shown.len` follow
    that view, so one new notification is one diff from the service to
    the scene. The `for`'s key check looks through the `let`s to the
    collection at the bottom.
  - Reads of a keyed collection (`xs.len`, `.first`, `.last`, `xs[i]`,
    `xs.contains(x)` on a keyed `state`, a view `let` or a host's keyed
    field) are
    `Op::Keyed`: answered with core's `with`/`get_key`, never by
    building the list as a value. The list value (`Slot::Keyed`'s
    memo) is built lazily, only for reads that need the whole list.
  - `await` on an `Async` waits for its operation: a `sleep`, or for an
    async `let` that is pending, the load settling (a settle effect
    wakes the awaiters). The operation is shared by every copy of the
    value, so several handlers awaiting it get the same result.
    `await` on a pending value with nothing to wait on is an error
    value, never a silent null.
  - A `bar` is a keyed instance per `screens.all` item that its own
    `screens:` picks (a connector or monitor id, a list of them,
    `focused`, `all`), keyed by the monitor's identity (`Screen.id`:
    make, model and description), with `screen` in scope and `screens:
    "<monitor id>"` (`Screens::Named`) set by the instance. A bar whose
    monitor leaves `screens.all` is parked: its nodes are `Remove`d (render
    plays their exit), its scope is frozen and its services let go, and
    it comes back as it was (nodes recreated under their ids with their
    last props, `Instant`) when the monitor returns, or is dropped by
    `Instance::forget_screen(id)` (the surface layer's
    `monitor_forgotten`, 30 s after the unplug). Every surface gets
    `Prop::Name`.
  - A surface's own element events (`on show`, `on hide`, `on dismiss`)
    are always live; the rest of its body is mounted when it is first
    shown and frozen (`rt.suspend`) while hidden. The instance sends
    `show` when `open` turns true (at mount for a surface without
    `open`) and `hide` when it turns false; render does not send them.
  - `exit` mirrors `enter` when not given. Component `tokens { }`
    entries (`Toast.radius`) join the global table, so an ancestor's
    `set { $Toast.radius: … }` still overrides them.
  - Runtime errors are values, located: `RuntimeError { what, error,
    file, span, node, component, scope }`, the span being the failing
    operation's (from `Chunk::spans`; the innermost, so a prop failing
    because a `let` it reads failed points into the `let`), else the
    binding's, handler's or timer's. `Instance::origin(node) ->
    (FileId, NodeIdx, Span)` maps a scene node back to its element (the
    overlay's click to `$EDITOR`, inspector provenance), and
    `Instance::freeze(&err)` suspends the faulting component's instance
    scope (`thaw` resumes it after the fixing reload). A fault at the
    config's top level (a file's `let`, handler or timer) has no scope:
    it is outlined, nothing is frozen.
  - The host loop: `Instance::step(now, wall) -> (Update, Wake)` sets
    wall-clock services to `wall` (every step: a wall clock that jumps
    back is followed, and a clock reader mounted by the step sees the
    time now), ticks the logic clock to `now`
    and says when to come back (`Wake { deadline, wall }`,
    `sleep_for(now, wall_now)`). The `strand run` logic thread is: feed
    the `screens` service from the surface layer's monitor hooks
    (`screens.all` with each `Screen { id: MonitorId, name: connector,
    … }`, `screens.focused`; `monitor_forgotten` →
    `forget_screen`); map render's `InputEvent`s to `event(node, name,
    args)` (`click`, `secondary`, `scroll` with `dy, dx`, `activate`,
    …; delivered through core input queues to the innermost element
    with a handler, every handler of that event on it in source order,
    `propagate()` passes it on once), `set_flag(node, NodeFlag, on)`,
    `set_size(node, w, h)` (layout facts for `self.width`) and
    `write(node, prop, PropValue)` (`<->` writes, outside any handler,
    checked against the place's type); call `step` and send the diff to
    render; sleep until `Wake::deadline` (logic clock), `Wake::wall`
    (on a realtime timer, so a suspend or clock step does not delay
    it), input, a monitor hook or the runtime's wake hook, whichever
    comes first. These take scene
    `NodeId`s; render produces them with `Renderer::hit`.
  - `get`/`set(path)` read and write exported `file.name` values and
    fields inside them (`theme.prefs.compact`; the CLI), `set` checked
    against the declared type. Dropping an `Instance` (or `shutdown`)
    disposes everything it mounted. `SceneMirror` applies diffs to a
    retained mirror, checks their consistency and renders it as text
    for snapshots.
  - Live reload: `Instance::from_build(rt, &Build, host, storage)`
    mounts a build with its identities; `Instance::reload(&Build) ->
    Report` commits a new one into the running instance. The new
    program is mounted next to the old one, which hands over by reload
    key (the scope's place in the instance tree, `/s<sid>[<monitor>]/
    c<sid>/f<sid>[<key>]`, plus the node's `Sid`): scene nodes keep
    their ids (the diff is the new tree reduced against what render
    shows: props that changed with their own transitions, moves,
    creates, removes; a kept surface whose layer or namespace changed
    gets a new id with its children moved under it; one of another
    surface kind is a new node with its children and state kept), state
    cells are
    reparented (a changed default adopted only where the value still
    held the old one, as a reload write; renamed or retyped cells and
    `@reset` reset; a surface's cells are keyed by its scope, not its
    name, so a renamed surface keeps them; a surface changed between
    `bar` and a single surface keeps its cells when the bar has one
    instance on that side, and resets them with a warning when it has
    several), handlers with an unchanged hash keep their tasks
    (others are disposed with the old instance, cancelling their
    `await`), timers and debounces rescale from the old countdown, a
    parked bar's cells wait for its monitor. While a `lock` is shown a
    build that changes a lock is not committed (`LockDeferred`); the
    caller retries after `lock_shown()` turns false. Lock hashes are
    compared with the running build's, so once a lock edit waits, every
    later build carries it and waits too (decisions.md, wave2-runtime). `reload_hard(&Build)`
    unmounts everything (persisted cells flushed) and mounts afresh. The
    diff comes with the next `step`/`tick`/`flush`.
  - `Instance::freeze(&RuntimeError)` also outlines the frozen
    component's top nodes (or the failing node) with a 2 px red
    `border`; `thaw` restores it, and a reload's new tree clears it.
  - Nodes outside the program (the error overlay):
    `external_create(kind, parent, index)`, `external_set(id, prop,
    value)`, `external_remove(id)`, `is_external(id)`. They share the
    instance's id allocator and diff, and survive reloads (hard ones
    too); input on them is the caller's.

- **Formatting** (`strand_compiler::fmt`): `format(src) -> Result<String,
  FormatError>` (and `format_parsed(src, &Parse)`), the one formatter
  behind `strand fmt` and LSP formatting. It never changes a file's
  meaning: a file with syntax errors is `FormatError::Syntax(errors)`, and
  a result whose tree differs from the input's up to spans
  (`fmt::shape(&ast::File) -> String`, the tree's `Debug` with spans
  removed) is `FormatError::Unstable` rather than written. The output is
  idempotent, keeps comments and line breaks, and ends with one `\n`.

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
`strand check <file>` checks the file with the rest of its config (the
default config directory if the file is in it, else the file's own
directory, both through `find_files`) and reports only the diagnostics
with a label, primary or secondary, in that file (so both files of a
cross-file redeclaration report it).

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
and `spans` (byte ranges with weight, italic, underline or colour: marks,
markup); a glyph run's `color` is its span's, else the node's, and an
underlined span's run carries its `underline` rect (physical pixels). Glyph atlases are keyed by
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
  events included). The cursor is set on enter (`wp_cursor_shape_v1`, else the
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
  waits for its text, a later one for a new node's); a timer at the
  deadline asks again, and any
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
- A spec's `overhang` grows the layer size and moves the margins out
  (`placement::layer_config`), the exclusive zone grows by the overhang
  on its edge so margin + zone is unchanged, and the input region is the
  box inside it (`SurfaceInfo::input_region`; an OSD's is empty).
- The keyboard: one `wl_keyboard` per seat with xkbcommon keymaps and
  key repeat (`get_keyboard_with_repeat`), as `InputEvent::Key` on the
  surface with keyboard focus (`State::keyboard_focus`).
- Later (planned, so the current shape does not block them):
  - M2/M4: surface `exit` poses need the unmap delayed until exit
    settles: render holds `Removed`/`open: false` until its exit is done
    (or a `SurfaceHost::exit_done` hook); spec changes (compositor-animated
    margins) get applied with the next buffer commit when a paint is
    pending instead of a bare commit.
  - M4: `raw_handles(surface)` (display + `wl_surface`) for GPU promotion
    on the same surface; `State::recreate_all()` for `strand reload
    --hard`.

### `strand-dev`

The language server, `strand-dev lsp` (stdio), built on `lsp-server` and
`lsp-types`; the runtime binary never links it. `strand_dev::serve(&
lsp_server::Connection)` runs the protocol on any connection (tests drive
it in process over `Connection::memory()`) against the builtin schema;
`serve_with(&Connection, Arc<Schema>)` takes the schema to check with,
chosen once by the caller (the builtin schema extended with
`Schema::extend` by the service crates it links); `run_stdio()` serves
stdin and stdout, and `capabilities()` is what it advertises. It reads
only `strand-compiler`'s public interfaces: `compile_with` for one config
at a time against that schema, `hir::Program` (`refs`/`reference_at`,
defs, locals, `tokens`, the typed tree) for hover, definition, rename and
completion, `Schema::doc` (of the same schema) for completion and hover
text, `Diagnostic::suggestions` for quick fixes,
`schema::members_of` for what `x.` offers, `fmt::format` for
formatting, and `source::find_files` for which files a document is
checked with (the rule of `strand check <file>`: the default config
directory if the file is in it, else a workspace folder that is itself a
config, else the file's directory, else the file alone; see
`docs/decisions.md`, wave2-lsp). Open documents replace their files' text
on disk; files read from disk are re-read when their size or mtime (or a
scanned directory's) changes, and clients that can are asked to watch
`**/*.strand`. Positions are UTF-16 (the protocol default), sync is
full-document, diagnostics are published per config after a 200 ms
debounce (`initializationOptions.debounceMs`), at once on open and save,
and workspace edits use versioned `documentChanges` when the client
supports them. `initializationOptions.configDir` overrides the default
config directory.

The inspector joins it in M5; tree-sitter highlighting is not built yet
(see `docs/decisions.md`, wave2-lsp).

### `strand-services`

Specified when M3 starts. It only produces writes and events into
`strand-core`.

### `strand-watch`

Produces typed events, never parsed content; logic turns them into writes.
It does not depend on `strand-compiler` or `strand-core`.

- **One channel.** `strand_watch::channel() -> (EventSink, Receiver<
  ChangeEvent>)`; `EventSink` is `Clone + Send` and
  `.with_waker(Fn())` calls a waker after each send (a calloop `Ping` on
  the logic loop). `ChangeEvent` is `Files(FileBatch)`,
  `System(SystemBatch)` or `Compositor(CompositorEvent)`.
- **Files.** `Watcher::spawn(Option<ConfigWatch>, Options, EventSink)`
  runs the `strand-watch` thread (one thread: a raw inotify fd and a
  control eventfd under `poll(2)`; with no inotify instance, everything
  is polled). Dropping the `Watcher` stops it; `Watcher::join(self) ->
  thread::Result<()>` stops it and says whether the thread panicked (the
  binary's `live::Worker::join` joins the compiler worker, then the
  watcher, and `strand run` logs a panic of either; the persist store's
  own-write observer holds the watcher weakly, so the worker's handle is
  the last strong one).
  `ConfigWatch { root, modules: ModuleSet { files, dirs, errors,
  too_deep }, rescan }` is `source::find_files`'s `Discovery` (`files`,
  `dirs`, `errors` with each error as text, `too_deep`) plus a
  `FnMut() -> io::Result<ModuleSet>` the binary implements with
  `find_files`; the watcher calls it when a `.strand` name, a directory
  or a directory link appears or vanishes in a config directory, a
  directory link on the way is swapped, the config directory itself is
  replaced, or on a rescan. When `spawn` returns, every watch is in place
  and every module file's baseline hash was read after its watch:
  start the watcher, then load. The `modules` passed in were listed
  before the watches existed, so the watcher lists the set once more at
  the first quiet period and reports a module created in between as
  `Created` (nothing when the set is unchanged). Referenced paths come
  from the compiler: after each reload the loader calls
  `set_referenced(impl IntoIterator<Item = impl Into<Referenced>>)`,
  each item `(path, role)` or `(path, role, hash)` with `hash` the
  `hash_bytes` of what the loader read, for every `(path,
  Role::{Shader, Settings, Wallpaper, Other})` the program references;
  it replaces the loader's previous set (and only that), and a file that
  no longer holds the bytes its `hash` names (saved between the read
  and the call) is reported. `watch_file(path, role)` /
  `unwatch_file(path, role)` add or drop one ad-hoc registration
  (counted per path and role) for owners whose paths the compiler does
  not collect (the wallpaper owner: `prefs.wallpaper` is a runtime
  value); `set_referenced` never replaces them. `watch_file` is
  register, then read; `watch_file_loaded(path, role, hash)` is read,
  then register. Module-set membership is separate, so a
  module file registered for another role stays a module. Neither a
  referenced file nor its directory need exist yet. Cache sources come
  through `watch_tree(dir, depth, CacheKind::{Apps, Icons, Fonts})`.
  **Blocking:** `watch_file`, `set_referenced` and `watch_tree` wait for
  the watcher thread (watches synced; a new path given without a hash
  is read before they return, one given with a hash is compared later
  on the watcher thread); call them from the loader, the compile worker,
  boot or a service thread, never from logic. `watch_file_loaded`,
  `unwatch_file`, `register_own_write` and `rescan` return at once and
  are safe on the logic thread.
  `register_own_write(path, hash_bytes(&bytes))` before Strand writes a
  file (settings write-back) makes the matching write silent; the
  registration is in place when it returns, and it silences the write
  under every registered path that resolves to that file. Own writes
  should be atomic (temporary file renamed over the path). A file with a
  write in progress (`MODIFY` seen in a config directory, or a new file
  created and not yet closed, and no `CLOSE_WRITE` yet) is never read;
  after 5 s (`Options::stalled_write`) with no further event and no
  change to its modification time it is read anyway with
  `Notice::StalledWrite(path)`, and read again when it is closed. A file
  a write may have reached while it was being read (its stamp moved, a
  `MODIFY`, creation or removal for it queued by then, outside the config
  directories also a `CLOSE_WRITE`, or modified less than 15 ms before
  the read ended) is left out of the batch with its baseline unchanged
  and read at a later quiet period. A file put off like this is still
  read within 500 ms of the event that made it due, however often it is
  rewritten: from then on a recent modification time alone does not put
  it off. In config directories, where every write makes a `MODIFY`, no
  batch carries a torn read. Outside them writes make no event before
  the close, so an in-place writer that pauses for more than 15 ms
  mid-write can be read torn; its `CLOSE_WRITE` then reports the whole
  file in the next batch. Only config directories are watched with
  `MODIFY`; every other content directory (referenced files, symlink
  hops, cache trees) hears completed writes and names only, so writers
  there cost one wakeup per file closed. The config root's parent and
  the stand-in for a missing directory are watched for names only, and
  every ancestor of a watched directory holds a light watch (moves and
  deletions of its children only), so moving any directory on the way
  reports the files below as `Removed`.
  `rescan()` is `strand reload`.
- **`FileBatch { changes, rescan, notices, first_event, last_event }`.**
  One batch per quiet period: 15 ms after the last completed write
  (`CLOSE_WRITE`, `MOVED_TO`, a new symlink or hard link; 50 ms when the
  latest event removed a watched file; at most 500 ms after the first
  event). `changes` is sorted by path then role, each `FileChange {
  path, canonical, kind: Created|Modified|Removed, hash:
  Option<blake3::Hash>, role, error }`, one per role the path is watched
  for; unchanged hashes and own writes are dropped before sending, so
  every change is real. A `Modified` whose hash is unchanged means a
  link now resolves elsewhere: skip the recompile, update the path.
  `path` is the path as registered (module files as `find_files`
  returned them, under the config root even when a directory link points
  elsewhere); `canonical` is the resolved target. Only regular files are
  hashed; anything else (a FIFO, a device) has `error:
  Some(InvalidInput)`. Cache-tree entries are not hashed. `rescan` is
  `Some(Overflow | Requested)` for a full rescan; `notices` reports
  polled directories, rescan-callback failures, backend errors, stalled
  writes, and `Notice::ModuleSet { errors, too_deep }` when a rescan's
  diagnostics differ from the previous scan's (complete lists, so the
  overlay replaces what it shows). `first_event` and
  `last_event` (`Instant`) are the earliest and latest events behind the
  batch (for a file put off from an earlier batch, the events that made
  it due, not the flush that put it off); latency measurements use
  `sent − last_event` as the watcher's share of save-to-pixels.
- **System settings.** `strand_watch::follow(&zbus::Connection,
  EventSink)` is the async portal client; `strand-services` runs it on
  the shared tokio current-thread runtime and session connection.
  `PortalSettings::spawn(Bus::Session, sink)` runs the same on its own
  `strand-portal` thread and connection. It sends one `SystemBatch {
  settings, at_boot: true, received }` within `BOOT_READ_TIMEOUT` (500
  ms; empty when there is no portal; `on change` must not fire for it),
  then one batch per `SettingChanged`, plus `at_boot: false` batches for
  boot reads that came late and for a full re-read whenever the portal
  starts or restarts: `SystemSetting::Dark { dark, scheme }`,
  `Accent(Option<[f64; 3]>)`, `Contrast(Normal | High)`, each with
  `.path()` = `system.dark` / `system.accent` / `system.contrast`.
- **Compositor.** `CompositorEvent::ConfigReloaded { failed:
  Option<bool> }` is `wm.config_reloaded` (`None` from Hyprland, which
  does not say). The M3 Hyprland and niri adapters live in
  `strand-services` (design.md's services table lists them) and send it
  through a clone of the same `EventSink`, so `strand-services` depends
  on `strand-watch` for `EventSink` and `CompositorEvent`; `strand-watch`
  depends on neither `strand-services` nor `strand-core`.

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

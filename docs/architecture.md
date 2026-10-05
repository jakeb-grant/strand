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
| Services | `strand-services` | tokio current-thread runtime; PipeWire and toplevel get their own threads | Block logic: they send state diffs and events |

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s (`strand-scene`) and layout facts
(`self.width` for container queries). No locks are shared across threads on a
hot path.

`strand run [dir]` (`crates/strand/src/run.rs`) is this wiring: the main
thread's surface host forwards the monitor hooks (`screens` as a list of
plain `ScreenInfo`s, `monitor_forgotten` as `Forget(id)`), surface-level
input (`hover` and `pressed` as `Flag`, `click`/`secondary`/`scroll` as
`Event`) and surface sizes to the logic thread over a calloop channel
(`run::ToLogic`); the logic thread owns the runtime, `SchemaHost::real`
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
settings writes reach the disk before the process exits. Until render
hit-tests and lays out inside surfaces (M2), input and size facts
address the surface's node.

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
  and the body's writes on `d.timer`. Tasks woken from other threads (IO and
  D-Bus replies) are polled at the start of the next flush, never between
  sinks. Tasks that event listeners spawn (`on click`, `on
  notifications.received`) are polled as soon as the listeners ran,
  before the next sink, so a sink reading what such a handler writes
  synchronously runs after it, once.
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
  re-stamp; returns `Redeclared`), and for `@reset` / the overlay's
  `[reset]`, `persisted.reset(rt)` (cancels a pending or queued write,
  removes the file, sets the default). Warnings arrive as
  `Diagnostic::PersistDefaultChanged` / `Diagnostic::PersistFailed` (write
  failures in a later tick, with a wake-hook call; `rt.is_idle()` is false
  while one waits to be reported). `rt.shutdown()` waits (bounded) for
  queued writes.
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
  each other's writes in the same tick. Off-thread reads: see
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
  `with`/`with_untracked`/`get_key` instead of holding `KeyedVec` clones;
  `writes_to` answering `WriteEdge::Feedback` is not an error (a
  self-normalising handler is valid; only a static cycle among `let`s is a
  load error); for `let x = svc.call(input)` declare the input's reads on
  `memo.effect_id()`, for `on change … after T` the tracked reads on
  `d.effect` and the body's writes on `d.timer`;
  create persisted cells with an instance-qualified path and keep the
  `Persisted` handle for `redeclare` (reload) and `reset` (`@reset`);
  lower `state x from "file.toml" { typed fields }` to
  `rt.settings_file(&store, resolved_path, fields)` with one `FieldSpec`
  per field from the checked schema (the type's decode and encode over
  `toml_edit::Item`, the declared default), keep the `Settings` handle,
  call `reload` (or `reload_with`, see `strand-watch`) when the watcher
  reports the file, call `redeclare` when a reload changes the
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
  BTreeSet<String>`). Configs see contributed names as a prelude
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
  `RecordDef::doc` mirrors `DocKey::Type`. A parameter's default keeps its
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
- **Services** (`strand_compiler::vm::ServiceHost`): the VM's only way
  to services.
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
  - `acquire`/`release(service)`: a reader count. Every mounted
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
    leaves or goes invisible.
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
  kept: `Instance::reset(path)` is `@reset`. A keyed list `state` that
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
  diagnostics, notices }`: one diff per tick, the boot one starting
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
    to `keyed_memo`.
  - Reads of a keyed collection (`xs.len`, `.first`, `.last`, `xs[i]`,
    `xs.contains(x)` on a keyed `state` or a host's keyed field) are
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
    `NodeId`s; render produces them when hit testing lands (M2).
  - `get`/`set(path)` read and write exported `file.name` values and
    fields inside them (`theme.prefs.compact`; the CLI), `set` checked
    against the declared type. Dropping an `Instance` (or `shutdown`)
    disposes everything it mounted. `SceneMirror` applies diffs to a
    retained mirror, checks their consistency and renders it as text
    for snapshots.

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
  is polled).
  `ConfigWatch { root, modules: ModuleSet { files, dirs }, rescan }` is
  `source::find_files`'s `Discovery` (`files`, `dirs`) plus a
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
  it replaces all registrations, and a file that no longer holds the
  bytes its `hash` names (saved between the read and the call) is
  reported. `watch_file(path, role)` / `unwatch_file(path, role)` add or
  drop one (counted per path and role); `watch_file` is register, then
  read. Module-set membership is separate, so a
  module file registered for another role stays a module. Neither a
  referenced file nor its directory need exist yet. Cache sources come
  through `watch_tree(dir, depth, CacheKind::{Apps, Icons, Fonts})`.
  `register_own_write(path, hash_bytes(&bytes))` before Strand writes a
  file (settings write-back) makes the matching write silent; the
  registration is in place when it returns. Own writes must be atomic
  (temporary file renamed over the path): an in-place write can be read
  half done. Every ancestor of a watched directory holds a light watch
  (moves and deletions of its children only), so moving any directory
  on the way reports the files below as `Removed`.
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
  polled directories and rescan-callback failures. `first_event` and
  `last_event` (`Instant`) let latency measurements subtract the quiet
  period.
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

Settings files (fixed in wave 2 by `strand-core`): the watcher should not
read a settings file twice or parse it on the logic thread. For each
declared file it holds the `SettingsSources` from `Settings::sources()`
(`Send`, cheap to clone). On a change event, on its own thread: `let m =
sources.mark()` (Strand's own writes done or queued; must come *before*
reading the bytes), read the bytes, hash them (an unchanged hash, such as
Strand's own write, stops here), then `sources.read_from(m, Ok(text))`
(also reads the overlay and probes writability) and post the
`SettingsRead` to the logic thread, which calls
`settings.reload_with(rt, read)` and only decodes. Strand's temp files next
to a settings file are named `.<name>.tmp.<pid>.<n>` (renamed over the file:
the watcher sees `MOVED_TO` for the file itself); its scratch-name filter
should ignore that pattern, as it does editors' scratch names.

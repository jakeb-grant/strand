# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, token evaluation per frame, layout, damage, paint, presentation | Wait on the logic thread, run handlers, evaluate bytecode |
| Logic | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program | Touch Wayland or pixels |
| Compiler worker | `strand-compiler` | Parse, check, lower changed modules off-thread | Mutate live state (it hands a compiled `Program` to logic) |
| Text worker | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: render keeps the last layout until a new one arrives |
| Watcher | `strand-watch` | inotify, portal, IPC socket | Parse files (it sends paths and hashes) |
| Services | `strand-services` | tokio current-thread runtime; PipeWire and toplevel get their own threads | Block logic: they send state diffs and events |

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s and layout facts
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
     keep `wants_frame` true: the delivery does, through step 2. A
     surface that has never painted (or whose text worker restarted) holds
     its first frame while its text is shaped, at most 50 ms: if
     `frame_deadline(surface)` is `Some(t)`, arm a timer for `t` and check
     `wants_frame` again then.

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
  timers, listeners and tasks stop, state kept; service events are kept for
  its listeners, input events dropped) and the fixing reload calls
  `rt.resume(scope)`, or moves the live state out with `reparent` and
  disposes the frozen scope (held work is released either way). Reloaded
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
  Service events are `EventQueue`s.
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

### `strand-services`, `strand-watch`

Specified when their milestones start (M3, M1). Both only produce writes and
events into `strand-core`.

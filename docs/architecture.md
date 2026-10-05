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
  }
  pub trait Painter {
      /// Paint everything that changed for `surface` and return the damage,
      /// already widened to cover the buffer's age.
      fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage;
      /// True while a spring or time signal on this surface is unsettled;
      /// the surface manager requests frame callbacks only while true.
      fn wants_frame(&self, surface: SurfaceId) -> bool;
  }
  ```

- **Scene protocol** (logic → render, one batch per tick): `SceneDiff`
  holding ordered `SceneOp`s over a retained tree: `Create { id, kind,
  parent, index }`, `Remove { id }` (render plays `exit` before unmounting),
  `Move { id, parent, index }`, `SetProp { id, prop, value, transition }`,
  `SetTokens { table }`. Node ids are generational. Prop values are typed
  (`Length`, `Color`, `Paint`, `Text`, `Shadow`, ...). `transition` is
  `Default` (the token spring for that prop class), `Spring { .. }`,
  `Duration { .. }` or `Instant`, matching `~` in the language. `Create`/`Move`
  take `parent: Option<NodeId>` (`None` for surface roots) and
  `PropValue::Unset` reverts a prop to its default. Render maps a surface
  root to Wayland surfaces with `Renderer::attach_surface(SurfaceId, NodeId)`.

### `strand-core`

A fine-grained push-pull graph, generic over value types
(`T: Clone + PartialEq + 'static`): `Signal` (state), `Memo` (lazy derived),
`Effect` (observer at the edge, e.g. the scene emitter), batching into ticks,
generational handles where a stale read returns an error. Keyed collections
publish `VecDiff`s and incremental `filter/map/take/sort_by` keep keys.
`Async<T>` keeps its last value and exposes `pending`/`error`. The dynamic
`Value` used by the VM is defined by `strand-compiler`, not here.

### `strand-compiler`

`syntax` (lossless lexer and parser with spans and recovery), `check` (names,
types, did-you-mean), `lower` (bytecode), `vm` (evaluates bytecode against
`strand-core` signals), `reconcile` (old program + new program → identity map
→ `SceneDiff` and state migration). One crate serves runtime, `strand check`
and the LSP. The grammar is specified in `docs/grammar.md`.

### `strand-text`

Request/response over a channel: `TextRequest { key, text, style, max_width,
scale }` → `TextLayout { key, size, glyph runs }`. Glyph atlases are keyed by
scale and LRU-bounded. Render draws the last delivered layout.
Each `TextLayout` also carries the `AtlasUpload`s (alpha pixels) for glyphs
rasterised while producing it, which render applies to its mirror of the
atlas in arrival order, and leases on the atlas pages it uses, so the worker
never recycles a page a live layout draws from.

### `strand-surface`

Owns the Wayland connection with smithay-client-toolkit: layer-shell surfaces
per output, a 2–3 buffer shm pool per surface with buffer age,
`damage_buffer`, `set_opaque_region`, fractional scale + viewporter, frame
callbacks only while `Painter::wants_frame`, `wp_presentation` timing, output
hotplug (monitor identity = make + model + description), and input forwarded
as `InputEvent`s. Compositor-animated poses (alpha modifier, viewporter,
margins) are its job in M4.

### `strand-services`, `strand-watch`

Specified when their milestones start (M3, M1). Both only produce writes and
events into `strand-core`.

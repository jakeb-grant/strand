# Decisions

Interpretations of ambiguous points in `design.md` and `architecture.md`.
Each track appends under its own heading.

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
  commit so the ack takes effect. A paint that returns no damage while
  `wants_frame` stays true arms a timer at `frame_deadline` if the host
  gives one, else requests a callback with a bare commit. Presentation
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
  Numbering of identical monitors follows plug order.
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


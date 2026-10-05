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
  shorthands (`pad`, `margin`, `radius`) arrive expanded as `Insets` /
  `Corners`; `NodeKind` has `start`/`center`/`end` for `split`, and names
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
  scale, so text never blanks.
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
  vello_cpu has only `std` + `f32_pipeline`, single-threaded.
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

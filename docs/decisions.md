# Decisions

Interpretations of ambiguous points in `design.md` and `architecture.md`.
Each track appends under its own heading.

## render

- 2026-10-04 · render: `Damage::area()` is the exact area of the union of
  its rects (overlaps counted once); "the pair whose union grows area least"
  is the pair minimising `area(bbox(a, b)) - area(a ∪ b)`. After a merge,
  rects the merged box covers are dropped.
- 2026-10-04 · render: `Color::lerp_oklab` interpolates premultiplied OKLab
  (CSS Color 4), so fades to transparent keep their hue; `t` is not clamped
  so overshooting springs extrapolate. sRGB transfer is extended
  sign-symmetrically so out-of-gamut colours round-trip; gamut mapping is
  left to the M2 token evaluator.
- 2026-10-04 · render: scene protocol additions not spelled out in the
  contract: `PropValue::Unset` reverts a prop to its default (instead of a
  sixth op); `SceneOp::Create`/`Move` take `parent: Option<NodeId>` with
  `None` for surface roots; `Transition::Duration` carries an `Easing`
  (`~ ease(..)`, `~ bezier(..)`); `TokenTable` holds resolved values keyed
  by path, since per-frame token evaluation on the render thread is M2
  (superseded 2026-10-05: token references, below).
  Shorthands (`pad`, `margin`, `radius`) arrive expanded as `Insets` /
  `Corners`. `NodeKind` adds `start`/`center`/`end` for `split` regions.
- 2026-10-04 · render: the glyph atlas is LRU-bounded per page, not per
  glyph: each scale has up to `max_pages` (default 4) square 256 px
  alpha pages packed with shelves; when full, the least recently used page
  is reset. Layouts hold a lease on every page they draw from and leased
  pages are never reset (the atlas briefly exceeds `max_pages` instead), so
  the render thread can keep drawing its last layout safely. Atlas pixels
  travel to render as `AtlasUpload`s inside each `TextLayout`; render must
  apply them in order even for layouts it discards.
- 2026-10-04 · render: glyphs are rasterised unhinted at quarter-pixel
  horizontal positions and whole-pixel baselines; colour glyphs (emoji) are
  skipped until the atlas grows a colour format. Tests use a vendored
  Liberation Sans (SIL OFL 1.1) with system fonts off, since CI has no
  DejaVu.
- 2026-10-05 · render: until taffy lands (M2), layout is absolute: a node
  sits at `x`/`y` inside its parent, sized by `width`/`height`/`size`
  (lengths in px or % of the parent); text nodes size to their shaped
  layout; a surface root fills its buffer. `font` and `color` already
  inherit. `Remove` unmounts at once (exit poses are M2); springs are M2,
  so props snap, but each prop keeps the `Transition` it was set with.
- 2026-10-05 · render: damage is per node: each frame every node gets a
  record of its physical bounds (ink, shadows included, clipped by
  ancestors) and a signature hashing its paint items plus everything
  inherited that changes how it paints (ancestor opacity, clips, and a
  move epoch for paint order). Changed records damage old and new bounds.
  Buffer age widens by the last `age - 1` frames (history of 4); age 0 or
  older buffers, resizes and scale changes repaint in full. Only a `paint`
  that returns non-empty damage is a frame (the caller commits it); an
  empty one records nothing (superseded 2026-10-05, below).
- 2026-10-05 · render: vello_cpu writes RGBA but `wl_shm` ARGB8888 is BGRA
  in memory, so every colour is handed to vello with red and blue swapped
  (compositing is per channel; gradients interpolate in sRGB to keep that
  valid) and vello renders straight into the shm buffer. Damaged pixels
  are cleared, split into disjoint rects, and the scene is drawn
  source-over once per rect under `push_clip_rect`. The f32 pipeline is
  used because the u8 one rounds ±1 differently where a shape crosses a
  clip edge, which would make partial repaints differ from full ones.
  Cost: vello_cpu 0.3 still loads and packs every row of the buffer, so a
  clock tick on a 2560×36 bar takes ~0.49 ms (u8 would be ~0.06 ms; full
  repaint ~0.51 ms; see `benches/clock_tick.rs`). Revisit with a vello
  that skips command-free regions, or accept ±1 and use u8, in M2.
- 2026-10-05 · render: shadows follow CSS box-shadow: blur radius is twice
  the Gaussian sigma and the shadow is clipped away under the casting box
  (so translucent surfaces do not darken). Borders are drawn inside the box
  with their width rounded to whole physical pixels (at least 1). Rounded
  corners shrink like CSS when they would overlap, so `radius: full` is a
  pill. A text layout shaped for another scale is drawn resampled until the
  re-shaped one arrives.
- 2026-10-05 · render: text state is kept per node *and* scale, so one
  tree shown on outputs of different scales holds a sharp layout for each
  instead of re-shaping back and forth. Scene edits and text deliveries
  only mark the surfaces whose root they touch as dirty (`SetTokens` marks
  all), so other outputs request no frame callbacks.
- 2026-10-05 · render: buffer age is counted in commits. `Painter::paint`
  returning empty damage is not a frame: it is not pushed into the damage
  history and the caller must not commit (or count) it. Every non-empty
  result must be committed; `Renderer::invalidate` covers a buffer that
  could not be. Returned damage is clipped to the buffer.
- 2026-10-05 · render: tokens travel unresolved. `PropValue::Token`
  holds a `TokenExpr` (path, colour method, `oklch(from …)` with channel
  arithmetic, or a `Template` filling the colours of a composite value in
  `PropValue::colors_mut` order). `TokenTable` holds plain values plus
  derived expressions; render evaluates at flatten time (M0 snaps; M2
  springs the roots). `.alpha(a)` sets alpha to `a`; `.mix(c, t)` lerps
  premultiplied OKLab by `t` (`8%` = 0.08); `.lighten/.darken(d)` move
  OKLCH L by `d`. Gamut mapping clips per channel until M2. An unresolvable
  reference reads as unset. Non-colour token props (`$space.2`) are also
  references; `Transition::Default` resolves through `$motion.*` tokens
  holding `PropValue::Transition`. Subtree overrides (`set { … }`) are not
  in the protocol yet.
- 2026-10-05 · render: `enter`/`exit` are props holding a
  `PropValue::Pose` (or a preset keyword like `popin`); render stores them
  and M2 plays them. The protocol now names every prop and node kind in
  the design catalogue (`fill`, `text_stroke`, `blur_fallback`, `arc`,
  `graph`, `pages`, `letters`, …) so the compiler and LSP share one table;
  render ignores what it does not draw yet.
- 2026-10-05 · render: `PaintTarget` carries `time`, the predicted
  presentation time on the `wp_presentation` clock; `Painter` gained
  `opaque_region` (default empty). Render reports the root's opaque
  background minus its corner squares when the root has no opacity and
  an opaque `bg`.
- 2026-10-05 · render: rasterisation runs in a fixed grid of 256×64 cells
  (256 = one vello wide tile); only cells the damage touches are
  rendered, each by a cell-sized context with the scene translated by the
  cell origin, then the damaged pixels are copied back. The fixed grid
  keeps partial repaints bit-identical to full ones. Clock tick: 0.12 ms on
  2560×36, 0.18 ms on 3840×2160 (was 0.48 / ~40 ms). Opacity and clip
  groups carry their subtree's bounds and are skipped outside the damage.
  vello_cpu is built with only `std` + `f32_pipeline` and contexts are
  created with `num_threads: 0`.
- 2026-10-05 · render: gradients interpolate in premultiplied OKLab:
  each segment is sampled into 8 stops, between which vello interpolates
  per channel (keeping the red/blue swap valid). Conic gradients are a full
  sweep from +x turned by the paint transform to `from` (CSS: clockwise
  from the top). Shadows with differing corner radii are drawn per
  quadrant, each with its corner's radius, split on whole pixels; this
  approximates a true per-corner blur where corners are within about 3σ
  of each other. Dithering stays M2.
- 2026-10-05 · render: robustness against values from user expressions.
  Non-finite numbers read as unset; lengths and offsets clamp to ±1e6
  logical px, shadow blur to 1000. Text shaping replaces non-finite or
  non-positive font sizes with the default, caps them at 1024 physical px
  (`MAX_FONT_PX`), drops non-finite wrap widths and line heights. The text
  worker catches a panicking request, answers with an empty layout and
  restarts its engine. `Create` with an index more than 65,536 past the
  live slots is rejected (`SceneError::InvalidId`). Rect and damage edge
  arithmetic saturates.
- 2026-10-05 · render: glyph masks too big for a 256 px page get a page
  of their own, sized to the mask rounded up to 64 px (at most 2048),
  leased and evicted like any other; failed allocations are not cached.
  Pages allocated past `max_pages` while all were leased are freed once
  unleased (the render mirror keeps their last pixels until the scale is
  dropped). Text state, requests and atlases of scales no surface uses
  are freed on detach and on rescale, and the worker's atlas is dropped
  with the mirror so a replugged output re-uploads its glyphs. Superseded
  and reverted requests are cancelled; the worker skips cancelled ones
  still queued. Text pending on the worker no longer requests frames:
  delivery marks the surface dirty.

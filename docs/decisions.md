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
  by path, since per-frame token evaluation on the render thread is M2.
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
  older buffers, resizes and scale changes repaint in full. Every `paint`
  call counts as a committed frame.
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

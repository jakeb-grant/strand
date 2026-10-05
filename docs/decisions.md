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

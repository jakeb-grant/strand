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

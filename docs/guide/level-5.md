# Level 5: your own theme, services and effects

Outline (M5 wave 2). The text follows; until then this page lists what
the level teaches and in which section.

You learn: `tokens`, spring overrides `~`, `service from dbus`,
`pages`, drag and drop, shaders; also `set { }`, `keyframes` and
`play`, `canvas`, `type` and `key`, `slot` and `fn` (README, "Every
construct and its level").
You build: a theme file, a control centre, a dock and a custom service.

## Sections

1. **A theme file** (`theme.strand`): the three token tiers; `tokens
   base { … }`, `extends` and `override`; `use tokens …, palette …`;
   palettes from a seed colour, a wallpaper (`material(image:)`) or an
   import (Catppuccin, base16); a theme switcher with `enum` and
   `match`; `strand set theme.look mocha`; `strand export gtk | kitty |
   hyprland`.
2. **Overrides for a subtree**: `set { $surface: … }`; component tokens
   (`component Toast(n) tokens { … }`, `$Toast.radius`).
3. **Motion**: spring overrides with `~` (`~ $motion.bouncy`,
   `~ 200ms`, `~ instant`); `keyframes` and `play`.
4. **A control centre**: `pages`; `slot` for a tile that wraps its
   content; a pure `fn` helper.
5. **A dock**: `type Pin { … }` and `state pins: [Pin] key app = []`;
   `key` on your own data; drag and drop (`drag:`, `on drop`).
6. **A custom service**: `service … from dbus`, checked against the
   bus's introspection; `from file`, `from listen`, `from poll` and
   `permit exec`.
7. **Escape hatches**: `shader "x.wgsl" { … }`; `canvas { draw: … }`.
8. **Plugins**: a Rust service built into your own `strand` binary
   (written by S-plugins, M5 wave 2).

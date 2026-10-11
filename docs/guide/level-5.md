# Level 5: your own theme, services and effects

Outline (M5 wave 2). The text follows; until then this page lists what
the level teaches and in which section.

You learn: `tokens`, spring overrides `~`, `service from dbus`,
`pages`, drag and drop, shaders, `file`; also `set { }`, `keyframes` and
`play`, `canvas`, `type` and `key`, `slot`, `fn`, settings files,
SVG layer selectors, and `service … from file | listen | poll` with
`permit exec` (README, "Every construct and its level").
You build: a theme file, a control centre, a dock and a custom service.

## Sections

1. **A theme file** (`theme.strand`): the three token tiers; `tokens
   base { … }`, `extends` and `override`; `use tokens …, palette …`;
   palettes from a seed colour, a wallpaper (`material(image:)`) or an
   import (Catppuccin, base16); a settings file
   (`state prefs from "prefs.toml" { … }`) for the accent and the
   wallpaper, edited by hand or by `<->`; a theme switcher with `enum` and
   `match`; `strand set theme.look mocha`; `strand export json | base24
   | --template <file>`, and a `file` element that keeps a neutral CSS
   custom-properties file in step with the theme (`file
   "~/.cache/strand/colors.css" { template: "colors.css.tmpl" }`, its
   template holes such as `--accent: {{ $accent }};`, and `run:` under
   `permit exec`). Strand ships no per-app templates, so the example is
   neutral (decisions.md m5-tools). The `file` element and `strand
   export` land in M5 wave 2, with this level's text.
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
7. **Escape hatches**: `shader "x.wgsl" { … }`; `canvas { draw: … }`;
   an SVG with bindable layers (`svg "icon.svg" { #needle { … } }`).
8. **Plugins**: a Rust service built into your own `strand` binary
   (written by S-plugins, M5 wave 2).

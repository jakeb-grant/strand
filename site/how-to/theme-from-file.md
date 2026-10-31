# A theme from a file

Take the shell's colours from a file another program writes, and follow
it when the file changes. Any tool that can write a file can theme your
shell this way, in whatever names it likes, and Strand's own widgets
follow along.

## The files

The file holds colours under names of its own:

```json
{ "background": "#101014", "foreground": "#e6e6e6", "highlight": "#7aa2f7" }
```

A service reads it, and `palette(…)` says which name is which role:

```strand file=theme.strand
// theme.strand: the palette comes from a file
service colors from file "~/.config/colors.json" {
  background: color?; foreground: color?; highlight: color?
}
use palette palette(
  from: material(seed: #7aa2f7, dark: true),
  surface: colors.background,
  fg: colors.foreground,
  accent: colors.highlight,
)
```

A bar to see it on:

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { box { size: 12; radius: full; bg: $accent } }
    center { text clock.format("%H:%M") }
  }
}
```

## How it works

- **`service … from file`** reads the file again each time it changes.
  A program should write a new file and rename it over the old one, so
  nothing reads it half written.
- **`color?`** says the colour may be missing, so a missing or broken
  file gives `null` rather than an error.
- **`palette(…)`** turns the colours into a palette. A role left out or
  null comes from `from`, which is also what you get if the file goes
  away.
- A role you give brings the roles worked out from it, so `$accent`'s
  `$on_accent` and containers stay in step, and the contrast guard
  applies as to any palette.
- When the file changes, it is a swap like any other: every `$` token
  springs to its new value, with no spring to write yourself.

## Variations

- **No fallback palette.** Without `from`, Strand works out the roles
  you leave out, and with no colours at all the result is null, so `use
  palette` needs one after `??`:
  `use palette palette(fg: colors.foreground) ?? material(seed: #7aa2f7)`.
- **One colour of your own.** `palette(from: import("catppuccin:mocha"),
  accent: #ff5500)` changes one colour of a scheme.
- **A theme you can change from the shell.** Colours can come from a
  `state` as well as a service, so a colour picker is `palette(accent:
  chosen)`.
- **Colours beyond the palette's roles.** Read them straight from the
  service, or name them in `tokens`.

# A theme from your wallpaper

Take every colour of the shell from your wallpaper, and follow it when
the wallpaper changes. Strand reads the picture, builds a Material 3
palette from it, and springs every colour on screen to the new palette.
For the whole theme file, with token sets and a look switcher, see the
guide's [level 5](../../docs/guide/level-5.md#1-a-theme-of-your-own).

## The files

```strand file=theme.strand
// theme.strand: the palette comes from the wallpaper
state prefs from "prefs.toml" {
  wallpaper: path = "~/Pictures/wall.jpg"; accent: color = #7aa2f7
}
let scheme: Palette = material(image: prefs.wallpaper, dark: system.dark)
                      ?? material(seed: prefs.accent, dark: system.dark)   // until the picture is read
use palette scheme
file "~/.cache/strand/accent.txt" { text: $accent }   // kept in step with every switch
```

A bar to see it on:

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { box { size: 12; radius: full; bg: $accent } }
    center { text clock.format("%H:%M") }
    end { text pct(battery.percent) { color: $fg.muted } }
  }
}
```

Point the setting at your wallpaper. `prefs.toml` is in your config
directory and is plain TOML; Strand makes it the first time a value is
written:

```toml file=prefs.toml
wallpaper = "~/Pictures/mountains.jpg"
```

or, from a terminal or a wallpaper script:

```sh
strand set prefs.wallpaper ~/Pictures/mountains.jpg
```

Here the bar starts in a purple picture's colours; then the setting
points at another picture, and every colour springs to its palette:

```example wallpaper-theme
```

## How it works

- **`material(image: …)`** builds a palette from a picture. It
  quantises a 128 px downscale of the image away from the logic thread,
  and caches the result by the file's content, so the same picture is
  not quantised twice.
- **It is an `Async`.** Reading the picture takes a moment, so its
  value is not there at first. `?? material(seed: …)` is the palette to
  use until it is. While a new picture is read the old palette holds,
  so the screen never flashes other colours.
- **The file is watched**, and so is the target of a link to it. A
  script that writes a new picture over it, or swaps the link, re-themes
  the shell; so does pointing `prefs.wallpaper` at another file.
- **`use palette scheme`** makes it the palette in force. With no
  `tokens` of your own, the built-in spacing, radii, fonts and springs
  stay as they are.
- **The last palette is kept** across restarts, so the shell starts in
  your wallpaper's colours rather than the default ones.
- **`dark: system.dark`** follows your desktop's light or dark setting.

When the palette changes, the colours spring to the new one in OKLab,
and the shell's state stays as it was: an open launcher keeps its
query.
[Tokens and theming](../concepts/theming.md#how-a-swap-animates) says
how a swap animates.

## Your colours for other programs

The `file` line writes your accent to `~/.cache/strand/accent.txt` as
`#rrggbb` and rewrites it after every switch, once, with the new
palette's value. A `file` with a `template:` writes a whole stylesheet
or config; `strand export` writes the palette once:

```sh
strand export json                                   # the palette as data
strand export base24 -o ~/.cache/strand/base24.yaml  # for base16/base24 templates
```

Strand ships no per-app themes or templates: these are generic
outputs, and the community's base16 and base24 templates work with the
`base24` export. The guide's
[level 5](../../docs/guide/level-5.md#2-your-colours-for-other-programs)
writes a CSS file.

## Variations

- **Another flavour of palette.** `material(image: …, variant: vibrant)`
  picks another Material 3 scheme: `tonal_spot` (the default),
  `vibrant`, `expressive`, `neutral`, `monochrome`, `fidelity`,
  `content`, `rainbow` or `fruit_salad`.
- **More contrast.** `contrast: system.contrast` follows the desktop's
  contrast preference, 0 for normal and 1 for high.
- **A published scheme instead.** `import("catppuccin:mocha")` is a
  palette too, and so are base16 and base24 schemes, matugen output and
  W3C design-token files.

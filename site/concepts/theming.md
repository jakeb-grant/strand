# Tokens and theming

Colours, spacing, radii, fonts, shadows and springs are **tokens**,
read as `$name`: `bg: $surface`, `pad: $space.3`, `radius: $radius.lg`.
A theme gives every token its value. Change the theme and everything
that reads a token follows, animated, with nothing rebuilt.

## Three tiers

| Tier | What it holds | Examples |
| --- | --- | --- |
| Palette | The colour roles: Material 3's roles one to one, plus 13 hue roles | `$surface`, `$fg`, `$accent`, `$on_accent`, `$error`, `$green` |
| Base tokens | Scales, and roles derived from the palette | `$space.2`, `$radius.lg`, `$font.ui`, `$motion.spatial`, `$fg.muted` |
| Component tokens | Knobs a component of yours exposes | `$Toast.radius` |

The [token reference](../../docs/reference/tokens.md) lists every
palette role and base token.

**Hue roles** are colours that must stay recognisable: `$red`,
`$orange`, `$yellow`, `$green`, `$cyan`, `$blue`, `$magenta`, and
`$bright_red` through `$bright_magenta`. A scheme that has these hues
gives its own; for one that does not, each is the plain hue moved a
little toward your accent.

## The built-in theme

With no theme of your own, Strand uses its built-in one: its base
tokens, and the palette

```strand fragment
material(seed: system.accent ?? #7aa2f7, dark: system.dark, contrast: system.contrast)
```

so the shell follows your desktop's accent colour, light or dark
setting and contrast preference, live.

## A theme file

A theme is ordinary declarations in any file. This one switches
between two looks and makes spacing tighter on demand:

```strand file=theme.strand
// theme.strand
enum Look { auto, mocha }
export state look = auto persist
export state compact = false persist
let scheme: Palette = match look {
  mocha => import("catppuccin:mocha"),
  auto  => material(seed: system.accent ?? #7aa2f7, dark: system.dark),
}
tokens roomy { space { 1: 4px; 2: 8px; 3: 12px; 4: 16px } }
tokens tight extends roomy { override space { 1: 2px; 2: 4px; 3: 8px; 4: 12px } }
use tokens compact ? tight : roomy, palette scheme
```

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.2; align: center
    box { size: 12; radius: full; bg: $accent }
    text clock.format("%H:%M")
    spacer {}
    text pct(battery.percent) { color: $fg.muted }
  }
}
```

- **A palette is a value.** `material(seed: …)` builds every role from
  one colour, as Android's Material You does; `material(image: …)` from
  a picture ([a theme from your wallpaper](../how-to/wallpaper-theme.md));
  `import("…")` reads a published scheme: Catppuccin, base16 and base24
  schemes, matugen output and W3C design-token files. Every source
  fills every role, and a role a source lacks is derived.
- **`tokens name { … }`** declares a token set. A set that names only
  some base tokens, as `roomy` names only `space`, takes the rest from
  the built-in theme. `extends roomy` starts a set from another one,
  and changing a token it inherits needs the word `override`, so a
  misspelt name is an error, never a new token nobody reads.
- **`use tokens …, palette …`** picks the set and the palette in force.
  Both are expressions, so the choice is live: `strand set theme.compact
  true` tightens the spacing, and `strand set theme.look mocha` changes
  every colour.

A token can be an expression over others, and it stays one:
`fg.muted: $fg.alpha(0.65)` is re-evaluated whenever `$fg` changes,
every frame of a palette change included. Colours have the methods
`alpha`, `mix`, `lighten` and `darken`, and `oklch(from $c, l: l +
0.12)` derives a colour from another's channels.

A raw hex colour in a prop is a warning: use a token, so the theme can
change it. Defaults in settings files and `material(seed:)` are
exempt.

## Overrides for part of a tree

`set { … }` as an item of a node overrides tokens for that node and
everything inside it:

```strand fragment
col { set { $surface: $surface.mix($accent, 4%) }
  // every node in here sees the tinted surface,
  // and $surface.hi is derived again from it
}
```

On the right of a `set`, `$surface` is the value from outside, so this
is not a loop.

A component can expose tokens of its own:

```strand fragment
component Tile(label: text) tokens { radius: $radius.lg; pad: $space.3 } {
  text label { radius: $Tile.radius; pad: $Tile.pad }
}
```

`set { $Tile.pad: $space.2 }` around some tiles makes those tighter
without editing `Tile`.

## How a swap animates

Choosing another palette, a desktop setting changing, a new wallpaper,
`strand set` and saving a theme file all take the same path:

1. The tokens that depend on what changed are resolved again.
2. Only the palette's roles spring to their new values, in OKLab.
   Every derived token is computed from them each frame, so it is
   exact at every step of the animation.
3. Text keeps a contrast of at least 3:1 against its background
   throughout. Where a swap between light and dark makes that
   impossible, the surface takes one picture of its old frame and
   crossfades from it instead.
4. What cannot be animated, such as a font, changes at once.
5. Files a `file` element writes are written once, with the new
   values, never with the in-between ones.

Here a bar switches from Catppuccin Latte to Mocha and back. The
surface, the text, the accent dot and the muted text, which is derived
from `$fg`, move to the new palette together; the pictures for light and dark mode
are the same, since an imported scheme is light or dark by itself.

```example swap
```

Nothing remounts: an open launcher keeps its query and a calendar its
month. The last palette is kept, so the shell starts in it rather
than flashing the default colours first.

## Your colours for other programs

`file "~/.cache/strand/accent.txt" { text: $accent }` keeps a file in
step with the theme, and `strand export json`, `strand export base24`
and `strand export --template` write the palette for other programs.
Strand ships no per-app themes; the guide's
[level 5](../../docs/guide/level-5.md#2-your-colours-for-other-programs)
shows both.

## Trying a colour

`strand set '$accent' '#ff5555'` sets a token for the running shell
only, over what the theme says, and `strand clear '$accent'` takes it
off. Quote both: a shell reads `$accent` as a variable and `#` as a
comment.

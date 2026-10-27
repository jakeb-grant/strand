# Layout

Strand lays nodes out with flexbox, as the web does: containers place
their children in rows and columns, and sizes, growth and alignment
are props. If you know CSS flexbox, most of this is familiar.

## Containers

| Container | Places its children |
| --- | --- |
| `row` | Side by side, left to right |
| `col` | One under another |
| `stack` | Over each other, in one cell its size |
| `grid` | In a grid of `columns:` columns |
| `split` | In three parts, `start`, `center` and `end`, with `center` centred on the whole whatever the others hold |
| `scroll` | In a view that scrolls along its `axis:` |
| `list` | In a column that scrolls, laying out and mounting only its visible rows |

`spacer` takes the space left in its row or column, and `box` is a
plain box to style. A surface, a `box` and a `popup` lay their children
out as a `stack` does.

```strand file=bar.strand
bar Top {
  edge: top; height: 36
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      for ws in workspaces.all {
        box { size: 8; radius: full; bg: $fg.faint
          when ws.focused { width: 20; bg: $accent }
        }
      }
    }
    center { text clock.format("%H:%M") }
    end { gap: $space.2
      text pct(battery.percent)
      text pct(audio.sink.volume) { width: 4ch; align: end }
    }
  }
}
```

The clock stays in the middle of the bar however wide the two sides
are, and the focused workspace's dot grows to a pill. Here the focus
moves from workspace 1 to 3, then to 2:

```example layout
```

## Sizes

- **`width`, `height` and `size`** take logical pixels (`36` is
  `36px`), a percentage of the parent (`40%`) or `ch`, the width of a
  character in the node's font (`4ch`). Unset, or `auto`, a node is
  the size of what it holds.
- **`min_width`, `max_width`, `min_height` and `max_height`** bound
  it: `max_height: 420` on a list makes it scroll past 420 px.
- **`pad`** is space inside a node's edge and **`margin`** space
  outside it. Both take one to four values, as in CSS: `pad: 0,
  $space.3` is none above and below and `$space.3` at the sides.
- **`gap`** is the space between a container's children.

A text that does not fit is cut with `ellipsis: end`, or wraps up to
`max_lines`.

## Sharing space: grow and shrink

- **`grow: 1`** gives a child a share of the space left in its row or
  column. Two children with `grow: 1` split it evenly.
- **`shrink`** says how much a child gives up when its row or column is
  too small: 1 by default, and `shrink: 0` keeps its size. An `image` or
  `icon` with a size in pixels or `ch` does not shrink unless you say
  so, so icons keep their size in a crowded bar.
- **`wrap: true`** on a `row` or `col` runs its children on into
  another line when one is full.

## Alignment

- **`align`** places a container's children on its cross axis (up and
  down in a `row`, across in a `col`): `start`, `center`, `end`,
  `stretch` or `baseline`. On a `text` it aligns the text's lines.
- **`justify`** shares the main axis among them: `start`, `center`,
  `end`, `space_between`, `space_around` or `space_evenly`.
- **In a `stack`**, `align` places every child on both axes. A child
  fills the stack on an axis where it has no size of its own, and
  sits at the start of one where it has.

There is no alignment for one child alone. Put it in a `row` or `col`
that places it (`row { justify: end; … }`), or take it out of the flow
with `place: absolute` and place it with `x` and `y`.

## Container queries

`self.width` and `self.height` are the node's own laid-out size, so a
node can change with the room it has:

```strand fragment
row { when self.width < 300 { gap: $space.1 } … }
```

A query switches with 4 px of slack, so a size hovering on the line
does not flicker.

## What lays the tree out again

Layout runs only when something that affects it changes: a size, a
`pad`, a text, a node added or removed. Then:

- **Paint-only props never cause layout.** `x`, `y`, `scale`, `rotate`
  and `opacity` move or fade what is already laid out, so animating
  them costs only painting.
- **A size that springs** lays out again only the part of the tree under
  the nearest ancestor whose size does not change, on each frame of the
  spring.
- **A surface the size of what it holds** (a panel, an OSD, a popup)
  springs its own size when that changes, growing and shrinking alike.
- **A `list`** lays out only the rows in its view, however long it is.

[Motion](motion.md) says how those changes animate, and
[performance](performance.md) what they cost.

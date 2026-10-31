# A morphing island

A pill at the top of the screen that grows into a card when you rest on
it, and shrinks back when you leave. It takes no popup and no second
copy of the pill: one box holds the pill's row, and while it is open the
card too, and the box is always the size of what it holds.

## The file

```strand file=island.strand
// island.strand: one box, and what it holds decides its size
let morph = spring(620, 0.92)

panel Island {
  anchor: top; margin: 6, 0, 0
  size: auto ~ morph

  state open = false
  on change island.hover after 120ms { if island.hover { open = true } }
  on change island.hover after 220ms { if !island.hover { open = false } }

  col { id: island; clip: true; bg: $surface; radius: (open ? 22 : 14) ~ morph
    row { height: 28; pad: 0, 14; align: center; shrink: 0
      text clock.format("%H:%M") { font: $font.mono }
    }
    if open {
      col { width: 320; shrink: 0; gap: $space.2; pad: 4, 14, 14, 14
        enter { opacity: 0 }
        text clock.format("%A %d %B") { weight: 600 }
        slider { value: <-> audio.sink.volume }
      }
    }
  }
}
```

## What it looks like

The pointer rests on the pill, then leaves. The box springs out and down
to fit the card, and back when the card goes.

```example island
```

## How it works

- **`size: auto ~ morph`** makes the panel the size of what it holds, on
  the `morph` spring. Nothing here sets a size for the box: the 320 is
  the card's width and the 28 is the row's height, and the box follows
  whichever row and card it holds.
- **`if open { … }`** is the whole morph. Adding the card makes what the
  box holds bigger, so the box springs to fit, on both axes at once;
  removing it does the same in reverse.
- **`clip: true`** keeps the card inside the box's edge, so the growing
  edge uncovers the card instead of the card spilling past it.
- **`width: 320; shrink: 0`** on the card keeps it laid out at its full
  size all the way, so its text does not rewrap while the box grows.
- **`enter { opacity: 0 }`** fades the card in as the box grows. It has
  no `exit { height: 0 }` on purpose: an `exit { height: 0 }` collapses the card first and
  the box's width only follows once it has gone, which reads as two moves.
- **`on change island.hover after …`** waits before it opens and closes,
  so passing over the pill does nothing. The two delays read the same
  value, and each handler checks it again when its wait ends.

## Why the surface stays still

A panel anchored to one edge, like this one, takes the whole width of the monitor from its first frame, and only the box takes the pointer, so the rest of that width lets clicks through to what is below. It also keeps the tallest height it has needed, across restarts, so after the first open under a config the box moves inside a surface that does not resize, and a compositor that animates resizes has nothing to animate.

## Variations

- **On every monitor.** `screens: all` on the `panel` gives each monitor
  its own box and its own `open`.
- **Windows keep clear of it.** `exclusive: 28` reserves room for the pill
  alone, so an open card covers windows instead of moving them.
- **A livelier shape.** A `spring` with a lower ratio overshoots; the
  [motion page](../concepts/motion.md) shows the choices.

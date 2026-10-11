# Level 1: the hello bar

You learn: surfaces, nodes, props, and services as live values.
You build: a bar on every monitor with the focused window's title, a
clock and the battery.

## Make a shell

```sh
strand new
```

This writes one file, `bar.strand`, into `~/.config/strand` (or
`$XDG_CONFIG_HOME/strand` if you set it). It refuses a directory that
already has files in it. Here is what it wrote:

```strand level=1 file=bar.strand
// bar.strand: a bar on every monitor. Every .strand file in this
// directory is part of your shell; save one and the change is live.
// Next steps: https://github.com/jakeb-grant/strand/blob/main/docs/guide/README.md
bar Top {
  edge: top; height: 32
  split {
    start  { text windows.focused?.title ?? "" }
    center { text clock.format("%H:%M") }
    end    { text pct(battery.percent) }
  }
}
```

Start it:

```sh
strand run
```

A bar appears along the top of every monitor. Plug in another monitor
and it gets a bar too; unplug it and that bar goes. You wrote nothing to
make that happen: a `bar` is on every monitor by default.

Leave `strand run` running for the rest of the guide. You never restart
it. Every save is applied to the running shell.

## Read it

**Lines starting with `//` are comments.** They run to the end of the
line.

**`bar Top { … }` is a surface.** A surface is something the compositor
shows: a bar, a panel, an on-screen display. `bar` is the kind, `Top` is
the name you give it, and the braces hold everything in it.

**`edge: top; height: 32` are props.** A prop is `name: value`. It ends
at a `;` or at the end of the line, so these two lines mean the same:

- `edge: top; height: 32`
- `edge: top` on one line, `height: 32` on the next.

Names are written in snake\_case: `max_width`, never `max-width` (that
would be `max` minus `width`).

**`split`, `start`, `text` are nodes.** Every node has the same shape:

```
kind [positional] { props; children }
```

The positional is one value written right after the kind, and the block
is optional. `text clock.format("%H:%M")` is a `text` node whose
positional is the text to show. `split` is a container with three parts:
`start` is pinned left, `end` right, and `center` is truly centred on
the bar, however wide the other two get.

**`windows`, `clock` and `battery` are services.** A service is a live
value: you read it, and when it changes, everything that read it
updates. There is no polling and no timer to write.

- `clock.format("%H:%M")` is the time as text. Because the format shows
  minutes, the clock wakes once a minute, and only that text redraws.
- `battery.percent` is a number from 0 to 1. `pct(…)` shows it as a
  percentage.
- `windows.focused` is the focused window, or `null` when nothing has
  focus. `?.` reads a field only if the value is not `null`, so
  `windows.focused?.title` is the title or `null`. `??` gives a fallback
  for `null`: here an empty string.

A service starts the first time your shell reads it and stops a few
seconds after nothing reads it any more. You never start or stop one.

## Change it

Make the bar a little taller, give the split some room at its ends, and
stop a long window title from pushing the clock around. Replace the
whole file with this and save:

```strand level=1 file=bar.strand
// bar.strand: a bar on every monitor. Every .strand file in this
// directory is part of your shell; save one and the change is live.
// Next steps: https://github.com/jakeb-grant/strand/blob/main/docs/guide/README.md
bar Top {
  edge: top; height: 36
  split { pad: 0, 12
    start  { text windows.focused?.title ?? "" { max_width: 40%; ellipsis: end } }
    center { text clock.format("%H:%M") }
    end    { text pct(battery.percent) }
  }
}
```

The bar grows to its new height and nothing restarts. Three new things:

- **A node's own props go in its block.** `split { pad: 0, 12 … }` sets
  the split's padding; the props come first, then the children.
- **Props with several values take commas.** `pad: 0, 12` is 0 above and
  below, 12 left and right; one to four values read as in CSS (top,
  right, bottom, left). Commas, not spaces, separate the values, so
  `a - 1` and `a -1` can never mean different things.
- **A positional and a block can go together.** `text … { max_width:
  40%; ellipsis: end }` keeps the title under 40% of the bar's width and
  cuts it with `…` at the end.

Every visual change you make animates: change `height: 36` back to
`height: 32`, save, and the bar springs to its new size. Animation is on
by default; you never ask for it.

## When you make a mistake

Change `height` to `heigth` and save. The bar keeps running exactly as
it was: a broken save never takes your shell down. After a quarter of a
second an overlay lists the problem, with the file, the line, a caret
under the word, and a suggestion ("did you mean `height`?"). Click it to
open the file in your editor. Fix the word, save, and the overlay goes.

You can check a config without running it, too:

```sh
strand check
```

It prints the same diagnostics and exits with an error if there are
any: useful when the shell is not running, or in a script.

## What you have

A bar on every monitor, live, animated, with no imports, no entry file
and no restart. Next, [level 2](level-2.md) gives it a theme, workspace
dots and a tray.

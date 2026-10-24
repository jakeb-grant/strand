# Level 3: things that remember

You learn: `state`, `<->`, `if`, `popup`, `enter`.
You build: a clock that opens a calendar, a volume control with a slider
that slides out on hover, and a battery that shows only when there is
one. At the end of this level your bar is design.md's bar (a), split
into one file per component.

## A clock with a calendar

Make `clock.strand`:

```strand level=3 file=clock.strand
component Clock {
  state open = false                               // one per monitor
  text clock.format("%a %d  %H:%M") {              // wakes once a minute
    on click { open = !open }
    popup { open: <-> open; Calendar }             // anchored here; Esc or click-away closes
  }
}

component Calendar {
  state month = clock.today.month_start()
  col { pad: $space.3; gap: $space.2; bg: $surface; radius: $radius.lg; shadow: $elevation.lg
    row { align: center
      button "‹" { on click { month = month.add(months: -1) } }
      text month.format("%B %Y") { grow: 1; align: center }
      button "›" { on click { month = month.add(months: 1) } }
    }
    grid { columns: 7; gap: $space.1
      for d in calendar.days(month) {
        text d.date.format("%e") { width: 28; height: 28; align: center
          when !d.in_month { color: $fg.faint }
          when d.date == clock.today { bg: $accent; color: $on_accent; radius: full }
        }
      }
    }
  }
}
```

Then put the clock in the centre of the bar:

```strand level=3 file=bar.strand
// bar.strand: one instance per monitor; `screen` is in scope
bar Top {
  edge: top; height: 36; margin: $space.2, $space.2, 0
  bg: $surface.alpha(0.72); blur: 24; radius: $radius.lg; shadow: $elevation.md
  font: $font.ui; color: $fg                      // inherited by every child
  split { pad: 0, $space.3                         // centre is truly centred
    start { gap: $space.3
      row { gap: $space.1
        for ws in workspaces.on(screen) { Dot ws } // keyed by ws.id, no max
      }
      text windows.focused?.title ?? "" { color: $fg.muted; max_width: 40%; ellipsis: end }
    }
    center { Clock }
    end { gap: $space.3
      text pct(battery.percent)
      for item in tray.items {
        image item.icon { size: 16
          on click     { item.activate() }
          on secondary { item.menu.open() }
        }
      }
    }
  }
}
```

Click the clock and a calendar opens, anchored to it. Press Escape, or click
anywhere else, and it closes.

**`state open = false` is a value your shell can change.** Everything
you have read so far comes from a service; `state` is your own. It
lives on the component, so each monitor's `Clock` has its own `open`:
opening the calendar on one monitor leaves the others alone.

**`open = !open` writes it.** `!` is "not", so a click flips it.
Writes happen in handlers (`on click { … }` and the like). A prop is a
live expression, never an assignment.
Writing a number has shorthands: `x += 1`, `x -= d`, `x *= 2`, `x /= 2`.

**`popup { … }` is a small surface anchored to its parent**, here the
clock's text. It shows while its `open` prop is true.

**`open: <-> open` is a two-way binding.** The popup reads `open` to
know whether to show, and writes it back when it closes itself (Escape,
a click away). One line replaces a prop plus a "closed" handler. The
right-hand side must be something writable: a `state`, a settings field,
or a service field that can be written.

**`Calendar` keeps its own `state month`.** The arrows move it with
`month.add(months: -1)`: a call can name its arguments. The grid shows
one cell per day from `calendar.days(month)`, padded with the days of
the months around it (`d.in_month` tells them apart), and today is
filled with the accent colour. `$on_accent` is the colour for text on
`$accent`, and `$fg.faint` is a fainter foreground still.

The clock's format changed too, to show the day: `%a %d  %H:%M`. It
still wakes only once a minute.

**Moving nodes into a new component starts them fresh.** The old
centre `text` is gone and `Clock` is new, so nothing carried over. That
is fine here, since the text had no state. In general a save keeps every
node whose source still maps to it, with its state, and wrapping a node
in a container such as `row` keeps it too; but a node you move into a
new component, or into another one, is a new node.

## Volume

Make `volume.strand`:

```strand level=3 file=volume.strand
component Volume {
  row { gap: $space.1
    on scroll(dy) { audio.sink.volume -= dy * 0.05 }
    icon audio.sink.icon { on click { audio.sink.muted = !audio.sink.muted } }
    if hover {                                     // the row's hover, latched while dragging
      slider { width: 90; value: <-> audio.sink.volume
               enter { width: 0; opacity: 0 } }
    }
  }
}
```

Then add it to the end of the bar:

```strand level=3 file=bar.strand
// bar.strand: one instance per monitor; `screen` is in scope
bar Top {
  edge: top; height: 36; margin: $space.2, $space.2, 0
  bg: $surface.alpha(0.72); blur: 24; radius: $radius.lg; shadow: $elevation.md
  font: $font.ui; color: $fg                      // inherited by every child
  split { pad: 0, $space.3                         // centre is truly centred
    start { gap: $space.3
      row { gap: $space.1
        for ws in workspaces.on(screen) { Dot ws } // keyed by ws.id, no max
      }
      text windows.focused?.title ?? "" { color: $fg.muted; max_width: 40%; ellipsis: end }
    }
    center { Clock }
    end { gap: $space.3
      Volume
      text pct(battery.percent)
      for item in tray.items {
        image item.icon { size: 16
          on click     { item.activate() }
          on secondary { item.menu.open() }
        }
      }
    }
  }
}
```

Point at the speaker icon and a slider slides out; drag it and the
volume follows. Scroll anywhere on the row to change the volume, and
click the icon to mute.

**`on scroll(dy) { … }` takes a parameter.** `dy` is how far the wheel
moved. Some events carry values like this; you name them in the
parentheses.

**`audio.sink.volume -= dy * 0.05` writes a service.** Some service
fields can be written, like `state`: the default output's volume and
its mute switch are two. Your write goes to the sound server, and when
the volume changes from anywhere else (a media key, another app) the
slider follows.

**`if cond { … }` adds and removes nodes.** `when` changes props of a
node that is always there; `if` decides whether its children exist at
all. `if hover` reads the row's `hover`. It stays true while you drag
the slider, even when the pointer leaves the row, so the slider never
vanishes under you. `if` can have an `else { … }` block for when the
condition is false.

**`trigger: hover` opens a popup by pointing.** The slider sits inside
the row, so `if hover` keeps it while you use it. A `popup` is a surface
of its own: an `if hover` popup would close on the pointer's way to it.
Give the popup `trigger: hover` instead and Strand opens and closes it:
it opens while the pointer is on the element it belongs to, stays while
the pointer is on either, and 200 ms more, long enough to cross the gap:

```
box { size: 28
  when popout.open { bg: $surface.hi }      // lit while its popup shows
  popup { id: popout; trigger: hover; pad: $space.3
    text "More here"
  }
}
```

The pointer is what opens such a popup. Its `open:` opens it from code
as well, and holds it open while true (a peek handing over to it);
once that is false, the pointer holds it as usual. `delay:`
makes the pointer rest first, and `grace:` changes the 200 ms. Pointing
at another `trigger: hover` element on the same bar swaps the popups at
once. `popout.open` reads whether a popup is open, whatever opens it.

**`enter { width: 0; opacity: 0 }` is where a node animates in from.**
When the slider appears it starts at no width and transparent, and
springs to its real size and full opacity. When it goes, it plays the
same pose in reverse before it is removed: `exit` mirrors `enter`
unless you write an `exit { … }` block of its own.

**`value: <-> audio.sink.volume`** is the same two-way binding as the
popup's: the slider shows the volume and writes it when dragged. A
slider runs from 0 to 1, as the volume does; for a value with other
ends, give them: `slider { value: <-> brightness_percent; range: 0, 100 }`
shows and writes 0 to 100.

## Battery

Make `battery.strand`:

```strand level=3 file=battery.strand
component Battery {
  row { gap: $space.1
    icon battery.icon
    text join(" · ", pct(battery.percent), dur(battery.time_left))
    when battery.percent < 0.15 && !battery.charging { color: $error }
  }
}
```

Then show it only when there is a battery:

```strand level=3 file=bar.strand
// bar.strand: one instance per monitor; `screen` is in scope
bar Top {
  edge: top; height: 36; margin: $space.2, $space.2, 0
  bg: $surface.alpha(0.72); blur: 24; radius: $radius.lg; shadow: $elevation.md
  font: $font.ui; color: $fg                      // inherited by every child
  split { pad: 0, $space.3                         // centre is truly centred
    start { gap: $space.3
      row { gap: $space.1
        for ws in workspaces.on(screen) { Dot ws } // keyed by ws.id, no max
      }
      text windows.focused?.title ?? "" { color: $fg.muted; max_width: 40%; ellipsis: end }
    }
    center { Clock }
    end { gap: $space.3
      Volume
      if battery.present { Battery }
      for item in tray.items {
        image item.icon { size: 16
          on click     { item.activate() }
          on secondary { item.menu.open() }
        }
      }
    }
  }
}
```

On a desktop with no battery, nothing shows. On a laptop you see an
icon, the charge and the time left, joined by a dot. `join` skips any
part that is `null`, so when UPower does not know the time left you see
only the charge. Below 15% and not charging, it turns red.

## Dots that grow in

Last, let new workspace dots grow in instead of popping. Replace
`dot.strand`:

```strand level=3 file=dot.strand
component Dot(ws: Workspace) {
  box { size: 8; radius: full; bg: $fg.alpha(0.25); hit: grow(6)
    when ws.occupied { bg: $fg.muted }
    when ws.focused  { width: 24; bg: $accent }    // later `when` wins, like CSS
    when ws.urgent   { bg: $error }
    when hover       { bg: $accent.hover }
    when pressed     { scale: 0.9 }
    enter { width: 0; opacity: 0 }                 // exit mirrors enter
    on click { ws.focus() }
  }
}
```

Make a new workspace: its dot grows from nothing, and the dots beside
it glide aside. Close it and the dot shrinks away.

## What you have

The bar from design.md, on every monitor: workspace dots, the focused
window's title, a clock with its calendar, volume, battery and tray, in
five files and about 80 lines.

| File | Holds |
| --- | --- |
| `bar.strand` | the `Top` bar |
| `dot.strand` | `Dot` |
| `clock.strand` | `Clock` and `Calendar` |
| `volume.strand` | `Volume` |
| `battery.strand` | `Battery` |

Next, [level 4](level-4.md) builds surfaces that come and go: an
on-screen display for volume, notifications, an app launcher and a lock
screen.

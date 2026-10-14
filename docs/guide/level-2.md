# Level 2: a themed bar with workspace dots and a tray

You learn: `$tokens`, `component`, `for`, `when`, `on click`.
You build: a themed bar with a dot for each workspace on its monitor, and
the system tray.

## Theme it with tokens

Replace `bar.strand` with this and save:

```strand level=2 file=bar.strand
// bar.strand: one instance per monitor; `screen` is in scope
bar Top {
  edge: top; height: 36; margin: $space.2, $space.2, 0
  bg: $surface.alpha(0.72); blur: 24; radius: $radius.lg; shadow: $elevation.md
  font: $font.ui; color: $fg                      // inherited by every child
  split { pad: 0, $space.3                         // centre is truly centred
    start { gap: $space.3
      text windows.focused?.title ?? "" { color: $fg.muted; max_width: 40%; ellipsis: end }
    }
    center { text clock.format("%H:%M") }
    end { gap: $space.3
      text pct(battery.percent)
    }
  }
}
```

The bar lifts off the screen edge, rounds its corners, turns
translucent and casts a shadow, and every change springs into place.

**A name starting with `$` is a design token.** Tokens are the shell's
shared values: spacing, radii, fonts, shadows and colours. You never
declare these: Strand has a built-in theme. Its colours come from your
desktop's accent colour (a blue if it has none), light or dark as your
desktop is set.

- `$space.2`, `$radius.lg`, `$font.ui` and `$elevation.md` are steps on
  scales. Use them instead of numbers and the whole shell stays
  consistent.
- `$surface`, `$fg` (foreground), `$accent` and `$error` are colour
  roles. `$fg.muted` is a softer foreground for secondary text.
- `$surface.alpha(0.72)` is the surface colour at 72% opacity. A name
  followed by `(` is a method: `alpha`, `mix`, `lighten` and `darken`
  work on any colour token in a prop. (Level 5's theme templates add a
  few more that only templates read.)

Tokens are live. When your desktop switches between light and dark, the
colours glide to the new palette; nothing reloads. Level 5 shows how to
write your own theme.

**`font` and `color` are inherited.** Set on the bar, they apply to
every text in it, so no `text` needs its own. `$fg.muted` on the title
overrides the colour for that one node.

**`blur: 24` asks the compositor to blur what is behind the bar.**
Strand never captures the screen. Where the compositor cannot blur, the
bar gets a little more opaque instead, so text stays readable.

## Workspace dots

A dot for each workspace: wide and bright for the focused one, dim for
empty ones, red for urgent ones. Make a new file, `dot.strand`, next to
`bar.strand`:

```strand level=2 file=dot.strand
component Dot(ws: Workspace) {
  box { size: 8; radius: full; bg: $fg.alpha(0.25); hit: grow(6)
    when ws.occupied { bg: $fg.muted }
    when ws.focused  { width: 24; bg: $accent }    // later `when` wins, like CSS
    when ws.urgent   { bg: $error }
    when hover       { bg: $accent.hover }
    when pressed     { scale: 0.9 }
    on click { ws.focus() }
  }
}
```

There is no import. Every `.strand` file in the directory is part of
the same shell, so `Dot` can be used from `bar.strand` as it is. Put the
dots at the start of the bar:

```strand level=2 file=bar.strand
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
    center { text clock.format("%H:%M") }
    end { gap: $space.3
      text pct(battery.percent)
    }
  }
}
```

Switch workspaces and the wide dot slides across.

**`component Dot(ws: Workspace)` is your own node.** It takes one
parameter, `ws`, of type `Workspace`. You call it like any other node:
`Dot ws`, with `ws` as its positional. Parameters can also be passed by
name in the block, and can have defaults: a
`component Toast(n: Notification, dense: bool = false)` is called as
`Toast n` or `Toast n { dense: true }`.

**`for ws in workspaces.on(screen) { … }` makes one `Dot` per
workspace.** `screen` is the monitor this copy of the bar is on (each
monitor gets its own), so each bar shows only its own workspaces. There
is no maximum: a tenth workspace gets a tenth dot. Workspaces bring their
own key (their id), so when one is added or removed the dots for the
others are kept, and the new dot is the only one drawn fresh.

**`when cond { props }` is conditional style.** While `cond` is true,
its props apply; when it turns false they spring back. Several `when`s
may set the same prop, and the later one wins, as in CSS: a focused
workspace that is also occupied is wide and `$accent`.

- `hover` and `pressed` are true while the pointer is over the node, or
  a button is held on it. `focused` and `selected` are the other two.
  They always mean the node the `when` is in, here the `box`.
- `ws.occupied`, `ws.focused` and `ws.urgent` are fields of the
  workspace, and update live like any service value.

**`on click { ws.focus() }` runs when the node is clicked.** Inside the
braces are statements; `ws.focus()` asks the compositor to switch to
that workspace. Service values have actions like this one, called with
`()`.

**`hit: grow(6)` makes the dot easier to click.** An 8-pixel dot is a
small target, so its clickable area grows 6 pixels on every side without
changing how it looks.

**The same name in two files.** Two files may each declare a component
with the same name (or a `type`, `enum` or `fn`): say `dot.strand` and
`menu.strand` both have a `component Item`. A file that declares a name
uses its own, so `Item` in `menu.strand` is the menu's. A file without
one of its own, like `bar.strand`, names the file it means: `dot.Item ws`
or `menu.Item t`. A bare `Item` there is an error that names both files
and offers both fixes:

```text
× `Item` is declared in 2 files, `dot.strand` and `menu.strand`, and not in this one
help: name the file it comes from: `dot.Item` or `menu.Item` (or declare `Item` in this file, which then uses its own)
```

Tokens follow the same rules: inside `dot.strand`, `$Item.gap` is the
dot's `Item`'s token; in `bar.strand` it is `$dot.Item.gap`. A name only
one file declares, like `Dot`, is used by its bare name everywhere.

Adding the second `component Item` is what makes a bare `Item` in
`bar.strand` an error. While `strand` is running, the reload holds the
new file back and the bar keeps running the last version that loaded;
the error in `bar.strand` says what to write, and once it is fixed both
files load together, each `Item` keeping its state. `strand check` and
the editor also put a note, not an error, on each declaration of a
shared name, listing the other files.

## The tray

Add the system tray at the end of the bar:

```strand level=2 file=bar.strand
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
    center { text clock.format("%H:%M") }
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

Each tray item is an `image` of its icon. A left click activates the
app; **`on secondary`** is the other button. Here it calls
`item.menu.open()`, which tells the app its menu is opening and sets
`item.menu.opened` to true. An app that draws its own menu shows it
then. Most apps (nm-applet, Discord, Steam) hand their menu to Strand
instead, and Strand draws nothing until your bar has a `popup` showing
`item.menu.items` while `item.menu.opened` is true. Bar (a) has no such
popup, so for those apps a right click does nothing visible yet;
[level 3](level-3.md) teaches `popup`, and the doc of the tray service's
`TrayMenu` record gives that menu popup.

## What you have

A themed bar with live workspace dots and a working tray, on every
monitor. Next, [level 3](level-3.md) adds things that remember: a clock
that opens a calendar, and a volume slider.

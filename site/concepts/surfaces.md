# Surfaces and monitors

A **surface** is something the compositor shows: a bar along an edge,
a panel, an on-screen display, a popup, a window or the lock screen.
Every node you write lives in one. Surfaces are declared at the top
level of a file, except `popup`, which is written inside the element
it opens from.

## The kinds

| Kind | What it is | By default |
| --- | --- | --- |
| `bar` | A strip along a screen edge | On every monitor, top edge, `top` layer; keeps its thickness free of windows |
| `panel` | Launchers, notification stacks, dashboards | On the focused monitor, `top` layer; windows go under it |
| `osd` | A passing display: volume, brightness | On the focused monitor, `overlay` layer; clicks pass through it |
| `popup` | A menu or card anchored to an element | Opens below its element (away from the edge on a bar), 6 px from it; mounts its children only while shown |
| `window` | An app's window, which the compositor manages | Tiles, floats and moves as any app's window does |
| `lock` | The lock screen | On every monitor; at most one per config |

Bars, panels and OSDs are layer-shell surfaces: the compositor puts
them on a **layer** above or below windows, `background`, `bottom`,
`top` or `overlay` (`layer: overlay` puts a panel above everything).
Each one has a stable namespace, `strand-<Name>`, so compositor rules
can match it: `strand-Top` for `bar Top`.

## What every surface takes

- **`open:`** says whether it is shown. A two-way `open: <-> open`
  lets the surface close itself: Escape, a click away and focus loss
  write `false`. A `window` is the exception: only the compositor's
  close and `w.close()` do.
- **`anchor:`** places a panel or OSD on its screen: `center`, an edge
  (`top`, `bottom`, `left`, `right`) or a corner (`top_right`, …).
  `margin:` keeps it off the edges it is anchored to.
- **`keyboard:`** is how it takes the keyboard: `none` (the default),
  `on_demand` (when you click it) or `exclusive` (while it is shown, as
  a launcher needs).
- **`on show`** and **`on hide`** run as it opens and closes, and
  `enter { … }` and `exit { … }` are where it animates from and to.
- **`inhibit_idle: true`** keeps the screen from dimming or locking
  while the surface is shown.

A bar reserves its edge, so windows are laid out beside it.
`exclusive:` on a `bar` or `panel` says how much: `true` its
thickness (a bar's default), a length in px, or `false` for none (a
panel's default). `attach: top` on a panel or popup grows it out of
that edge with concave corners.

## Monitors

A `bar` is on every monitor. Each monitor gets its own instance, with
its own state and with `screen`, that monitor, in scope:

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      for ws in workspaces.all.filter(w => w.screen == screen.name) {
        text ws.name { color: $fg.muted
          when ws.active { color: $accent }
          on click { ws.focus() }
        }
      }
    }
    end { text screen.name { color: $fg.faint } }
  }
}
```

```strand file=dock.strand
panel Dock {
  screens: all; anchor: bottom; margin: 0, 0, $space.2
  row { width: screen.width / 2; height: 48; radius: $radius.lg; bg: $surface }
}
```

On one monitor, `DP-1`, the two draw this. The active workspace moves
from 1 to 2:

```example surfaces
```

- **`screens:`** picks the monitors: `all`, `focused`, a monitor's
  name (`"DP-1"`) or a list of names. A bar's default is `all`.
- **A `panel` or `osd` is on the focused monitor** unless `screens:`
  says otherwise. Then it is one surface the compositor places, and
  `screen` is not in scope in it: reading it is a load error that says
  to add `screens: all`.
- **With `screens: all`**, a panel or OSD is on each monitor, as a bar
  is, with its own state and `screen` on each.
- **Plugging and unplugging.** A monitor plugged in gets its instances.
  An unplugged monitor's instances are kept with their state for 30
  seconds and come back with it if the monitor does. Monitors are told
  apart by make, model and description, so a monitor moved to another
  port is still the same one.

State declared in a component or a surface is per instance, so a
calendar opened on one monitor's bar stays closed on the others. State
declared at the top level of a file is one for the whole shell.

## Popups

`popup { … }` is written inside the element it belongs to and opens
from it, so it needs no position:

```strand fragment
text clock.format("%H:%M") {
  on click { open = !open }
  popup { open: <-> open; Calendar }
}
```

It nests (a menu's submenu is a popup in a popup), `anchor: pointer`
opens it at the click, as a context menu, and `trigger: hover` opens it
while the pointer is on its element. A popup takes the keyboard only
when a click or a key opened it.
[A bar with a calendar popup](../how-to/calendar-popup.md) is a whole
example.

## Windows

A `window` is a surface the compositor manages as an app's window:
it tiles or floats, can be moved and resized, and appears in task bars.
The compositor decides its size and place, so it takes no `screens:`,
`layer:`, `anchor:`, `margin:` or `keyboard:` (each is a load error);
`width` and `height` are the size it asks for. `title:` sets its title
and `app_id:` the id window rules match, `strand-<Name>` by default.
Give it `id: w` and `w.focused`, `w.maximized` and `w.tiled` follow the
compositor, while `w.move()`, `w.maximize()` and `w.close()` ask it.

## The lock screen

`lock { … }` is the lock screen, on the `ext-session-lock` protocol. It
is on every monitor, with one password shared between them, and
something in it must call `auth.submit`, or it is a load error, since
it would have no way out. It fails closed: if anything in it faults, a built-in password field takes
its place. A save that changes it waits until you unlock. The guide's
[level 4](../../docs/guide/level-4.md) builds one.

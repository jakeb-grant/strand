# Remembering things by key

A shell often keeps one thing for each of something else: the window
you last used on each workspace, a wallpaper for each monitor, a short
name for each app. Keep those in a
[dictionary](../concepts/language.md#dictionaries), `[K: V]`, which
holds one value for each key. Something whose order matters, such as
what you used most recently, stays a list.

## The last window on each workspace

A click on a workspace goes back to the window you were using there:

```strand file=workspaces.strand
// workspaces.strand: a click on a workspace goes back to the window you last used there
state last_win: [int: text] = [] persist

on change windows.focused?.id {
  let at = windows.focused?.workspace ?? -1
  if at >= 0 {
    last_win[at] = windows.focused?.id ?? ""
    last_win = last_win.filter(e => workspaces.all.any(w => w.id == e.key))
  }
}

bar Workspaces {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.3; align: center
    for ws in workspaces.all {
      text ws.name {
        when ws.active { color: $accent }
        on click { ws.focus(); windows.all.get(last_win[ws.id] ?? "")?.focus() }
      }
    }
  }
}
```

- **`[int: text]`** holds a window id (a `text`) for each workspace
  number (an `int`). It starts empty, `[]`, and the type says what kind
  of empty.
- **`last_win[at] = …`** adds the entry for that workspace, or replaces
  it. Assigning is for handlers, as with any `state`.
- **`last_win[ws.id]`** reads it back, or `null` for a workspace you
  have not used yet, so it is unwrapped with `??` like any optional
  value.
- **`windows.all.get(id)`** finds the window with that id, or `null`
  when it has closed since: a keyed list reads one item by its key.
- **`persist`** keeps the dictionary across restarts. Window ids do not
  survive a compositor restart, and an old one just finds no window,
  which does nothing.

### Forgetting what has gone

Nothing caps a dictionary's size. The line after the write drops the
entries of workspaces that no longer exist: `filter` keeps the entries
its test is true for, each an `Entry` with its `key` and `value`.

Prune when you write, as here, and not when a service first reports.
`on change` takes a value's first report as its starting point, and a
service can start out empty: pruning against an empty
`workspaces.all` at startup would forget everything you had kept.

## A wallpaper for each monitor

A wallpaper that depends on the monitor, kept in a
[settings file](settings-file.md) you can edit by hand:

```strand file=wallpaper.strand
// wallpaper.strand: a wallpaper for each monitor, by its name, in walls.toml
state walls from "walls.toml" {
  fallback: path = "~/walls/default.jpg"
  per_screen: [text: path] = []
}

panel Wallpaper {
  screens: all; layer: background; width: 100%; height: 100%
  image walls.per_screen[screen.name] ?? walls.fallback { fit: cover; grow: 1 }
}
```

The dictionary is a table in the file, one line per monitor. A monitor
it does not name shows the fallback:

```toml file=walls.toml
fallback = "~/walls/mountains.jpg"

[per_screen]
DP-1 = "~/walls/left.jpg"   # the wide one
HDMI-A-1 = "~/walls/right.jpg"
```

A handler sets one entry, `walls.per_screen[screen.name] = picked`, and
so does a terminal or a script:

```sh
strand set walls.per_screen.DP-1 ~/walls/sea.jpg
```

Strand writes back only the line that changed, keeping your comments
and the order of the rest.

## A short name for each app

A dictionary written out in full is a lookup table. This taskbar shows
each window by a short name for its app, falling back to the window's
title, and puts the window you used last first:

```strand file=taskbar.strand
// taskbar.strand: each window by its app's nickname, the last one used first
let nick: [text: text] = ["org.gnome.Nautilus": "Files", "kitty": "Terminal", "firefox": "Web"]
state stamp = 0
state used: [text: int] = []                     // window id -> when it last had focus

on change windows.focused?.id {
  let id = windows.focused?.id ?? ""
  if id != "" {
    stamp += 1
    used[id] = stamp
    used = used.filter(e => windows.all.any(w => w.id == e.key))
  }
}

bar Taskbar {
  edge: bottom; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.3; align: center
    for w in windows.all.sort_by(w => -(used[w.id] ?? 0)) {
      text nick[w.app_id] ?? w.title { ellipsis: end; on click { w.focus() } }
    }
  }
}
```

- **`nick[w.app_id] ?? w.title`** is the app's short name when the
  table has one, and the title when it does not.
- **`used`** gives each window a number that grows with every focus, so
  sorting by it, largest first, puts the latest first. A window never
  focused counts as 0 and goes last.
- **The filter bounds it** by the windows that are open, as the
  workspaces' was bounded by the workspaces.

Keys can be `int`, `text`, `path`, `bool` or an enum of your own. An
enum key is written bare, so a table of how long each kind of
notification stays reads `[low: 3s, normal: 6s, critical: 0s]`.

## A recent list stays a list

"The last five workspaces you were on" has an order, newest first,
which a dictionary does not keep: its entries are always in key order.
Keep it as a list, and bound it with `take`:

```strand file=recent.strand
// recent.strand: the last five workspaces you were on, the latest first
state recent: [int] = [] persist

on change workspaces.all.find(w => w.focused)?.id {
  let id = workspaces.all.find(w => w.focused)?.id ?? -1
  if id >= 0 { recent = ([id] + recent.filter(x => x != id)).take(5) }
}

bar Recent {
  edge: left; width: 40
  bg: $surface; font: $font.ui; color: $fg
  col { pad: $space.2, 0; gap: $space.2; align: center
    for id in recent key id {
      text workspaces.all.get(id)?.name ?? "" { on click { workspaces.all.get(id)?.focus() } }
    }
  }
}
```

The write puts the newest first, drops its older place in the list,
and keeps five. A launcher's recently picked themes are the same line
with a theme's name for the id.

## Which to use

| You keep | Use | Bounded by |
| --- | --- | --- |
| One value for each key (a workspace, a monitor, an app) | A dictionary, `[K: V]` | `m = m.filter(e => …)`, dropping keys that are gone |
| Records with their own order, kept as you arrange them | A keyed list, `[T] key field` | What you push and remove |
| The most recent few, newest first | A list, `[T]` | `take(n)` |

Each is one `state`, so `persist` keeps any of them across restarts.

## Coming from Python or JS

A dictionary is written in brackets, `["kitty": "Terminal"]`, not
braces, and the empty one is `[]` with its type declared:
`state used: [text: int] = []`. Braces in Strand hold a block, so
`{"kitty": "Terminal"}` is an error whose fix rewrites it in brackets.
There is no `get`, `set` or `has`: `m[k]` reads, `m[k] = v` writes,
`m[k] != null` asks, and `m.remove(k)` forgets. A `for` walks
`m.entries`, each with its `key` and `value`. The
[troubleshooting page](../help/troubleshooting.md#a-dictionary-that-does-not-check)
lists each mistake with its fix.

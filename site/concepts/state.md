# State and reactivity

Strand has three kinds of value. You write props as expressions over
them, and Strand keeps every prop up to date as they change. You never
write code that updates the screen.

## Three kinds of value

- **`state x = 0`** is a cell you write: a popup's open flag, a search
  query, the month a calendar shows. Its default is evaluated once,
  when the cell is made.
- **`let x = expr`** is a value derived from others. It is read-only,
  and recomputed only when something it reads has changed, and only
  when something reads it.
- **Service fields** such as `audio.sink.volume` or `clock.today` are
  live values that the system changes. A few are writable; the
  [service reference](../../docs/reference/services.md) marks them
  `rw`.

```strand file=volume.strand
state open = false
let level = audio.sink.muted ? 0 : audio.sink.volume

bar Volume {
  edge: bottom; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.2; align: center
    icon audio.sink.icon { size: 16 }
    text pct(level) { on click { open = !open } }
    if open {
      slider { width: 120; value: <-> audio.sink.volume }
    }
  }
}
```

`level` is derived from two service fields. The text shows it and
changes when either does. A click writes `open`, and the slider
appears. Here the volume then changes from outside, and both follow:

```example state
```

## Bindings record what they read

A program is one graph of values. Each prop, `let` and condition
records exactly what it read the last time it ran. When a value is
written:

- if the new value equals the old one, nothing downstream runs;
- otherwise only what read it is recomputed, and only the part of the
  screen that changed is painted again;
- when nothing is written, nothing runs and no frame is drawn.

Writes made together count as one change. Every binding sees a
consistent set of values, never a mix of old and new, and the screen
gets one set of changes for them.

## Where writes happen

Values are written only in **handlers**: the bodies of `on`, `after`
and `every`. A handler assigns with `=`, `+=`, `-=`, `*=` and `/=` to a
`state`, a field of one, a service's `rw` field or a field of a
settings file. Assigning to a `let`, a prop or a component's parameter
is a load error with a fix.

## Where state lives

Where you declare a `state` decides how many copies there are:

| Declared | One cell for |
| --- | --- |
| At the top level of a file | The whole program |
| Directly in a component's body | Each place the component is used |
| Directly in a surface's block | Each instance of the surface (a bar has one per monitor) |
| In a `for` body | Each item, in each instance |

Anywhere else is a load error that says where to move it. State never
sits on a leaf element such as a `text`, so editing an element never
takes state with it.

## Two-way bindings: `<->`

A widget that changes a value takes a two-way binding:

```strand fragment
slider { value: <-> audio.sink.volume }
input { text: <-> query }
popup { open: <-> open; Calendar }
```

The widget shows the value, and when you drag, type or dismiss, it
writes the value back. The target must be writable: a `state`, a
service's `rw` field or a settings field. A prop that only shows a
value takes a plain expression; `<->` is for the props the reference
marks two-way, such as `value`, `text` and `open`.

## Values that take time: `Async`

A search or a wallpaper palette is not ready at once. Such calls give
an `Async<T>`, which keeps its last value while the next one loads:

- `.pending` is true while a load runs, and `.error` says why the last
  one failed;
- `x ?? fallback` gives the value it holds, or `fallback` while it has
  none;
- using an `Async<T>` where a `T` is needed is a load error, so the
  waiting case is never forgotten.

`apps.search(query)`, `material(image: …)` and
[a declared action with a result](../how-to/dbus-and-actions.md#an-action-that-gives-back-a-value)
are `Async`.

## Events and timers

- `on change a, b { … }` runs when a watched value changes. It never
  runs for the first value, nor for a value a reload writes, so an OSD
  does not flash at boot or on save.
- `on change a after 1.2s { … }` runs once, 1.2 s after the last change.
- When a watched path moves to another item, its new value is the new
  baseline and `on change` does not run: `on change
  clipboard.history.first?.text` stays quiet when a new copy becomes the
  first. Watch the item's key too and it runs on each new one:
  `on change clipboard.history.first?.id, clipboard.history.first?.text`.
- `after 6s while !hover { … }` runs once, after its condition has held
  for six seconds in all; `every 1s { … }` runs every second.
- An event such as `on notifications.received(n)` is never merged:
  every one is delivered, in order.

## Keeping state across restarts

- **`persist`** on a `state` keeps its value across restarts.
- **`export`** on a top-level `state` or `let` makes it readable from
  other files and from the command line: `strand toggle launcher.open`
  flips `export state open = false` in `launcher.strand`.
- **Settings files** keep choices in a TOML file you can edit; see
  [a settings file](../how-to/settings-file.md).

## Loops are caught

A `let` or token that depends on itself is a load error naming the
cycle. A handler that writes one cell more than 30 times a second is
warned about and slowed to 30 writes a second, keeping its last value.
Your input does not count: a scroll or a drag may write as fast as it
moves, and so may `<->` and the command line.

[Live reload](live-reload.md) says what happens to state when you save
a file.

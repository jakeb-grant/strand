# An app launcher

A search box in the middle of the screen that finds your apps as you
type, best match first, and starts the one you pick. A key opens it;
Escape, a click elsewhere or focus moving away closes it. The guide's
[level 4](../../docs/guide/level-4.md#an-app-launcher) explains each
line of it.

## The file

```strand file=launcher.strand
// launcher.strand. Bind a key to: strand toggle launcher.open
export state open = false
state query = ""
let hits = apps.search(query)          // Async<[Hit]>: fuzzy + frecency, keeps last result

panel Launcher {
  screens: focused; layer: overlay; anchor: center; keyboard: exclusive
  open: <-> open                       // Escape, click-away and focus loss write false
  enter { opacity: 0; scale: 0.96 }
  on show { query = "" }
  col { width: 600; pad: $space.3; gap: $space.2; radius: $radius.xl; clip: true
        bg: $surface.alpha(0.82); blur: 32; shadow: $elevation.xl
    input { text: <-> query; placeholder: "Search apps"; font: $font.title; focus: true; nav: results }
    list { id: results; max_height: 420               // virtualised; arrows move selection
      for h in hits {
        row { pad: $space.2; gap: $space.3; radius: $radius.md; align: center
          when hover    { bg: $surface.hi }
          when selected { bg: $accent.container }
          on activate { h.app.launch(); open = false }
          image h.app.icon { size: 32 }
          col { grow: 1
            text h.app.name { marks: h.ranges; mark_color: $accent; weight: 600 }
            text h.app.comment ?? "" { font: $font.caption; color: $fg.muted; ellipsis: end }
          }
        }
      }
    }
    if hits.len == 0 && query != "" && !hits.pending { text "No matches" { color: $fg.muted } }
  }
}
```

The launcher fades and scales in over seven apps, then "ter" is typed a
letter at a time and the list narrows to the apps that match.

```example launcher
```

## Bind a key

Strand does not grab keys. Your compositor runs the command instead:

```sh
strand toggle launcher.open
```

Bind it to a key such as Super+D in your compositor (how: [Start it
and bind keys](../start/running.md#start-it-and-bind-keys)).

`strand toggle` flips an exported `bool` state. `launcher.open` is the
file's name, then the state's: `export` is what lets the command line
reach it.

## How it works

- **`apps.search(query)`** matches the apps' names fuzzily, then their
  keywords and generic names, and ranks by how often and how lately
  you launched each. An empty query lists every app, the most used
  first.
- **It returns an `Async`.** A search takes a moment, so `hits` keeps
  its last result while the next one runs, and the list never blanks
  while you type. `hits.pending` is true while a search runs, which is
  why "No matches" waits for `!hits.pending`.
- **`list` lays out and mounts only its visible rows**, however many
  apps there are. It has a selection: the arrow keys move it,
  `when selected` styles that row, and Return runs its `on activate`.
- **`nav: results`** sends the arrow keys and Return from the input to
  the list with `id: results`, so you keep typing while you move the
  selection.
- **`keyboard: exclusive`** takes the keyboard while the launcher is
  open, so you can type at once.
- **`on show { query = "" }`** empties the box each time it opens.
- **`marks: h.ranges`** highlights the letters that matched in
  `$accent`.

## Variations

- **More than starting an app.** `h.app.actions` lists the extra
  actions an app's desktop entry offers (a new window, a private one),
  each with a `name` and `a.launch()`.
- **A calculator in the same box.** An action that gives back what its
  command prints turns a command-line calculator into a value that
  follows `query`; see
  [calling a D-Bus method or a declared action](dbus-and-actions.md#an-action-that-gives-back-a-value).

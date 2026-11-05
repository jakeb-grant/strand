# The language in brief

A Strand shell is a directory of `.strand` files. Each file declares
surfaces, components, state and the rest; together they make one
program, checked as a whole before anything runs. This page names the
pieces. The [guide](../../docs/guide/README.md) teaches them in order,
and [grammar.md](../../docs/grammar.md) is the exact syntax.

## Files

Your config is a directory, `~/.config/strand`
(`$XDG_CONFIG_HOME/strand`) unless you name another one to `strand
run`. Every `.strand` file in it, up to three
directories deep, is loaded. There are no imports and no entry file:
a component declared in one file is usable in every other.

What a file declares at the top level is either shared by every file
(components, surfaces, enums, types, functions, token sets, keyframes,
actions and services) or the file's own (its `state` and `let`). `export` shares a
value too: `export state open = false` in `launcher.strand` is read
elsewhere, and from the command line, as `launcher.open`.

## Surfaces and nodes

A **surface** is something the compositor shows: a `bar`, `panel`,
`osd`, `window` or `lock`. Its body is a tree of **nodes**, and every
node is written the same way:

```strand fragment
kind positional { props; children }
```

The positional is one value, such as the text of a `text`. Anything in
braces is a prop or a child. Here is a whole shell in one file:

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.3; align: center
    text "Strand" { weight: 600 }
    spacer {}
    text clock.format("%a %d  %H:%M")
    text pct(battery.percent) { color: $fg.muted }
  }
}
```

`bar Top` is on every monitor. `row` lays its children out side by
side, and `spacer` takes the room between them. `text` draws text.
The <a href="../reference/elements/index.html">element reference</a> lists every element with
its props and events.

A **component** is a node kind of your own:

```strand file=chip.strand
component Chip(label: text, on: bool = false) {
  text label { pad: $space.1, $space.2; radius: full
    when on { bg: $accent; color: $on_accent }
  }
}
```

It is called like a built-in element, `Chip "Wi-Fi" { on: true }`, with
its first parameter as the positional and the rest as props.

## Props

A prop is `name: value`, ended by a line break or `;`. Several values
take commas: `pad: 0, $space.3` is no padding at the top and bottom and
`$space.3` at the sides, as in CSS. `font`, `weight` and `color` are
inherited: set on a node, they apply to everything inside it that does
not set its own.

A `when` changes props while a condition holds, and the last one
written wins, as in CSS:

```strand fragment
text "Battery" {
  when battery.percent < 0.2 { color: $error }
  when hover { color: $accent }
}
```

`hover`, `pressed`, `focused` and `selected` are the state of the node
the `when` is written in. `hover` follows what is under the pointer as
drawn, so when a list scrolls or a row closes under a pointer that stays
still, it moves to what is there now.

## Expressions

Every prop's value is a live **expression**. `text clock.format("%H:%M")`
is not set once: it shows the clock's value whenever it changes. You
write what a prop should be, and Strand keeps it so. [State and
reactivity](state.md) says how.

Expressions are small and typed:

| Kind | Examples |
| --- | --- |
| Numbers and lengths | `36`, `0.72`, `4px`, `40%`, `4ch` (a bare number is px) |
| Durations and angles | `200ms`, `1.2s`, `5min`, `270deg` |
| Colours | `#7aa2f7`, `$accent`, `$surface.alpha(0.8)` |
| Text | `"Search apps"`, joined with `+`; no interpolation |
| Lists | `[1, 2, 3]`, `apps.search(q).take(5)` |
| Dictionaries | `[1: "web", 2: "code"]`, `last_win[ws.id] ?? ""` |
| Optional values | `n.image ?? n.app.icon` (`??` gives the right side when the left is `null`) |
| Choices | `muted ? 0 : volume`, `match kind { volume => …, brightness => … }` |
| Functions | `x => x * 2`, `fn double(x: int) -> int { x * 2 }` |

A value that might be `null` must be unwrapped with `?.` or `??`
before it is used, and `strand check` says where you forgot. A
`match` must cover every case.

### Dictionaries

A **dictionary** `[K: V]` keeps one value for each key: the last
window used on each workspace, a wallpaper for each monitor, how long
each urgency of notification stays.

```strand file=memory.strand
enum Urgency { low, normal, critical }
state last_win: [int: text] = [] persist
let waits: [Urgency: duration] = [low: 3s, normal: 6s, critical: 0s]
```

A key is an `int`, a `text`, a `path`, a `bool` or an enum, written
bare (`low`). `last_win[3]` reads the value for `3`, or `null` when
there is none, so it is unwrapped like any optional value:
`last_win[ws.id] ?? ""`. A handler sets one with `last_win[3] = id`
and forgets one with `last_win.remove(3)`. `for e in last_win.entries`
walks the entries, each with its `key` and `value`, and `.keys`,
`.values`, `.len` and `.filter(e => …)` read the rest. The
[functions reference](../../docs/reference/functions.md) lists every
member.

Entries are always kept in key order, whatever order they were written
in: numbers by value, texts by character code, enums in the order they
are declared. `.keys` and `for e in m.entries` walk them in that order,
and two dictionaries with the same entries are equal (`==`). Nothing
caps a dictionary's size: a handler that adds entries also drops the
ones it no longer needs, `m = m.filter(e => …)`.

**Coming from Python or JS.** A dictionary is written with brackets,
not braces: `[k: v]`, and `[]` when it is empty. Braces hold a block
in Strand, so `{"a": 1}` is an error whose fix is `["a": 1]`. A
dictionary's type is declared where it starts empty
(`state m: [text: int] = []`); an untyped `[]` is a list. There are no
`get`, `set` or `has` methods: `m[k]`, `m[k] = v` and `m[k] != null`
do those jobs, and a value is never `null`, so a key that reads
`null` is one that is not there.

## Trees that change

`if`, `match` and `for` add and remove nodes:

```strand fragment
if battery.charging { icon "battery-charging-symbolic" }
for ws in workspaces.all { text ws.name }
```

A `for` gives each item its own copy, kept by the item's **key**: a
workspace by its id, a notification by its id. When the list changes,
copies whose key stays keep their state and slide to their new place.
A list of plain values names its key, `for p in pins key p.app`.
A keyed list reads one item by its key with `get`: `tray.items.get(id)`
is the tray item with that id, or `null` when there is none.

## Handlers

`on click { … }` and the other `on` blocks are **handlers**: the only
place where values are written and actions run.

```strand fragment
button "Mute" { on click { audio.sink.muted = !audio.sink.muted } }
```

`after 6s { … }` and `every 1s { … }` are timers, and `on change x
{ … }` runs when `x` changes. A handler may `await` a slow call, and
other handlers run while it waits.

## Checked before it runs

`strand check` reads the whole config and reports every problem it
finds, each with where it is and, where it can, a fix: an unknown name
with a did-you-mean, a wrong type, a `null` not handled. A running
shell checks each save the same way, and keeps running the last good
version while a file has errors.

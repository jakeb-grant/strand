# Keyboard shortcuts

A surface that has the keyboard hears every key, and your config says what
each key means. Write the keys a handler is for, and those keys are its:

```strand file=launcher.strand
// launcher.strand: Ctrl+K opens the search field, Escape steps back
export state open = false
state searching = false
state query = ""

panel Launcher {
  anchor: center; keyboard: exclusive; open: <-> open
  width: 400; height: 200
  bg: $surface; color: $fg; font: $font.ui

  on key "ctrl+k" { searching = true }
  when searching {
    on key "esc" { searching = false; query = "" }
  }

  col { gap: $space.2; pad: $space.3
    text searching ? "Search" : "Press Ctrl+K"
    input { text: <-> query; focus: searching }
  }
}
```

## A claim stops everything else

`on key "esc" { … }` is a **claim**. When the key matches, that handler
runs, and nothing else does: no typing in the field, no list keys, no
Tab moving focus, no Escape closing the panel, no other handler.

Claims are looked for from the focused element out to the surface, and the
innermost one wins. On one element, the last one written wins.

## In a `when`

A claim in a `when` claims only while the condition holds. With it false,
the key carries on as if the claim were not there. Above, Escape leaves
the search while `searching` is true. Once it is false, the next Escape
closes the panel, as a two-way `open` does by itself.

Any handler of an element may sit in a `when`, `on click` too.

## Writing a key

| Pattern | Means |
|---|---|
| `"ctrl+n"`, `"ctrl+shift+p"` | Modifiers `ctrl`, `shift`, `alt`, `super`, before the key. They are exact: `"n"` is not Ctrl+N |
| `"a"`, `"shift+a"` | A letter, in any case. `"a"` is not Shift+A |
| `"?"`, `"1"` | Any other single character: what the key types, however it took Shift to type it. Write `"!"`, not `"shift+1"` |
| `"esc"`, `"enter"`, `"tab"`, `"space"` | Short names, with `up`, `down`, `left`, `right`, `pgup`, `pgdn`, `backspace`, `del` |
| `"Page_Down"`, `"F5"`, `"XF86AudioMute"` | A key's own name, in any case |

With Ctrl, Alt or Super held, a key matches by its name and then by its
Latin name, so `ctrl+n` also works on a Russian layout. A plain `"j"` matches
only a key called that, and leaves the Cyrillic letter on it to be typed.

`strand check` says when a pattern names no key, and which it thinks you
meant. It knows the common names; any name starting `XF86` or `dead_` is
taken as written, unchecked.

## Hearing keys without taking them

`on key(k)` only listens: the key still does what it would have done. It
does not hear a key something claimed. `on key_release(k)` hears a key let
go, and `k.repeat` tells a key held down from a new press.

```strand file=keys.strand
panel Keys {
  keyboard: exclusive; width: 200; height: 40
  state last = ""
  on key(k) { if !k.repeat { last = k.name } }
  on key "ctrl+q" (k) { last = "quit, repeat: " + (k.repeat ? "yes" : "no") }
  text last
}
```

A claim that takes `(k)` gets the key too, as above. To also run the next
claim out, call `propagate()` in it; the default stays stopped.

## Moving a list from a key

A list's selection moves from code as it does from the arrows. Give the list
an `id:`, then call `next()`, `prev()`, `first()`, `last()`, `select(i)` or
`activate()` on it from any handler:

```strand file=picker.strand
// picker.strand: Ctrl+N and Ctrl+P walk the results from the field
export state open = false
state query = ""
state names: [text] = ["Files", "Firefox", "Terminal"]

panel Picker {
  anchor: center; keyboard: exclusive; open: <-> open
  width: 300; bg: $surface; color: $fg; font: $font.ui
  on key "ctrl+n" { results.next() }
  on key "ctrl+p" { results.prev() }
  on key "ctrl+y" { results.activate() }
  col { gap: $space.2; pad: $space.3
    input { text: <-> query; focus: true; nav: results }
    list { id: results; height: 120; wrap: true
      for name in names key name {
        box { pad: $space.1
          when self.selected { bg: $accent.alpha(0.22) }
          on activate { open = false }
          text name
        }
      }
    }
    text join(" of ", (results.selected_index ?? 0) + 1, names.len)
  }
}
```

They go to the row the keys would: scrolled into view, a row not mounted yet
included. `select(i)` past either end stops at that end. On an empty list they
do nothing. `results.selected_index` is the selected row's place, from 0, or
null with none. `wrap: true` takes the arrows, `next()` and `prev()` round from
one end to the other.

## A strip or a grid

`axis: horizontal` lays a list out in a line, and `columns: 4` as a grid. It is
still one list with one selection, and the arrows follow the layout:

```strand file=wallpapers.strand
// wallpapers.strand: a grid the arrows walk, going round at the ends
export state open = false
state names: [text] = ["Dune", "Forest", "Harbour", "Lake", "Peak"]

panel Wallpapers {
  anchor: center; keyboard: exclusive; open: <-> open
  width: 400; height: 240; bg: $surface; color: $fg; font: $font.ui
  list { focus: true; grow: 1; columns: 3; gap: $space.2; pad: $space.3; wrap: true
    for name in names key name {
      box { height: 80; pad: $space.1
        when self.selected { bg: $accent.alpha(0.22) }
        text name
      }
    }
  }
}
```

| | `axis: horizontal` | `columns: n` |
|---|---|---|
| Up, Down | do nothing, but Down opens a row's popup and Up closes a submenu | a line, in the same column |
| Left, Right | one row | one row, stopping at a line's end |
| `next()`, `prev()` | one row | one row, across lines |
| Page keys, space and Shift+space on a focused list | a view of rows | a view of lines |
| Each row is | as wide as it needs, and fills the list's height | a share of the list's width, and fills its line's height |

Down from above an empty cell of a short last line goes to the last row. With
`wrap: true`, moving off an end goes round.

In an `input` with `nav:` to either, Left and Right stay the caret's. To give
them to the list, claim them: `on key "right" { results.next() }`.

## Focus and what it does

Fields, lists, buttons, sliders and segmented controls can all have the
keyboard's focus, and so can any element you write a `focus:` on, even
`focus: false`. Tab and Shift+Tab walk them in order. `when focused { … }` styles
whichever has it, and `node.focus()` moves it from any handler.

| On a focused | Key | Does |
|---|---|---|
| `button`, `slider`, `segmented` or anything not below | Return, space | its `on activate` if it has one, else `on click` |
| `list` row | Return, space | selects the row and runs its `on click`, then its `on activate`, or the list's |
| element inside a `list` row | Return, space | as a `button` does, and selects the row and runs its `on activate` |
| `input` | Return | its `on activate`, or with `nav:` the selected row's |
| `list` | Return | the selected row's `on activate` |
| | Page keys, space, Shift+space | a view of rows |
| `slider` bound two-way | Left, Down / Right, Up | a step down / up |
| | Page keys | ten steps |
| | Home, End | the ends |
| `segmented` bound two-way | Left, Right | the choice beside this one |
| `scroll` | arrows | a step |
| | Page keys, space, Shift+space | a view |
| | Home, End | the ends |
| | Return | as a `button` does |

A step is the slider's `step:`, or a hundredth of its `range` without one. A claim
beats every one of these, as it beats any default. Space on a `scroll` or `list`
only pages, so it never clicks what is around it. Held, it goes on paging.
Return and space press a node once, however long the key is held. Return on an
`input` or a `list` is no press: held, it runs `on activate` again with each repeat.
A `slider` or `segmented` without `<->` shows its value and leaves its arrow, Page,
Home and End keys alone.

The mouse is not the keyboard here. A pointer's click runs `on activate` only on a
`list`'s row; anywhere else it runs `on click`. So a `button` with only an
`on activate` answers Return and ignores the mouse. Give a button `on click`, and
both reach it.

A press on a button focuses it, but not away from a field or a list: a button
beside a search box runs and the typing stays in the box. An element you wrote
`focus:` on takes focus from a press whatever has it.

## Moving focus with the arrows

`arrows: focus` on a container makes the arrows inside it move focus to the
nearest thing that way, by where things are laid out, so it works on a row, a
column or tiles of any size. `arrows: focus_wrap` goes round at the ends.

```strand file=power.strand
// power.strand: a menu of tiles, the arrows between them
export state open = false

panel Power {
  anchor: center; keyboard: exclusive; open: <-> open
  bg: $surface; color: $fg; font: $font.ui
  col { arrows: focus; gap: $space.2; pad: $space.3
    row { gap: $space.2
      button "Lock" { focus: true; when focused { bg: $accent } }
      button "Sleep" { when focused { bg: $accent } }
    }
    button "Shut down" { when focused { bg: $accent } }
  }
}
```

They stop at the nearest container with `arrows:`, and don't reach into another
one inside it. An element that uses an arrow keeps it: a field's caret, a list,
a slider, a segmented control's Left and Right, a `scroll` the way it runs.

## Scrolling from code

Give a `scroll` an `id:`, then move it and read it back. A `list` takes the same:

```strand file=log.strand
// log.strand: a page that scrolls by key, and says how far it went
export state open = false
state lines: [text] = ["one", "two", "three", "four", "five", "six"]

panel Log {
  anchor: center; keyboard: exclusive; open: <-> open
  width: 300; bg: $surface; color: $fg; font: $font.ui
  on key "g" { page.scroll_to(0) }
  on key "j" { page.scroll_by(40) }
  on key "z" { last.reveal() }
  col { gap: $space.2; pad: $space.3
    text join("", page.scroll_y.round(), " px")
    scroll { id: page; focus: true; height: 60
      col {
        for line in lines key line { text line }
        text "the end" { id: last }
      }
    }
  }
}
```

`scroll_to(y)` and `scroll_by(dy)` spring as a wheel does, and stop at the ends.
On one that runs along x, they go along x; on `axis: both`, write
`scroll_to(x: 40)` or `scroll_by(dx: 40)`. `scroll_y` and `scroll_x` follow what
is drawn, and reading them wakes nothing while it stands still. `node.reveal()`
on any element scrolls whatever is around it, as little as it takes, at once.
Focus moving onto something in a `scroll` by a key or from code does the same; a
press doesn't, so what you pressed stays under the pointer.

## Dead keys, Compose and input methods

A focused `input` types what your keyboard composes. On a layout such as `de`,
`fr` or `us(intl)`, a dead key and then `e` types `é`, and a Compose key works the
same way, from xkb's table for your locale and your `~/.XCompose`. There is nothing
to turn on, and with no table keys type what they type.

The keys of a sequence type nothing until the last one. A handler still hears each
by its real name, `dead_acute`, `Multi_key` or `e`, so your claims match as ever,
and `k.text` is empty until the key that finishes the sequence.

Escape in the middle of a sequence ends it and does nothing else: no handler hears
that Escape, and the launcher stays open. The next Escape closes it. A sequence also
ends when focus leaves it: the keyboard leaving, keys going to another surface, the
surface closing or a pointer press. A key held with Ctrl, Alt or Super ends one and
acts as usual, while AltGr types and composes.

Strand has no input method: `text-input-v3` is not supported, so Fcitx and IBus
cannot type into an `input`. Chinese, Japanese or Korean text that needs one cannot
be typed there, though it can be pasted with Ctrl+V.

## Around a password

A claim of a key that types, such as `"q"`, around a `type: password` field
or around a field on a `lock`, is an error from `strand check`, since it
would eat what is typed there. Give it a modifier, or put it on an element
the field is not inside.

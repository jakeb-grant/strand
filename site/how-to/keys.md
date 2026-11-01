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

## Around a password

A claim of a key that types, such as `"q"`, around a `type: password` field
or around a field on a `lock`, is an error from `strand check`, since it
would eat what is typed there. Give it a modifier, or put it on an element
the field is not inside.

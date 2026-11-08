# Another app's config file

Keep a file that another program reads in step with your shell: a
terminal's font size from a setting you change in the bar, a status
file with the track that is playing, a compositor's monitor lines from
the monitors connected. The `file` element writes it, and writes it
again whenever what it reads changes.

## The files

```strand file=shell.strand
// shell.strand: the font size lives in prefs.toml; kitty and a status
// line read the files below
permit exec "pkill"
state prefs from "prefs.toml" { font: int = 11 }

file "~/.config/kitty/strand.conf" {
  text: "font_size " + num(prefs.font) + "\n"
  run: ["pkill", "-USR1", "kitty"]
}
file "~/.cache/strand/now-playing.txt" {
  text: join(" - ", media.artist, media.title)
}

bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    center { text media.title ?? "" }
    end {
      row { gap: $space.2
        button "A-" { on click { prefs.font = prefs.font - 1 } }
        text num(prefs.font)
        button "A+" { on click { prefs.font = prefs.font + 1 } }
      }
    }
  }
}
```

kitty reads the first file with `include strand.conf` in its
`kitty.conf`, and reloads its config on the signal `run:` sends. A
status line, a prompt or an OBS text source reads the second.

## How it works

- **`text:` is an expression like any other.** It reads settings,
  `state`s, `let`s, `fn`s and services, and the file follows them. A
  number, a bool or a colour is written as text for you (a colour as
  `#rrggbb`); a list is an error, so join it yourself.
- **It waits for the services it reads.** Until `media` has answered,
  its fields are defaults, so the file is not written at all, even
  with a `??` fallback, and a good file from the last session is never
  replaced by an empty one. If a service it reads has stopped, the file
  says so in a notice.
- **It writes only what changed**, atomically, at most twice a second:
  a track change writes once, and the last value always lands.
- **`run:`** runs 200 ms after a write, and needs `permit exec` for its
  program. A `run:` whose program changes what the file is made from
  would loop; Strand holds it after three rounds and says so.
- **Strand never overwrites a file it did not write.** The first time,
  a file already there gives a notice with `[replace]`.

## Variations

- **Leave the file alone.** `text: null` writes nothing, so a
  condition can say when the file is not Strand's to write:
  `text: wm.name == "sway" ? swaycfg() : null`.
- **A theme file and a live one.** A `text:` that reads only the theme
  is a template, with `$surface.light()` and the colour channels; one
  that also reads a setting is not. For both, use a `template:` file
  for the theme and a second `file` for the rest.
- **A longer pause.** To write a file only once a value has settled,
  copy it into a `state` after a delay and write that:
  `on change prefs.font after 2s { font = prefs.font }`.
- **Code another program runs.** A compositor's config runs at its next
  reload, outside `permit exec`. Put outside text (a monitor's model, a
  title) only where the format expects a name, and escape it with a
  `fn` anywhere else.
- **Colours in a setting.** `hex(c)` and `argb(c)` format any colour,
  in a `text:` or in a label: `hex(prefs.accent, hash: false)`.

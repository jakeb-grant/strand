# A settings file

Keep the choices of your shell in a plain TOML file that anyone can
edit, with types, defaults and live updates, and change them from the
shell itself or from a terminal. A settings file is one declaration:
`state name from "file.toml" { fields }`.

## The files

```strand file=bar.strand
// bar.strand: the bar's choices live in prefs.toml beside this file
state prefs from "prefs.toml" {
  seconds: bool = false; position: Edge = top; accent: color = #7aa2f7
}
use palette material(seed: prefs.accent, dark: system.dark)

bar Top {
  edge: prefs.position; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    center { text clock.format(prefs.seconds ? "%H:%M:%S" : "%H:%M") }
    end {
      button prefs.seconds ? "Hide seconds" : "Show seconds" {
        on click { prefs.seconds = !prefs.seconds }
      }
    }
  }
}
```

A settings declaration at the top of a file belongs to that file, as
any top-level `state` does; `export state prefs from …` would let other
files read it as `bar.prefs`.

Here it is before there is a settings file. A click on the button sets
`prefs.seconds`, and the clock shows them:

```example settings
```

And the settings themselves, which you can leave out: a missing file
gives every field its default.

```toml file=prefs.toml
# The bar's choices. Strand keeps these comments when it writes.
seconds = true
position = "bottom"
accent = "#e06c75"
```

## How it works

- **Each field is `name: type = default`**, read as `prefs.seconds`.
  A relative path is beside your config; `~/…` is in your home
  directory.
- **Edits apply live, field by field.** Save the TOML file and the bar
  follows: the clock shows seconds, the bar moves to the bottom edge,
  the colours spring to the new accent.
- **A bad value keeps that field's last good value**, with a
  diagnostic; the other fields still apply. A TOML syntax error keeps
  every last good value. A line you delete goes back to its default.
- **It is two-way.** The button assigns `prefs.seconds`, and Strand
  writes the file, keeping its comments, spacing and order. A `<->`
  binding writes it the same way. Writes are saved after 250 ms of
  quiet.
- **From a terminal**, `strand set` writes a field, as the button
  does, and `strand get` reads it:

  ```sh
  strand set prefs.position top
  strand get prefs.accent
  ```

  With the file's name first, `bar.prefs.position` is the same field;
  when two files declare settings named `prefs`, only that form works.
- **The value's type decides what the file may say:** `true` or
  `false` for a `bool`, a quoted `"#rrggbb"` for a `color`, an enum's
  variant by name for an enum (`position = "bottom"`).

## Where the file lives

Writes follow symbolic links and replace the file through a temporary
file and a rename, so a settings file in a dotfiles repository stays a
link. A file you cannot write, such as one in `/nix/store`, gets the
write in an overlay in Strand's state directory instead, with a notice.

## Variations

- **Controls bound to it.** `segmented { options: Edge; value: <->
  prefs.position }` is a picker for the edge, a segment for each of
  `Edge`'s variants; `options: [Edge.top, Edge.bottom]` offers only
  those two. A field `level: float = 0.5` takes `slider { value: <->
  prefs.level }` the same way.
- **Several files.** Each `state … from` is its own file; give them
  different names (`prefs`, `layout`).
- **One value per key.** A [dictionary](../concepts/language.md#dictionaries)
  field, `wallpapers: [text: path] = []`, is a table in the file, one
  line per key; Strand writes back only the keys that changed, keeping
  the rest as you wrote them:

  ```toml
  [wallpapers]
  DP-1 = "~/walls/left.jpg"   # the wide one
  HDMI-A-1 = "~/walls/right.jpg"
  ```

  A handler sets one entry, `prefs.wallpapers[screen.name] =
  picked.file`, and so does a terminal: `strand set
  prefs.wallpapers.DP-1 ~/walls/new.jpg`. `strand get prefs.wallpapers`
  prints them all. A bad entry keeps the whole field's last good value,
  with a notice naming its key.
- **Records and lists of them.** A field can hold a record of your own
  `type` (as [the dock](../../docs/guide/level-5.md#5-a-dock) declares
  `Pin`), a list of records, or a dictionary of them. Pinned apps, in the order you keep
  them:

  ```strand file=pins.strand
  // pins.strand: pinned apps kept in pins.toml; a click moves one to the front
  type Pin { id: text; hidden: bool }
  state pins from "pins.toml" { apps: [Pin] = [] }

  bar Pins {
    edge: bottom; height: 32
    bg: $surface; font: $font.ui; color: $fg
    row { pad: 0, $space.3; gap: $space.3; align: center
      for p in pins.apps.filter(p => !p.hidden) key p.id {
        text p.id { on click { pins.apps = [p] + pins.apps.filter(q => q.id != p.id) } }
      }
    }
  }
  ```

  In the file, a list of records can be one `[[apps]]` section per
  element, and a record a `[section]` of its own, as well as the inline
  `{ … }` form:

  ```toml
  # the browser
  [[apps]]
  id = "firefox"

  # the terminal
  [[apps]]
  id = "kitty"
  ```

  A field you leave out of a record is its default, here `hidden =
  false`. A key the record does not have, such as `hiden = true`, keeps
  the field's last good value with a notice naming where it is, so a
  typo never passes for the default. Strand writes the list back in the
  form the file has, matching elements by value: when the click moves
  the terminal to the front, each pin keeps the comments written on it,
  and an element you changed is written afresh. A comment above an
  element's `[[apps]]` is that element's and moves with it. A
  dictionary of records works the same way: `profiles: [text: Profile]`
  reads `[profiles.desk]`, with a list inside it as
  `[[profiles.desk.mons]]`.
- **A bad value is not written over.** While a field's value in the
  file is bad (a typo, or a key a record no longer has after you edit
  its `type`), a write of that field from the shell is kept live but
  not saved, with a notice: saving it would replace what you wrote,
  every other profile of a dictionary among it. Fix the file, and its
  value applies.

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
  prefs.position }` is a picker for the edge; a field
  `level: float = 0.5` takes `slider { value: <-> prefs.level }` the
  same way.
- **Several files.** Each `state … from` is its own file; give them
  different names (`prefs`, `layout`).

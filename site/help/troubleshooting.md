# Troubleshooting

Each section starts from what you see and says what to do. When a
problem is not here, run the shell with more logging (see
[Logs](#where-the-logs-go)) and save a
[glitch report](../tools/reporting.md).

## `strand` is not found

The [install script](../start/install.md) links `strand` into
`~/.local/bin`, and says so when that directory is not on your `PATH`.
Add it to your `PATH` in your shell's profile and open a new terminal.
Your compositor reads its own `PATH` when it starts: if it cannot find
`strand`, start it by its full path, `~/.local/bin/strand run`.

The script stops, and changes nothing, on a system it cannot install
on: it names the missing libraries and the command that installs them,
or the glibc it needs.

## The shell does not start

`strand run` prints why it stopped and exits with an error status.

- **"cannot connect to the Wayland compositor".** Strand connects to
  the compositor that `WAYLAND_DISPLAY` names. Run it from inside your
  Wayland session, or from the compositor's startup commands.
- **"the compositor does not offer zwlr\_layer\_shell\_v1"** (or
  `wl_compositor`, or `wl_shm`). Your compositor cannot show a shell:
  Strand needs layer shell. [Install](../start/install.md#what-the-compositor-needs)
  lists what it needs and the compositors Strand is tested on.
- **"… does not exist".** The config directory is not there. The
  message names the `strand new` command that makes it, with a starter
  bar; or give `strand run` the directory you meant. A path that is a
  file is "… is not a directory".
- **"no config directory (set XDG\_CONFIG\_HOME or HOME)".** Neither
  variable names a directory to look in. Set one, or give the
  directory.

## A config that does not check

A mistake in a file is found before it runs. `strand check` prints each
problem with its file, its line and, when it can, a fix. This one is
computed by the same check when this site is built:

```strand-check
bar Top {
  edge: top; heigth: 32
  text clock.format("%H:%M")
}
```

While the shell runs, a saved file that does not check is held back:
its last good version keeps running, `strand watch` lists it under
"held back", and the overlay below shows the problem. Fix it and save
again.

## A dictionary that does not check

A [dictionary](../concepts/language.md#dictionaries) keeps one value
per key, and most of its mistakes come from another language's habits.
This one is computed when the site is built:

```strand-check
state last_win: {int: text} = []
```

Each error and its fix:

- **`syntax::dict_braces`**, "dictionaries use brackets: `[k: v]`"
  (or "`[K: V]`" for a type): a dictionary and its type are written in
  brackets, `[1: "web"]` and `[int: text]`, and an empty one is `[]`.
  Braces hold a block. On one line, the fix rewrites the braces when
  they can only be a dictionary (quoted or number keys, one `K: V`). A
  JS object with name keys, `{app: "a", label: "b"}`, or a TS object
  type, `{x: int, y: int}`, may be a record: build one by its type's
  name, `Pin(app: "a", label: "b")`, declared `type Pin { … }`.
- **`syntax::dict_item`**, "every item of a dictionary is `key:
  value`": `[1: "a", 2]` mixes entries with a list's items. Give every
  item a value, or none for a list. `[:]` is the same code, "an empty
  dictionary is `[]`".
- **`check::dict_key`**, "a dictionary cannot be keyed by `float`" (or
  another type): a key is an `int`, `text`, `path`, `bool` or an enum.
  Key by a whole number (`x.round()`) instead of a `float` or a length,
  and by a record's `id` instead of the record. A key type is never
  optional: `[int?: text]` says "cannot be keyed by `int?`".
- **`check::dict_value`**, "a dictionary's value cannot be `text?`" (or
  another optional type): a missing key already reads `null`, so drop
  the `?`. In a literal, "a dictionary's value cannot be `null`" on
  `[1: null, 2: "a"]` (Python's `{1: None}`): leave the entry out,
  which the fix does.
- **`check::dict_duplicate`**, "`1` is a key twice in this
  dictionary": remove one of the two entries.
- **`check::dict_untyped`**, "`m` is an untyped `[]`, a list read by
  position": `state m = []` is a list. To key it by text, declare it:
  `state m: [text: int] = []`.
- **`check::type_mismatch`**, "this assignment expects `int`, found
  `float`", on `m["b"] = 0.5` into `state m = ["a": 1]`: a dictionary
  keeps its literal's value type and does not widen as a list of whole
  numbers does. Declare it, `state m: [text: float] = ["a": 1]`, which
  the fix does.
- **`check::type_mismatch`**, "this assignment expects `text`, found
  `null`", on `m[k] = null`: a dictionary's values are never `null`;
  forget an entry with `m.remove(k)`, as the help says.
- **`check::dict_compound`**, "`+=` cannot change a dictionary entry,
  which may be missing": write it whole with a value for the missing
  case, `m[k] = (m[k] ?? 0) + 1`, which the fix does. The missing
  case starts from the value type's zero: `""` for text, `[]` for a
  list, `0s` for a duration, and 1 for `*=`.
- **`check::nullable`**, "a key expects `int`, but this may be null":
  `m[windows.focused?.workspace]` keys by a value that may be `null`.
  Give a fallback with `??`, or read it only when it is there.
- **`check::dict_for`**, "`for` walks a list, and this is a dictionary,
  `[int: text]`": walk its entries, `for e in m.entries { … e.key …
  e.value }`.
- **`check::dict_keyed`**, "a dictionary is already keyed, by its
  keys": drop the `key …` clause.
- **`check::dict_two_way`**, "`value` cannot be bound two-way to a
  dictionary entry": bind a `state` that starts from the entry, `state
  level = m[k] ?? 0.5`, with `value: <-> level`, and write the entry
  `on change level { m[k] = level }`.
- **`check::dict_service`**, "a service field cannot hold a dictionary
  yet" (or an action's parameter, or a command's output): read a list
  of a `type` you declare instead. A plugin's schema that declares one
  is skipped with a warning, "a schema's type cannot be a dictionary
  `[K: V]` yet".
- **`check::unknown_field`**, "dictionaries have no method `get`" (or
  `has`, `set`, `push`): read an entry as `m[k]`, which is `null` when
  the key is absent, and write one as `m[k] = v`; `has(k)` is `m[k] !=
  null`. The other words from Python and JS are named too: `delete(k)`
  and `pop(k)` are `m.remove(k)`, `items()` is `m.entries`, `size()`
  and `length` are `m.len`, and a list's method (`m.map(…)`) goes
  through `m.entries`. Each has a fix where the rewrite is mechanical.
- **`check::missing_key`**, "`get` needs a keyed collection, and `xs`
  has no key": `xs.get(k)` is a keyed list's, finding an item by its
  key. Declare the key, `state xs: [T] key field = []`, or read the
  list by position, `xs[i]`, or by a test, `xs.find(x => …)`. Only a
  `state` list of records takes a key: for a `let` of records,
  `xs.find(x => x.field == k)` is the read, and for a list of plain
  values `xs.find(x => x == k)` or `xs.contains(k)`.
- **`check::type_mismatch`**, "`key` of `pins.get` expects `text`,
  found `int`": `get` takes the list's key, the type of the field its
  `key` names.

Two mistakes show only when the shell runs, as a
[runtime error](#part-of-the-shell-stopped-responding) that `strand
watch` prints:

- **"no entry for this key"**: `m[k].f = x` writes a field of an entry
  that is not there, and changes nothing. Write the whole entry,
  `m[k] = …`, when it may be missing.
- **"a service cannot take a dictionary yet"**: a value holding a
  dictionary was on its way to a service's field or action, which
  `strand check` could not see. Pass a list of a `type` you declare.

## A settings dictionary keeps its last good value

A dictionary in a [settings file](../how-to/settings-file.md) is a
TOML table, and one bad entry keeps the whole field as it was, with a
notice naming the key:

- **`"DP-1": expected path`**: that entry's value does not have the
  value type. Quote a text or a path; write a duration as `"200ms"`.
- **`key "three": expected int`**, or **`expected one of low, normal,
  critical`**: the key does not read as the key type. An `int` key is
  written in decimal, an enum key by its variant's name.
- **`key "03": the same key as "3"`**: two keys read as one; remove
  one.

From a terminal, `strand set` writes one entry, `strand set
prefs.wallpapers.DP-1 ~/walls/a.jpg`. It refuses a whole dictionary
("`strand set` writes one entry") and `null` ("`strand set` does not
remove an entry"): a handler removes one with `m.remove(k)`.

## A file named like a built-in service stops loading

A file's exports are read as `file.name`, so a file named like a
[built-in service](../concepts/services.md) cannot export anything:
`power.open` would mean the service. A config that loaded before an
upgrade can meet this when the upgrade adds a service of that name;
`power` and `drives` are new, so a `power.strand` or `drives.strand`
that exports a state or a function no longer loads:

```text
check::redeclared

  × `power.open` cannot be read: `power` is a builtin service
   ╭─[power.strand:1:14]
 1 │ export state open = false
   ·              ──┬─
   ·                ╰── exported from a file named like it
   ╰────
  help: rename `power.strand`; its exports are read as `file.name`
```

Rename the file (`powermenu.strand`) and change what reads its exports
to the new name (`powermenu.open`). A file of that name that exports
nothing keeps loading.

## The error overlay

When a save leaves problems that stand for a quarter of a second, an
overlay opens listing every one: the problems first, then notices,
such as a state value a reload kept although its default changed. A
save that is fixed within that quarter second, as format-on-save does,
never opens it. Fixing a problem takes it off the list, and the
overlay closes when nothing is left on it.

- **Click a row** to open its file at that line. Strand runs
  `$STRAND_EDITOR` when it is set, as a template with `{file}`, `{line}`
  and `{col}` in it (`foot -e nvim +{line} {file}`). Else it runs
  `$VISUAL` or `$EDITOR` as `<editor> +<line> <file>`. A terminal
  editor (`vi`, `vim`, `nvim`, `nano`, `micro`, `kak`, `hx`, `helix`,
  `ne`, `joe`, `mg`) runs inside `xdg-terminal-exec` when it is
  installed, else `$TERMINAL -e`. With no terminal to run it in, or no
  editor set, the click runs `xdg-open` on the file.
- **The close button** dismisses the overlay and clears its notices.
  It stays away until the list changes.
- **A notice with `[reset]`** resets that state cell to its default when
  clicked.

## Part of the shell stopped responding

A runtime error, such as a value a binding cannot compute, freezes only the
component it happened in, and outlines that component in red. An error
outside any component freezes nothing. The rest of the shell keeps
running. `strand watch` prints the error as a `fault:` line with where
it happened. The next reload of a fixed config takes the outline away.

A handler that fails, such as an `await` of an action that exits
non-zero, freezes nothing. That click's run ends with the error,
`strand watch` prints it as a `fault:` line, and every handler,
that one included, runs again at its next event. A handler that fails
the same way again, such as an `every` timer on each tick, is printed
once, until a run of it completes. A `strand watch` you start later
still sees it: it prints the `fault:` line again at the handler's next
failing run.

## A surface does not appear

- **Check the config first.** `strand check` should be clean: a file
  held back is still running its old version.
- **A `window` or a popup needs `xdg_wm_base`**, and a `lock` needs
  `ext_session_lock_manager_v1`. Without them they are not shown.
  `strand report` lists every global your compositor offers.
  A missing `xdg_wm_base` is logged as a warning for windows; for
  popups, run with `STRAND_LOG=debug` to see it.

## The lock screen warns or will not lock

- **"no `strand-auth` helper is installed".** The lock checks passwords
  in a separate helper, `strand-auth`, and Strand finds none. For a
  config with a `lock`, `strand run` says so at start, or when a reload
  adds the `lock`, and `strand check` reports an error, since such a
  lock could not be unlocked but from a TTY. Both name the directory to
  put the helper in, beside `strand`. The
  [install script](../start/install.md) puts it there; if you built
  Strand yourself, [Install](../start/install.md#build-it) shows how to
  build it.
- **"… checks passwords with the `login` PAM service".** There is no
  `/etc/pam.d/strand`, so the lock uses your system's `login` setup,
  which works. [The lock screen's helper](../start/install.md#the-lock-screens-helper)
  says how to give the lock its own.
- **"the compositor does not offer ext-session-lock".** A `lock` cannot
  lock the session on this compositor.

## A panel fades in over what it opens from

On Hyprland, a panel that should grow out of a bar shows both at once,
the panel half transparent, as it opens: Hyprland is fading the new
surface in over Strand's own animation. Paste what
`strand compositor-rules` prints into your Hyprland config. If you
wrote the rule yourself, its namespace must match the whole name,
`"^strand-.*$"`: Hyprland ignores a rule that matches only the start
of one. See [Strand and your compositor](../concepts/compositors.md).

## A panel jumps or twitches as it settles

A compositor that animates a surface's moves and resizes shows each one.
Strand asks for none once a surface is on screen
([Strand and your compositor](../concepts/compositors.md#what-strand-does-about-it)),
so a jump that is still there is worth a report: `strand report` saves
one, with the compositor and its protocols ([Reporting a
glitch](../tools/reporting.md)).

## Blur shows a tint instead

`blur` needs the compositor to blur what is behind the surface, through
`ext-background-effect-v1`. Without it `blur` draws its tint fallback,
and Strand says why once per run, in the log and to `strand watch`,
naming the first surface that asked for blur. `blur_fallback: none` turns the tint off. On
Hyprland, `strand compositor-rules` prints layer rules for your config;
paste them into your Hyprland config, and Hyprland blurs Strand's
surfaces itself.

## An input method does not type in a field

Strand does not support input methods (`text-input-v3`), so Fcitx and IBus
cannot type into an `input`, and Chinese, Japanese or Korean text that needs
one cannot be typed there. Pasting works, with Ctrl+V. Dead keys and Compose
need no input method:
[Keyboard shortcuts](../how-to/keys.md#dead-keys-compose-and-input-methods)
says how they behave.

## The GPU is not used

Strand draws on the CPU by default and moves a surface to the GPU once
it animates a large area. It asks for a GPU device only where a DRM
render node (`/dev/dri/renderD*`) exists, and it needs a Vulkan driver
for it; a software driver such as lavapipe counts as none. With no
device, everything still draws on the CPU, but a `shader` node draws
nothing; [the inspector](../tools/inspector.md) says why on that
node's pane.

`strand report` shows the GPU on its `gpu:` line: `unused`,
`starting`, `up` with the adapter, or `unavailable` with the reason,
and how many devices were lost. After three lost devices in one run
the GPU stays off until Strand restarts, and the reason says so.
A build without the GPU backend gives "built without the GPU backend".

## Commands say "no running strand"

`strand watch`, `strand reload`, `strand set` and the other commands
talk to the shell through a socket, by default
`$XDG_RUNTIME_DIR/strand-<display>.sock`. "no running strand on …;
start one with `strand run`" means nothing answers there: the shell is
not running, or the command runs with another `WAYLAND_DISPLAY`,
`XDG_RUNTIME_DIR` or `STRAND_SOCKET` than the shell has.

A second `strand run` on the same display runs without a socket and
logs "no IPC socket (strand reload, strand watch): … another strand
answers on …". The commands reach the first shell.

## Where the logs go

The shell logs to its standard error: the terminal you started it in,
or wherever your compositor sends its programs' output. `STRAND_LOG`
sets the level (`error`, `warn`, `info`, `debug`, `trace`; `warn` by
default):

```sh
STRAND_LOG=debug strand run 2> strand.log
```

[Running a shell](../start/running.md#logs) says what each level adds.

## Start over: `strand reload --hard`

When the shell's state has gone wrong and a normal save does not bring
it back, `strand reload --hard` drops every state that is not `persist`
and makes every surface again, without restarting the shell.

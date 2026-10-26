# Running a shell

This page is the day-to-day loop: make a shell, start it, edit it while
it runs, and check it from a terminal. [Level 1 of the
guide](../../docs/guide/level-1.md) walks through the same loop with
the hello bar.

## Where the config lives

A shell is a directory of `.strand` files. By default it is
`$XDG_CONFIG_HOME/strand`, or `~/.config/strand` when
`XDG_CONFIG_HOME` is not set. Every command on this page that takes a
directory uses that one when you give none.

Every `.strand` file in the directory is part of one shell, down to
three directories below it. There is no entry file and nothing to
import. Files and directories whose names start with `.` are skipped,
and symlinks are followed, so a config kept in a dotfiles repository
and linked into place works. `strand check` warns about `.strand`
files that sit deeper than three directories, since they are not
loaded.

## Make a shell

```sh
strand new
```

`strand new` writes one file, `bar.strand`, the guide's hello bar, and
prints where it went:

```text
wrote /home/you/.config/strand/bar.strand
start it with `strand run`
```

It makes the directory if it is missing. It refuses a directory that
already holds anything, even a hidden file, and writes nothing there.
`strand new ~/shells/test` writes into another directory instead.

## Start it

```sh
strand run
```

`strand run` stays in the foreground until the compositor goes away or
it gets SIGINT or SIGTERM (Ctrl-C in its terminal). Before it ends it
writes the `persist` state and settings it has not saved yet. `strand run
~/shells/test` runs another directory.

If the directory has no `.strand` files yet, the shell starts with
nothing to show and waits: the first file you save there appears.

## Save to change it

Leave `strand run` running. Strand watches the config directory, and
each save changes the running shell instead of restarting it: a
changed value animates to its new one, and state you did not touch is
kept, such as an open popup or the text in a launcher. Files saved
together, as "save all" does, count as one change.

A save that does not check leaves the last good shell running. A
quarter of a second later an overlay lists the problems, and the fix
takes it away. [Troubleshooting](../help/troubleshooting.md#the-error-overlay)
says what the overlay does.

Strand keeps the last config that checked in
`$XDG_CACHE_HOME/strand/last-good` (`~/.cache/strand/last-good` by
default). A config that is broken when the shell starts runs that last
good version, with the overlay up. With no last good version yet, only
the overlay shows until you fix the file.

## Check it

```sh
strand check
```

`strand check` reads every `.strand` file of the config as one
program, type-checks it without running it, and prints what is wrong,
with the file, the line and often a "did you mean" fix. It exits with
an error status when it finds an error, so it works in scripts and
before you start the shell. It ends with a summary line:

```text
strand check: 1 file in /home/you/.config/strand: 0 errors, 0 warnings
```

`strand check ~/shells/test` checks another directory. Given a file,
`strand check bar.strand` checks it together with the rest of its
config and prints only the problems in that file.

## Format it

```sh
strand fmt
```

`strand fmt` rewrites `.strand` files in Strand's one style. With no
path it formats the config directory; it also takes directories and
files. With `--check` it writes nothing, lists each file that is not
formatted and exits with an error status when there is one:

```text
not formatted: /home/you/.config/strand/bar.strand
strand fmt: 2 files, 1 not formatted
```

A file with syntax errors is reported and left as it was. A formatted
file replaces the old one through a rename, so a symlinked file stays
a link. Your editor's format command uses the same formatter (see
[Editor setup](../tools/editor.md)).

## Watch what it does

```sh
strand watch
```

`strand watch` connects to the running shell and prints each event
until the shell ends: every reload with its time, what kind of edit it
was, which files it committed or held back and which state it kept,
reset cells, runtime faults and notices. A save of the hello bar with
a new height, then a typo in it, looks like this:

```text
reload (20.6 ms): prop
  committed: /home/you/.config/strand/bar.strand
reload (18.0 ms): no change
  held back: /home/you/.config/strand/bar.strand
  /home/you/.config/strand/bar.strand:5:14: error[check::unknown_prop]: unknown prop "heigth" on "bar"; did you mean "height"?
```

`strand watch --json` prints the same events as one JSON object per
line, for scripts. Its first line, `{"event":"watching","v":1}`, says
that no event after it is missed.

## Reload by hand

```sh
strand reload
strand reload --hard
```

You rarely need `strand reload`: saving already reloads. It rescans
the config now and prints that reload's event as `strand watch` does.

`strand reload --hard` also drops every state that is not `persist`
and makes every surface again. Use it when the shell's state has gone
somewhere you cannot get back from. While the lock screen is shown, a
hard reload waits until you unlock.

## Change values from a terminal

```sh
strand set theme.look mocha
strand set audio.sink.volume +5%
strand toggle launcher.open
strand get theme.look
```

These talk to the running shell:

- `strand set <path> <value>` writes an exported `state`, a settings
  field, a service field the service lets you write, or a token. The
  value is read as the target's type. For a service's number, a signed
  number or percentage, such as `+5%`, steps from its current value.
- `strand toggle <path>` flips an exported `bool` state.
- `strand get <path>` prints an exported value, a settings field or a
  token.

A path is `file.name` for a value a file exports (`launcher.open` is
the `export state open` of `launcher.strand`), `service.field` for a
service (`audio.sink.volume`) and `$name` for a token. `strand set
'$accent' '#ff5555'` lays a token over the theme until `strand clear
'$accent'` takes it off. Bind these commands to keys in your
compositor; levels [4](../../docs/guide/level-4.md) and
[5](../../docs/guide/level-5.md) of the guide do.

## Logs

The shell logs to its standard error. `STRAND_LOG` sets how much: a
level, one of `off`, `error`, `warn`, `info`, `debug` and `trace`, with
`warn` the default. Each line names its level and where it came from:

```text
strand: WARN strand::run::logic: no IPC socket (strand reload, strand watch): …
```

```sh
STRAND_LOG=info strand run
```

`info` adds what the shell does as it runs, such as each surface
mapped, each reload and each service starting or stopping. The topic `damage`
(`STRAND_LOG=damage`, or `STRAND_LOG=info,damage` with a level) prints
one line per painted frame with the rectangles it repainted.

## Start it with your session

Start `strand run` from your compositor's startup commands, so it runs
in the session's environment. For example:

- sway: `exec strand run`
- Hyprland, in a hyprlang config: `exec-once = strand run`
- niri: `spawn-at-startup "strand" "run"`

The shell connects to the display `WAYLAND_DISPLAY` names, and the
commands above reach it through a socket in `$XDG_RUNTIME_DIR` named
for that display (`strand-wayland-1.sock`). Its log then goes wherever
your compositor sends its programs' standard error.

A second `strand run` on the same display finds the socket taken: it
runs, but with no socket, so `strand watch` and the other commands
keep reaching the first one. To run a second shell on purpose, give it
its own `STRAND_SOCKET` and `STRAND_STATE_DIR`.

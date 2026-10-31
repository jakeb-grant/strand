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

A runtime error, such as a handler that fails, freezes only the
component it happened in, and outlines that component in red. An error
outside any component freezes nothing. The rest of the shell keeps
running. `strand watch` prints the error as a `fault:` line with where
it happened. The next reload of a fixed config takes the outline away.

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

## Blur shows a tint instead

`blur` needs the compositor to blur what is behind the surface, through
`ext-background-effect-v1`. Without it `blur` draws its tint fallback,
and Strand says why once per run, in the log and to `strand watch`,
naming the first surface that asked for blur. `blur_fallback: none` turns the tint off. On
Hyprland, `strand compositor-rules` prints layer rules for your config;
paste them into your Hyprland config, and Hyprland blurs Strand's
surfaces itself.

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

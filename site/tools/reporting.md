# Reporting a glitch

When something on screen looks wrong, such as a bar that dips for a
frame or a popup that jumps, Strand can save what led up to it, replay
it and cut out the moment for whoever fixes it. You type one command;
the rest is optional.

## Save a report

While the shell runs, type in a terminal what you saw:

```sh
strand report the clock jumped when the launcher opened
```

Strand saves a report and prints where it went, with one line to paste
to your coding agent:

```text
saved a glitch report to /home/you/.local/state/strand/reports/2026-10-26_19-51-11
paste this to your coding agent:

Strand glitch report "the clock jumped when the launcher opened" at …
```

The report is a folder under `reports/` in Strand's state directory
(`$STRAND_STATE_DIR` when it is set, else `$XDG_STATE_HOME/strand`, else
`~/.local/state/strand`), named by the local date and time, readable
only by you. Strand keeps the newest 20 and deletes older ones.

### What it holds

While it runs, Strand records what drove the shell in a fixed 256 KB
buffer in memory: pointer moves, clicks, scrolls and keys, each
service's changes, the `strand set` and `toggle` commands it was sent,
reloads, and surfaces and monitors coming and going. When the buffer is
full the oldest entries go first, so how far back it reaches depends on
how busy the shell is; a replay uses at most the last two minutes.
Nothing goes to disk until you report. The report holds that
recording, every service's state at that moment, a picture of each
surface as Strand last drew it, a copy of your config's files, and
your words.

It never holds what you type: a key that types a character is counted,
not kept. Nothing is kept while the screen is locked or an admin
password dialog or a login screen shows, and a report asked for then is
refused.

`strand set system.replay false` turns the recording off, and it stays
off across restarts. A report then holds everything but the recording.

### Without words

`strand report` with no words prints a description of the running
shell instead: the compositor's globals with their versions, the
monitors and their scales, the memory the shell uses, the GPU's status,
and each surface's backend and frame times. It is what to attach to a
bug about your hardware or compositor. `--json` prints it as JSON.

## Watch it replayed

```sh
strand replay
```

`strand replay` replays the newest report, or the folder you name, in
the evaluation sandbox. The sandbox is a separate program,
`strand-eval`, which is not part of the shell. The
[install script](../start/install.md) puts it beside `strand`. If you
built Strand yourself, build it from the checkout and put it beside
`strand` too:

```sh
cargo build --release --manifest-path eval/Cargo.toml
cp eval/target/release/strand-eval "$(dirname "$(command -v strand)")/"
```

Without it, `strand replay` says so and stops.

The replay draws on the CPU into a stand-in compositor of its own, and
never touches your session's display. It writes into the report's
`replay/` folder and prints, in one line, whether its frame at the
moment you reported matched your live shell (VERIFIED or DRIFTED), then
where `replay.txt` and the viewer are. Then it opens the viewer in your
browser. `--no-open` leaves it closed.

The viewer shows the frame large with the timeline under it: scrub
through the recorded window to find the moment.

## Make a clip

A clip hands the agent exactly the moment and the elements that look
wrong. In the viewer:

1. Drag across the timeline to set when, or press `[` at the start and
   `]` at the end while you scrub.
2. Click the elements on the frame, or drag a rectangle over them.
3. Add a note if you like.
4. Press **Copy for agent** and paste the text, or **Save clip** for
   one `.zip` with the frames cropped around those elements.

The same clip comes from a terminal, once the report has been
replayed:

```sh
strand replay clip --from -3.2s --to -2.4s --node '#card'
```

Times count back from the report (`-3.2s`, or `-3200ms`), and `--to`
defaults to the report itself. `--node` names an element by its `#id`,
its key or a `file:line` in your config. `--note` adds a note, and
`--save clip.zip` writes the `.zip`. The viewer shows this command for
what you selected.

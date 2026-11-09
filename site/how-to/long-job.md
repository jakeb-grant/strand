# Running a long job and following it

A sync, a backup or a build runs for minutes and prints as it goes. A
shell wants to show what it prints, a button to stop it, and why it
failed. In Strand that is a declared action with no result: its run
keeps the lines the program printed, the status it ended with, and,
with a block of fields, its output read as data.

## The files

```strand file=jobs.strand
// jobs.strand: a sync and a backup, and what each printed
permit exec "rsync", "restic"

// rsync holds its output back when it goes to a pipe; `stdbuf -oL`
// makes it print each line as it goes. Status 24 means some source
// files vanished while it copied: listed as no failure.
action sync(src: path, dst: path) ["stdbuf", "-oL", "rsync", "-a", "--info=progress2", "--", src, dst] ok 0, 24

// restic prints a JSON line per update with `--json`, which the block
// reads as fields. Status 3 means it saved, but could not read some
// files.
action backup(dirs: [path]) ["restic", "-r", "/mnt/usb/restic", "backup", "--json", "--", dirs] ok 0, 3 {
  message_type: text
  percent_done: float?
  snapshot: text? = snapshot_id
}
```

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      button sync.running ? "Stop sync" : "Sync" {
        on click { if sync.running { sync.stop() } else { sync("~/Music/", "/mnt/usb/music/") } }
      }
      button backup.running ? "Stop backup" : "Back up" {
        on click { if backup.running { backup.stop() } else { backup(["~/Documents", "~/Pictures"]) } }
      }
    }
    end { gap: $space.2
      if backup.running { meter backup.percent_done ?? 0 { width: 120 } }
      text backup.error ?? (backup.code == 3 ? "Saved; some files unreadable" : backup.snapshot ?? "") {
        color: $fg.muted
      }
      Log
    }
  }
}

component Log {
  state open = false
  button "Log" {
    on click { open = !open }
    popup { open: <-> open; SyncLines }
  }
}

component SyncLines {
  col { pad: $space.3; gap: $space.1; width: 640; bg: $surface; radius: $radius.lg; font: $font.mono
    text sync.error?.split("\n")?.first ?? (sync.running ? "Syncing" : "") { font: $font.ui }
    for l in sync.lines.reverse().take(16).reverse() key l.n {
      text l.text { color: l.stderr ? $fg.muted : $fg; opacity: l.open ? 0.7 : 1 }
    }
  }
}
```

## The run: `lines`, `code` and `error`

An action declared with a command and no `-> T` runs once at a time:
a call while it runs queues one more run. Its name read without a call
is its run:

- **`sync.running`** is true while its command runs, and
  **`sync.stop()`** stops it, as Ctrl-C in a terminal would.
- **`sync.lines`** is what the command printed, as a terminal would
  show it: its last 1000 lines, stdout and stderr together in the order
  Strand read them. Each `Line` has its number `n`, its `text`,
  `stderr` (which of the two streams it came on) and `open`.
- **`sync.code`** is the status it ended with, as a shell's `$?` says
  it: 130 when a `stop()`'s SIGINT ends it. A program that catches
  SIGINT ends with a status of its own: `rsync` exits 20 when stopped.
- **`sync.error`** says why it failed: the headline, then the end of
  what it printed on stderr, such as
  ``"`rsync` failed (exit status: 23): rsync: [sender] … Permission
  denied"``. Its first line is `sync.error?.split("\n")?.first`,
  which suits rsync, pacman and other programs that say why first; a
  program that prints progress on stderr before it fails (git, curl)
  leads with its progress, so read its own marker from `lines`
  instead (see [A download with a progress
  bar](download-progress.md#how-it-reads)).

`lines`, `code` and `error` are emptied when a run starts, not when it
ends, so after a failure what it printed is still there to read. A
command that outlives the panel showing it keeps running, and so does
one that outlives Strand.

**An open line** is one the program has not ended yet: a progress bar
it redraws with `\r`, or a question waiting for an answer. It is the
last of `lines`, with `open` true, and it changes as the program
redraws it, so rsync's `--info=progress2` line is one line, not
hundreds.

**stderr is not an error.** Programs write progress and notices to
stderr as often as errors (curl's bar, git's progress, `tail -F`'s
notices), so Strand only says which stream a line came on. Whether a
line is an error is the program's to say: a prefix (`error:`,
`fatal:`), a phrase, or a field of its JSON. The bar above dims stderr
lines; a config that wants errors in red tests their text with
`starts_with` or `contains`.

**The end comes in one piece.** When the command ends, its last lines,
its `code`, its `error` and `running` turning false all arrive
together, so `on change sync.running` already sees all of them.

## A block of fields: its output as data

`backup` ends with a block, as a `from listen` service does. Each line
restic prints on stdout is read as a document, and its fields fill
`backup.message_type`, `backup.percent_done` and `backup.snapshot`
beside `backup.running`, at most once a frame. stderr never reaches
them. The fields are emptied when a run starts.

`on backup.line(l)` hears every line restic prints on stdout, `l`
holding that line's fields alone, while some handler is there to hear
it. Lines on stderr are no event. restic writes each file it could not
read there, as a JSON line of its own (`{"message_type":"error",…}`):
they are in `backup.lines`, tagged `stderr`, while they are among its
last 1000 lines, and in `backup.error` when the run fails. `lines` is
not a count of them: restic prints up to 60 status lines a second with
`--json`, so in a backup of more than a few seconds the errors printed
early have left `lines` by its end. That is why the bar says "some
files unreadable" for status 3 and no number. Hearing lines never
starts the command: only a call does.

A block on an action with `-> T` is an error: that action reads its
whole output as its `T`. Its fields cannot be named `running`,
`started`, `error`, `code`, `lines` or `stop`, which are its run's; a
key path keeps the key: `err: text? = error`.

## Things to know

- **Nothing is read from the program.** Its stdin is empty, so a
  question it asks (pacman's `[Y/n]`, a password prompt) ends it, the
  question still on its open line. Give it its own flag instead
  (`--noconfirm`, `--yes`).
- **Some programs print nothing until the end** when their output goes
  to Strand. See [Output that arrives all at
  once](buffered-output.md).
- **A list parameter** is one argument per item: `backup(["~/Documents",
  "~/Pictures"])` runs `restic … -- /home/you/Documents
  /home/you/Pictures`, each `~/` expanded because the items are `path`s.
- **`ok`** lists the statuses that are no failure; see [Exit statuses
  that mean none](exit-statuses.md).
- **A command elevated with `pkexec`** runs as root, and `stop()`
  cannot signal it; a helper that elevates should pass Ctrl-C on.

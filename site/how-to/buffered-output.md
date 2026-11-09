# Output that arrives all at once

A command that prints line by line in a terminal can print nothing in
Strand for a long time, then everything at once when it ends or when a
few kilobytes have piled up. It is not Strand holding the lines back:
most programs hold their own output when it goes to a pipe instead of
a terminal, and Strand reads programs through pipes.

## The files

```strand file=feeds.strand
// feeds.strand: commands that print as they go, each told to
permit exec "pacman", "python3", "grep", "journalctl"

// A C program: `stdbuf -oL -eL` makes it print each line as it ends.
action upgrade() ["stdbuf", "-oL", "-eL", "pacman", "-Syu", "--noconfirm"]

// Python: `-u`, or `PYTHONUNBUFFERED=1` through `env`.
action convert(file: path) ["python3", "-u", "/home/me/bin/convert.py", file]
action resize(file: path) ["env", "PYTHONUNBUFFERED=1", "python3", "/home/me/bin/resize.py", file]

// grep, sed and jq have a flag of their own.
service errs from listen ["stdbuf", "-oL", "journalctl", "-f", "-o", "cat", "-p", "err"] { line: text }
action find(word: text) ["grep", "--line-buffered", "-r", "--", word, "/home/me/notes"]
```

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      button upgrade.running ? "Upgrading" : "Upgrade" { on click { if !upgrade.running { upgrade() } } }
      text upgrade.lines.last?.text ?? "" { color: $fg.muted }
    }
    end { gap: $space.2
      text errs.line { color: $fg.muted }
    }
  }
}
```

## Why it happens

A program writes through a buffer. On a terminal the buffer is
emptied at every line break, so you see each line as it is written;
on a pipe it is emptied only when it is full (a few KiB) or when
the program ends. That is what C's stdio, Python, grep, sed, awk
and jq do. A program that prints a few lines a minute into a pipe can
then show nothing for an hour.

Strand gives commands no terminal on purpose: a terminal would make
what a program prints a screen to draw rather than lines, mix stdout
and stderr into one, end the command when Strand exits, and make
programs ask questions no one can answer.

## The remedies

| Program | Remedy |
| --- | --- |
| a C program (coreutils, pacman, most others) | `["stdbuf", "-oL", "-eL", …]` before it |
| Python | `python3 -u`, or `["env", "PYTHONUNBUFFERED=1", "python3", …]` |
| grep | `--line-buffered` |
| sed | `-u` |
| jq | `--unbuffered` |
| awk | `fflush()` after each `print` |
| a tool with a format for programs | its own `--json`, `--progress` or `--line-buffered` flag |

`stdbuf` reaches only programs that write through C's stdio and leave
its buffering alone. Python buffers on its own, so it takes `-u`;
programs written in Go or Rust usually print each line as it ends
anyway.

## `permit exec` sees through `stdbuf` and `env`

`permit exec "pacman"` allows `["stdbuf", "-oL", "pacman", …]` and
`["env", "LC_ALL=C", "pacman", …]`: `strand check` checks the program
after them. It does so only when `stdbuf` is followed by nothing but
`-i`, `-o` and `-e` options (`-oL`, `-o L`, `--output=L`), and `env`
by nothing but `NAME=value` items, each written out, in either order.
Anything else (`env -i pacman`, a parameter where the program would
be) stops it, and the program checked is `stdbuf` or `env` itself, so
`permit exec "env"` would allow any program after it.

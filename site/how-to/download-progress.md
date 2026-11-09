# A download with a progress bar

curl, git and wget draw their progress on one line, redrawing it with
`\r`, and on stderr. Strand reads that the way a terminal shows it: the
line being redrawn is one line of the action's `lines`, its newest
drawing, so a bar is the last line's numbers.

## The files

```strand file=fetch.strand
// fetch.strand: a download and a git pull, each with its progress
permit exec "curl", "git"

// `-f`: an HTTP error is a failing status, so `.error` says why.
// `-#`: curl's bar, `###   37.5%`, instead of its table.
action fetch(url: text, to: path) ["curl", "-fL", "-#", "-o", to, "--", url]

// git prints its progress only to a terminal unless asked.
action pull(dir: path) ["git", "-C", dir, "pull", "--progress", "--ff-only"]

// The last number of the line being drawn: curl's percentage.
fn percent(l: Line?) -> float { (numbers(l?.text).last ?? 0) / 100 }
```

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      button fetch.running ? "Cancel" : "Download" {
        on click {
          if fetch.running { fetch.stop() } else { fetch("https://example.org/big.iso", "~/Downloads/big.iso") }
        }
      }
      if fetch.running { meter percent(fetch.lines.last) { width: 160 } }
      text fetch.error ?? "" { color: $fg.muted }
    }
    end { gap: $space.2
      button pull.running ? "Pulling" : "Pull" {
        on click { if !pull.running { pull("~/.local/share/chezmoi") } }
      }
      // git's own words: `Receiving objects:  45% (1740/3867)`.
      text pull.error?.split("\n")?.first ?? (pull.lines.last?.text ?? "") { color: $fg.muted }
    }
  }
}
```

## How it reads

- **One line, redrawn.** curl writes `\r` and its new drawing a few
  times a second. Strand treats a `\r` as a terminal does: the line
  starts again and what follows replaces it. `fetch.lines.last` is that
  line, its newest drawing, with `open` true until a line break ends
  it. A `\b` removes the character before it, as a spinner uses it,
  and colour and cursor codes are removed.
- **`numbers(t)`** gives every number in a text, in order:
  `numbers("######  37.5%")` is `[37.5]`, and
  `numbers("Receiving objects:  45% (1740/3867)")` is `[45, 1740,
  3867]`. A number is digits with an optional `.` and more digits, with
  no sign, so `5-6` reads as `[5, 6]`. It gives `[]` for null, so
  `numbers(l?.text).last ?? 0` is 0 before the first line.
- **Why it failed.** With `-f`, a 404 is curl's status 22, and
  `fetch.error` is the headline and what curl said: ``"`curl` failed
  (exit status: 22): curl: (22) The requested URL returned error:
  404"``.
- **Cancel** is `fetch.stop()`: curl gets Ctrl-C's signal and ends,
  `fetch.code` is 130, and `fetch.error` stays null, since a stop is no
  failure.
- **The progress is on stderr**, which is no error: most programs draw
  progress there so that their output stays clean. The line's `stderr`
  is true; nothing else follows from it.

## Things to know

- **A program that draws its bar only on a terminal** prints none
  when its output goes to Strand. Ask for it (`git --progress`, `curl
  -#`), or for a format meant for programs (`ffmpeg -progress
  pipe:1`, `restic --json`), which a [block of
  fields](long-job.md#a-block-of-fields-its-output-as-data) reads.
- **A download that must outlive the shell** does: the command runs
  on when Strand restarts or exits.

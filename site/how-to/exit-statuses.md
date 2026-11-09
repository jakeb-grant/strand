# Exit statuses that mean none

Some programs end with a status other than 0 when nothing is wrong:
`checkupdates` exits 2 when there are no updates, `grep` 1 when nothing
matched, `diff` 1 when the files differ. To Strand, any status but 0 is
a failure, so the answer "none" would be lost. `ok` lists the statuses
that are no failure.

## The files

```strand file=updates.strand
// updates.strand: answers whose "none" is a status
permit exec "checkupdates", "grep", "rsync", "pacman"

// 2: no updates. Nothing printed, so the call's value is `[]`, not an error;
// otherwise an update a line, a line an item.
action updates() -> [text] ["checkupdates", "--nocolor"] ok 0, 2

// 1: no line matched. `grep -c` still prints the count, 0.
action todos(file: path) -> int ["grep", "-c", "TODO", "--", file] ok 0, 1

// 1: no orphans, said in silence; pacman's errors are said on stderr, and fail.
action orphans() -> [text] ["env", "LC_ALL=C", "pacman", "-Qdtq"] ok 0, 1

// 24: some source files vanished while it copied.
action mirror(src: path, dst: path) ["rsync", "-a", "--", src, dst] ok 0, 24

// A polled command takes `ok` after its interval.
service pending from poll ["checkupdates", "--nocolor"] every 1h ok 0, 2 { list: [text] }
```

```strand file=bar.strand
// Each call's value and its `.error`, read apart.
let news = updates()
let notes = todos("~/notes/today.md")
let lost = orphans()

bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      text news.error != null ? "Update check failed" : news.pending ? "" : (news ?? []).len == 0 ? "Up to date" : join(" ", (news ?? []).len, "updates")
      text notes.error != null ? "Notes unreadable" : join(" ", notes ?? 0, "to do")
      text lost.error ?? join(" ", (lost ?? []).len, "orphans")
    }
    end { gap: $space.2
      button "Mirror" { on click { mirror("~/Music/", "/mnt/usb/music/") } }
      text mirror.code == 24 ? "Mirrored; some files vanished" : mirror.error ?? "" { color: $fg.muted }
      text pending.list.len == 0 ? "" : "Updates pending" { color: $fg.muted }
    }
  }
}
```

`?? []` alone would hide what `ok` tells apart. A failed call's value
is null too, so `(updates() ?? []).len == 0` reads "Up to date" when
checkupdates exits 2, when it fails (exit 1, `==> ERROR: Cannot fetch
updates`) and before it answers. The bar reads `news.error` first, then
the value; `news.error` holds why in the program's words (``"`checkupdates`
failed (exit status: 1): ==> ERROR: Cannot fetch updates"``) for a
shell that would rather show them. In the same way grep's 2 (the notes
file is not there) is `notes.error`, not "0 to do".

## What `ok` does

`ok 0, 2` follows the command on its line, or a polled command's
`every`. 0 must be among the statuses, so the clause reads rightly on
its own (`ok 2` is an error whose fix is `ok 0, 2`), and each is 0 to
255. A listed status is no failure:

- an action with a result reads what the program printed as its value
  (nothing printed is `""` for `text`, `[]` for a list, or null for an
  optional; read as a list, the lines it printed are an item each);
- a polled service's fields update;
- an action's `error` stays null and no notice is shown;
- its `code` still holds the status, so `mirror.code == 24` can say
  what happened.

A program ended by a signal is never ok. `ok` is for a command's
status alone: on a `from listen` service (its command ending always
means it is started again), on a D-Bus method and on a polled file it
is an error.

## When a status means two things

Use `ok` for a status the program documents as "none" or "partly":
grep's 1, diff's 1, rsync's 24, restic's 3, borg's 1. pacman and yay
exit 1 both when there is nothing to list (`pacman -Qdt` with no
orphans, `yay -Qua` with no AUR updates) and when they fail (`pacman
-Qi nosuch`). The two are told apart by what the program prints:
"none" is silence, a failure only words on stderr. A call or a polled
command whose program ends with a listed status having printed nothing
but stderr fails, so `ok 0, 1` reads both rightly with no `sh -c`
around it, as `orphans` above does. No orphans is `[]`; a pacman that
cannot read its database fails, and the call's `.error` says why in
pacman's words (``"`pacman` failed (exit status: 1): error: …"``). An action without a result (a job) keeps
what it printed in its `lines`, so its listed status is no failure
whatever it printed: rsync's 24 warns about each vanished file on
stderr.

A program's lines read as a list are its items, one a line: `->
[text]` over `checkupdates`' two lines is two updates, and its
blank lines are none. A line `[int]` cannot read (`pidof`'s `1234
5678` is two numbers on one line) is left out with a warning in the
log, and output with no line it can read is the call's `.error`, so
print one value per line. JSON keeps its reading: a program printing a JSON
array, pretty or not, gives its items. Read straight from the
program, the list keeps `ok` and `.error`. A helper script piping it
into `jq` loses the status: a pipeline exits with its last program's status, so
checkupdates' 1 and its 2 would both reach Strand as jq's 0. A helper
that keeps a pipeline needs `set -o pipefail`, and then owns the
status: its `ok` lists its own exit statuses, not checkupdates'.

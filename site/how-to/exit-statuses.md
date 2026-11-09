# Exit statuses that mean none

Some programs end with a status other than 0 when nothing is wrong:
`checkupdates` exits 2 when there are no updates, `grep` 1 when nothing
matched, `diff` 1 when the files differ. To Strand, any status but 0 is
a failure, so the answer "none" would be lost. `ok` lists the statuses
that are no failure.

## The files

```strand file=updates.strand
// updates.strand: answers whose "none" is a status
permit exec "checkupdates", "grep", "rsync"

// 2: no updates. Nothing printed, so the call's value is `""`, not an error.
action updates() -> text ["checkupdates", "--nocolor"] ok 0, 2

// 1: no line matched. `grep -c` still prints the count, 0.
action todos(file: path) -> int ["grep", "-c", "TODO", "--", file] ok 0, 1

// 24: some source files vanished while it copied.
action mirror(src: path, dst: path) ["rsync", "-a", "--", src, dst] ok 0, 24

// A polled command takes `ok` after its interval.
service pending from poll ["checkupdates", "--nocolor"] every 1h ok 0, 2 { list: text }
```

```strand file=bar.strand
bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      text (updates() ?? "") == "" ? "Up to date" : "Updates ready"
      text join(" ", todos("~/notes/today.md") ?? 0, "to do")
    }
    end { gap: $space.2
      button "Mirror" { on click { mirror("~/Music/", "/mnt/usb/music/") } }
      text mirror.code == 24 ? "Mirrored; some files vanished" : mirror.error ?? "" { color: $fg.muted }
      text pending.list == "" ? "" : "Updates pending" { color: $fg.muted }
    }
  }
}
```

## What `ok` does

`ok 0, 2` follows the command on its line, or a polled command's
`every`. 0 must be among the statuses, so the clause reads rightly on
its own (`ok 2` is an error whose fix is `ok 0, 2`), and each is 0 to
255. A listed status is no failure:

- an action with a result reads what the program printed as its value
  (nothing printed is `""` for `text`, `[]` for a list, or null for an
  optional);
- a polled service's fields update;
- an action's `error` stays null and no notice is shown;
- its `code` still holds the status, so `mirror.code == 24` can say
  what happened.

A program ended by a signal is never ok. `ok` is for a command's
status alone: on a `from listen` service (its command ending always
means it is started again), on a D-Bus method and on a polled file it
is an error.

## When not to use it

Use `ok` only for a status the program documents as "none" or
"partly": grep's 1, diff's 1, rsync's 24, restic's 3, borg's 1. When
one status means two things, `ok` hides one of them. pacman exits 1
both when `-Qdtq` finds no orphans and when `-Qi nosuch` fails, so `ok
0, 1` on a `pacman -Qi` would make a missing package look like an
empty answer. There, leave the status a failure: the call's `.error`
then says why in pacman's words (``"`pacman` failed (exit status: 1):
error: package 'nosuch' was not found"``), and a shell can show it.

A program's lines are one text, not a list: `-> [text]` over
`checkupdates`' two lines is one item holding both. Where a list is
wanted, a helper script prints it as a JSON array
(`checkupdates --nocolor | jq -Rn '[inputs]'`).

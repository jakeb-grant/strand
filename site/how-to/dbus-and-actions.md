# Calling a D-Bus method or a declared action

Strand's [built-in services](../concepts/services.md) cover the usual
things. For the rest, a config declares what it needs in a line or two:
a D-Bus object's properties and methods, or a command to run. Handlers
then call them like functions, and `strand check` checks every call.

## The files

```strand file=services.strand
// services.strand: what no built-in service does
permit exec "hyprctl", "qalc", "wf-recorder"

service ppd from dbus system "net.hadess.PowerProfiles" {
  profile: text rw = ActiveProfile
  action hold(profile: text, reason: text, app: text) = HoldProfile
}

action dispatch(ws: int) ["hyprctl", "dispatch", "workspace", ws]
action calc(q: text) -> text ["qalc", "-t", q]
action rec(file: path) ["wf-recorder", "-f", file]
```

```strand file=bar.strand
state query = ""

bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { gap: $space.2
      button "1" { on click { dispatch(1) } }
      button "2" { on click { dispatch(2) } }
    }
    center { gap: $space.2
      input { width: 160; text: <-> query; placeholder: "2 + 2" }
      text calc(query) ?? "" { color: $fg.muted }
    }
    end { gap: $space.2
      button ppd.profile { on click { ppd.hold("performance", "a long build", "strand") } }
      button rec.running ? "Stop" : "Record" {
        on click { if rec.running { rec.stop() } else { rec("/tmp/clip.mp4") } }
      }
    }
  }
}
```

Here is the bar with the power profile given as `balanced` and `calc`
answered as qalc would answer it. "2 + 2" is typed, then Record is
clicked:

```example dbus
```

## A D-Bus object as a service

`service ppd from dbus system "net.hadess.PowerProfiles" { … }` is the
power profiles daemon on the system bus.

- **`profile: text rw = ActiveProfile`** is its `ActiveProfile`
  property, read as `ppd.profile` and followed live. `rw` makes it
  writable: `ppd.profile = "power-saver"` in a handler, a `<->`
  binding or `strand set ppd.profile balanced` sets the property.
- **`action hold(…) = HoldProfile`** declares one of the object's
  methods. A handler calls it as `ppd.hold("performance", "a long
  build", "strand")`; each parameter is one argument of the method.
  The call does not hold the service up, and its answer is waited for
  however long the daemon takes.
- **It is checked against the object's introspection**, like its
  properties: a misspelt method or property is an error with a
  did-you-mean, and so is a wrong number of arguments. When Strand
  cannot read the object's methods, they are not checked and a warning
  says so.

A service declared this way starts when the shell first reads it and
stops 5 s after its last reader goes, as the built-in ones do.

## A declared action runs a command

`action dispatch(ws: int) ["hyprctl", "dispatch", "workspace", ws]`
declares a command once, at the top level of a file. `dispatch(1)` in a
handler runs it.

- **The command is a list.** Its first item is the program, written
  out; the others are texts and the action's parameters. Each
  parameter is exactly one argument, never split on spaces, and no
  shell stands between Strand and the program.
- **`permit exec` lists the programs a config may run.** An action
  whose program it does not allow is a load error. `strand check`
  lists every action with its command, so you can see what a config
  runs before you run it.
- **It runs detached.** The handler does not wait for it. If the
  program cannot start, or exits with an error, Strand logs it,
  `strand watch` shows a notice, and `rec.error` says why, so the shell
  can show it too.

## An action that gives back a value

`action calc(q: text) -> text ["qalc", "-t", q]` has a result type,
so its command's output is its value. In a binding, `calc(query)` is
an `Async<text>`, as `apps.search(query)` is:

- it runs when first read, and again each time `query` changes,
  ending a run still going;
- it has 10 seconds, as a polled command has;
- the last answer stays shown while the next one runs (`.pending`),
  and `.error` says why a run failed;
- `?? ""` gives the empty text until there is an answer.

A `text` result is what the program printed, trimmed. A number, a
`bool`, a colour or an enum is read from the printed text; a record
`type` of your own or a list is read from JSON, and a record also from
`key=value` lines. In a handler, `let r = await calc(q)` waits for it.

## An action you can stop

An action's name also reads as its run. `rec.running` is true while
its command runs, and `rec.stop()` in a handler stops it as Ctrl-C in
a terminal would (`SIGINT`, then `SIGTERM` and `SIGKILL` if the program
will not end), so a recorder finishes its file. One button starts and
stops it, with no pid file.

## From a terminal

A service's actions can be called from a terminal too, with
`strand call`:

```sh
strand call notifications.clear
```

For a service no declaration can describe, a plugin can add one: a
binary built from Strand's Rust library, git-only and marked unstable.
The guide's [plugins page](../../docs/guide/plugins.md) shows how.

# Services

A **service** is the system as values: the battery, the audio devices,
the workspaces, the clock. You read its fields in expressions, as you
read state, and the screen follows them as they change. You never poll
or subscribe.

```strand fragment
text pct(battery.percent)                        // follows the battery
icon audio.sink.icon                             // the default output's icon
for ws in workspaces.all { text ws.name }        // every workspace, live
```

## Built-in services

Strand has 28 built-in services. Each one is in the
[service reference](../../docs/reference/services.md), with every
field, function, action and event.

| Service | What it is |
| --- | --- |
| `apps` | Installed apps, from their desktop entries, with search |
| `audio` | Audio devices and the programs' streams, from PipeWire |
| `auth` | The lock screen's password check, through PAM |
| `battery` | The battery, from UPower |
| `bluetooth` | Bluetooth adapters and devices, from BlueZ |
| `brightness` | The screen's backlight |
| `calendar` | Month grids for calendars |
| `clipboard` | The clipboard, and a history of what you copied |
| `clock` | The date and time |
| `cpu`, `memory`, `disk` | Processor load, memory use, disk usage |
| `greetd` | A login screen, when Strand runs as greetd's greeter |
| `idle` | Whether you are away |
| `keyboard` | Lock keys, layouts and modifiers |
| `media` | The active media player, over MPRIS |
| `network` | The network, from NetworkManager or iwd |
| `notifications` | The notification server, which the shell is |
| `outputs` | The monitors' configuration |
| `polkit` | The admin password dialog, as the session's polkit agent |
| `screens` | The monitors |
| `session` | The login session and power: lock, suspend, log out |
| `system` | The desktop's appearance settings: dark, accent, contrast |
| `tray` | The system tray |
| `video` | The programs using a camera or casting a screen |
| `windows` | The open windows |
| `wm` | The compositor: its name and binding mode |
| `workspaces` | The workspaces |

## Started when read

A service starts when the shell first reads it, is shared by
everything that reads it, and stops 5 seconds after its last reader
goes away or is no longer shown. A shell that never reads `bluetooth`
never talks to BlueZ. One that reads it only in a closed popup stops
it a few seconds after the popup closes.

A service also wakes the shell only as often as what you read changes.
`clock.format("%H:%M")` wakes once a minute, and `"%H:%M:%S"` once a
second.

## Reading, writing and acting

- **Fields** are read anywhere: `battery.percent`, `audio.sink.volume`.
  Items in a list, such as each workspace or notification, carry their
  own key, so a `for` over them keeps each copy's state as the list
  changes.
- **Writable fields**, marked `rw` in the reference, are assigned in a
  handler or bound with `<->`: `audio.sink.volume = 0.5`,
  `slider { value: <-> audio.sink.volume }`.
- **Actions** do something: `ws.focus()`, `n.dismiss()`,
  `h.app.launch()`. They run only in handlers, and their errors are
  values: a failing action never stops the shell.
- **Events** are delivered one by one, in order:
  `on notifications.received(n) { … }`.

Where a daemon may be missing (NetworkManager, BlueZ, logind, the
backlight), the service's `available` field says whether it answers,
so a config can hide what cannot work. A missing daemon never stops
the shell.

## Services of your own

What the built-ins do not cover, a config declares in a line or two:

```strand fragment
service ppd from dbus system "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }
service temp from poll ["sensors", "-j"] every 5s { cpu: float = package }
permit exec "sensors"
```

| Source | Reads |
| --- | --- |
| `from dbus` | A D-Bus object's properties, followed live, and its methods as actions |
| `from file` | A file (JSON, `key=value` lines or plain text), again each time it changes |
| `from dir` | A directory's files, again each time one is added, removed or changed |
| `from listen` | The lines a command prints, as they come |
| `from poll … every T` | A command's output, or a file, every `T` while a reader is shown |

Their fields are checked like the built-ins'. A `from dbus` service is
checked against the object's introspection, so a misspelt property is
an error with a did-you-mean. Running a command needs `permit exec`,
naming the programs allowed. A custom service starts and stops as the
built-ins do, and saving a change to its declaration restarts only
that service.

## Actions as a fallback

When there is no service for something, a declared **action** runs a
command, and a `from dbus` service can call the object's methods:

```strand fragment
permit exec "hyprctl"
action dispatch(ws: int) ["hyprctl", "dispatch", "workspace", ws]
```

`dispatch(3)` in a handler runs it, with each parameter as exactly one
argument and no shell in between. An action with a result type, such
as `-> text`, gives the command's output as a value.
[Calling a D-Bus method or a declared action](../how-to/dbus-and-actions.md)
shows each form with a whole config.

For what no declaration can describe, a plugin adds a service in Rust;
see the guide's [plugins page](../../docs/guide/plugins.md).

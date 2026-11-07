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

Strand has 30 built-in services. Each one is in the
[service reference](../../docs/reference/services.md), with every
field, function, action and event.

| Service | What it is |
| --- | --- |
| `apps` | Installed apps, from their desktop entries, with search |
| `audio` | Audio devices, their cards' profiles and the programs' streams, from PipeWire |
| `auth` | The lock screen's password check, through PAM |
| `battery` | The battery, from UPower |
| `bluetooth` | Bluetooth adapters, devices and pairing requests, from BlueZ |
| `brightness` | The screen's backlight and the keyboard's |
| `calendar` | Month grids for calendars |
| `clipboard` | The clipboard, and a history of what you copied |
| `clock` | The date and time |
| `cpu`, `memory`, `disk` | Processor load, memory use, disk usage |
| `drives` | Removable drives: mount, unmount, eject, from UDisks2 |
| `greetd` | A login screen, when Strand runs as greetd's greeter |
| `idle` | Whether you are away |
| `keyboard` | Lock keys, layouts and modifiers |
| `media` | The media players, over MPRIS |
| `network` | The network, from NetworkManager or iwd |
| `notifications` | The notification server, which the shell is |
| `outputs` | The monitors' configuration |
| `polkit` | The admin password dialog, as the session's polkit agent |
| `power` | The power profile, from power-profiles-daemon |
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
  changes, and `get` finds one by it: `windows.all.get(id)` is the
  window with that id, or `null` once it has closed.
- **Writable fields**, marked `rw` in the reference, are assigned in a
  handler or bound with `<->`: `audio.sink.volume = 0.5`,
  `slider { value: <-> audio.sink.volume }`.
- **Actions** do something: `ws.focus()`, `n.dismiss()`,
  `h.app.launch()`. They run only in handlers, and their errors are
  values: a failing action never stops the shell.
- **Events** are delivered one by one, in order:
  `on notifications.received(n) { … }`.

Where a daemon may be missing (NetworkManager, BlueZ, logind,
UDisks2, power-profiles-daemon, the backlight), the service's
`available` field says whether it answers, so a config can hide what
cannot work. A missing daemon never stops the shell.

## A control panel and a pairing dialog

A panel of controls that would otherwise need a tool each: the power
profile, which output each app plays to, the default output's modes (a
headset's call and high-fidelity profiles), and the drives plugged in:

```strand file=controls.strand
// controls.strand: the power profile, where each app plays, the
// headset's mode and the drives plugged in
export state open = false

panel Controls {
  anchor: top_right; keyboard: on_demand; open: <-> open
  bg: $surface; color: $fg; font: $font.ui
  state why = ""
  on drives.failed(device, error) { why = error }
  col { gap: $space.2; pad: $space.3
    if power.available {
      row { gap: $space.2
        for p in power.profiles key p {
          button match p { power_saver => "Saver", balanced => "Balanced", performance => "Fast" } {
            when p == power.profile { bg: $accent }
            on click { power.profile = p }
          }
        }
      }
    }
    for st in audio.streams.filter(s => s.direction == output) key st.id {
      row { gap: $space.2
        text st.app
        for dev in audio.sinks key dev.id {
          button dev.description {
            when st.device == dev.id { bg: $accent }
            on click { st.move_to(dev) }
          }
        }
      }
    }
    for p in audio.sink.profiles key p.name {
      button p.description {
        when p.active { bg: $accent }
        on click { p.select() }
      }
    }
    for d in drives.all key d.device {
      row { gap: $space.2
        icon d.icon
        text d.name
        for v in d.volumes key v.device {
          button v.mounted ? "Unmount" : "Mount " + (v.label ?? bytes(v.size)) {
            on click { if v.mounted { v.unmount() } else { v.mount() } }
          }
        }
        if d.can_eject { button "Eject" { on click { d.eject() } } }
      }
    }
    text why { color: $fg.muted }
  }
}
```

- **`power.profile = p`** asks power-profiles-daemon to switch. Only
  the profiles the machine offers are in `power.profiles`. If the daemon
  settles on another profile right after, Strand asks once more.
- **`st.move_to(dev)`** sends one app's sound to `dev`, where it stays
  when the default output changes; `st.device` is the output it plays
  to now.
- **`p.select()`** switches the card's profile, a headset between its
  call and high-fidelity modes.
- **`d.eject()`** unmounts every filesystem on the drive, then ejects
  the medium or powers the drive off, so it is safe to pull. A refusal,
  such as a file still open on it, arrives as `drives.failed`.

A device being paired may ask for a PIN or a passkey, or ask you to
confirm a code. While a config reads `bluetooth.request`, Strand is
BlueZ's pairing agent and the question arrives there, so the dialog is
an ordinary panel that opens while there is a request:

```strand file=pair.strand
// pair.strand: what a Bluetooth device asks as it pairs
panel Pair {
  anchor: center; keyboard: exclusive; open: bluetooth.request != null
  bg: $surface; color: $fg; font: $font.ui
  state typed = ""
  col { gap: $space.2; pad: $space.3
    text bluetooth.request?.device?.name ?? ""
    text match bluetooth.request?.kind ?? confirm {
      pin => "Type the PIN"
      passkey => "Type the passkey"
      display => "Type this on the device"
      confirm => "Does the device show this code?"
      service => "Allow " + (bluetooth.request?.service ?? "")
    }
    text bluetooth.request?.code ?? ""
    if bluetooth.request?.kind == pin || bluetooth.request?.kind == passkey {
      input { type: password; focus: true; text: <-> typed
        on activate { bluetooth.request?.respond(typed); typed = "" }
      }
    }
    row { gap: $space.2
      if bluetooth.request?.kind == confirm || bluetooth.request?.kind == service {
        button "Allow" { on click { bluetooth.request?.accept() } }
      }
      button "Refuse" { on click { bluetooth.request?.reject() } }
    }
  }
}
```

- **`?.` at each step.** `bluetooth.request` is null while nothing
  asks, so `bluetooth.request?.device` may be null too, and the name
  is `bluetooth.request?.device?.name`.
- **`respond(typed)`** answers a PIN or a passkey, from a
  `type: password` input; **`accept()`** allows a code to confirm or a
  service the device wants, and **`reject()`** refuses any request.
- **The panel's `open:` reads `request`** even while the panel is
  closed, so Strand stays the agent and the dialog can open. A shell
  that never reads it leaves pairing to the system's own agent.

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
| `from file` | A file (JSON, XML, `key=value` lines or plain text), again each time it changes |
| `from dir` | A directory's files, again each time one is added, removed or changed |
| `from listen` | The lines a command prints, as they come |
| `from poll … every T` | A command's output, or a file, every `T` while a reader is shown |

Their fields are checked like the built-ins'. A field cannot be a
[dictionary](language.md#dictionaries) yet: declare a `type` for an
object or a D-Bus map with known keys, and a list of one for many. A
`from dbus` service is
checked against the object's introspection, so a misspelt property is
an error with a did-you-mean. Running a command needs `permit exec`,
naming the programs allowed. A custom service starts and stops as the
built-ins do, and saving a change to its declaration restarts only
that service.

A document can be XML as well as JSON: an RSS or Atom feed fetched by
`curl`, or a tool's XML output. Its elements are records whose fields
are their attributes and child elements, and the `type` you declare
decides whether a field takes one element or every one of its name. A
`Date` field reads dates as documents write them: RFC 3339, RFC 2822
and Unix time. [Data from the web and from tools](../how-to/data-from-the-web.md)
has recipes for feeds, a JSON API and a scan.

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

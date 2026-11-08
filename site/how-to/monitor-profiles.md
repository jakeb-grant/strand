# Monitor profiles

Arrange your monitors from your shell: a saved profile for each set of
monitors you use, kept in a settings file you can edit by hand, and a
best guess for a set you have not saved yet. Nothing here is a
monitor feature of its own. It is a handful of general pieces put
together:

- **`outputs`** lists every connected monitor, on or off
  (`outputs.all`), and sets them together (`outputs.apply`). Each
  monitor's `id` is what it is (its make, model and description, which
  usually carries its serial number), whatever port it is plugged into.
- **`ready`**, which every service has, says when `outputs` has
  answered, so the layout is applied at start and again when the
  compositor comes back ([Events and timers](../concepts/state.md#events-and-timers)).
- **A [settings file](settings-file.md)** holds the profiles, one
  `[[…]]` section per monitor.
- **The `file` element** writes your compositor's monitor lines from
  `outputs.all` ([another app's config file](config-for-another-app.md)).
- **`ws.move_to(screen)`** puts workspaces on the monitors you want.
- **A `from dbus` service** follows the laptop's lid
  ([a D-Bus method or an action](dbus-and-actions.md#the-laptops-lid)).

The files below are one config: drop them into `~/.config/strand`
beside your bar.

## A best guess

Monitors left to right in the order the compositor lists them, the
laptop's own panel last, each at its preferred mode:

```strand file=layout.strand
// layout.strand: a best guess, left to right, the laptop's panel last

// The laptop's own panel, which goes last.
fn inside(o: Output) -> bool { o.name.starts_with("eDP") }
// Every monitor is on (the lid, below, turns the panel off).
fn lit(o: Output) -> bool { true }

// How wide a monitor is in the layout: a turned one is as wide as its
// mode is tall, and a scaled one narrower.
fn wide(o: Output) -> int {
  let m = o.preferred ?? o.mode
  let w = match o.transform {
    rotate_90 => m?.height ?? 0
    rotate_270 => m?.height ?? 0
    flipped_90 => m?.height ?? 0
    flipped_270 => m?.height ?? 0
    _ => m?.width ?? 0
  }
  (w / o.scale).round()
}

let order = outputs.all.filter(o => lit(o)).sort_by(o => inside(o))
export let guess = outputs.all.map(o => OutputChange(
  name: o.name, enabled: lit(o), mode: lit(o) ? o.preferred : null,
  x: order.take(order.index_of(p => p.name == o.name) ?? 0).sum(p => wide(p)), y: 0))
```

- **`sort_by`** keeps the order it finds among equals, and `false`
  comes before `true`, so every monitor but the laptop's panel
  (`eDP-1`) keeps its place and the panel goes last.
- **`x`** is the sum of the widths of the monitors before this one:
  `take` keeps those, `index_of` says how many there are, and `sum`
  adds up what `wide` gives for each. A 4K monitor at scale 1.5 turned
  on its side is 2160 / 1.5 = 1440 wide, so the next one starts at
  1440.
- **`OutputChange`** says what to change: a field left out (here
  `scale` and `transform`) stays as it is.

The list operations are the ones every list has (the
[reference](../../docs/reference/functions.md) lists them); there is no
layout operation.

## A profile for each set of monitors

Save the layout you arranged (by hand, or with your compositor's own
tool) for the monitors connected now, and get it back whenever the same
monitors are connected again. A set nobody saved a profile for gets the
best guess:

```strand file=monitors.strand
// monitors.strand: a saved profile for each set of monitors, else the best guess
type Mon {
  id: text; on: bool; width: int; height: int; refresh: float
  x: int; y: int; scale: float; transform: OutputTransform
}
type Profile { mons: [Mon] }
state saved from "monitors.toml" { profiles: [text: Profile] = [] }

// The monitors connected now, by what each one is, whatever port it is on.
let setup = outputs.all.map(o => o.id).sort_by(i => i).join(" + ")

fn kept(o: Output) -> Mon {
  Mon(id: o.id, on: o.enabled, width: o.mode?.width ?? 0,
    height: o.mode?.height ?? 0, refresh: o.mode?.refresh ?? 0,
    x: o.x, y: o.y, scale: o.scale, transform: o.transform)
}
fn placed(m: Mon) -> OutputChange {
  OutputChange(name: outputs.all.find(o => o.id == m.id)?.name ?? "",
    enabled: m.on,
    mode: m.on ? OutputMode(width: m.width, height: m.height, refresh: m.refresh) : null,
    x: m.x, y: m.y, scale: m.scale, transform: m.transform)
}
let mons = saved.profiles[setup]?.mons
let chosen = mons == null ? layout.guess : (mons ?? []).map(m => placed(m))

// At start, when the monitors change, when a profile is saved or edited,
// and when the compositor comes back; after a dock has settled.
on change chosen, outputs.ready after 1500ms {
  if outputs.ready { outputs.apply(chosen) }
}

bar Displays {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.3; align: center
    text mons == null ? "Best guess" : "Saved profile"
    button "Save this profile" {
      on click { saved.profiles[setup] = Profile(mons: outputs.all.map(o => kept(o))) }
    }
  }
}
```

- **`setup`** names the set of monitors connected now by their `id`s,
  in a fixed order, so the same monitors are the same set whatever port
  they are on, and two monitors of one model tell apart by their
  descriptions. It is the key of the
  [dictionary](../concepts/language.md#dictionaries) of saved profiles.
- **Save this profile** stores the monitors as they are now under that
  key. Arrange them some other way first (your compositor's settings,
  `wlr-randr`, a config line) and save once for each place you work.
- **`on change chosen, outputs.ready`** applies the chosen layout when
  `outputs` has first answered, when the set of monitors changes, when
  a profile is saved or edited, and after the compositor comes back.
  `on change` never runs for the first value, and `ready` turns true
  once the monitors are in, as a change, so naming it is what applies a
  layout when the shell starts.
- **`after 1500ms`** waits until nothing has changed for a second and
  a half, so a dock that brings up its monitors one by one is arranged
  once. Moving a monitor by hand changes nothing `chosen` reads, so it
  stays where you put it until you save it.

## The file

The settings file holds one section per set of monitors, each monitor
a `[[…]]` table under it. It is yours to edit and keep with your
dotfiles: an edit applies live, and Strand writes a saved profile back
into the sections the file has, keeping the comments on every other one.

```toml
# Monitor profiles, one per set of monitors.

# At the desk: the Dell on its side, left of the laptop.
[profiles."BOE | 0x0BCA | BOE 0x0BCA + Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13"]
[[profiles."BOE | 0x0BCA | BOE 0x0BCA + Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13".mons]]
id = "Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13"
on = true
width = 3840
height = 2160
refresh = 59.997
x = 0
y = 0
scale = 1.5
transform = "rotate_90"

[[profiles."BOE | 0x0BCA | BOE 0x0BCA + Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13".mons]]
id = "BOE | 0x0BCA | BOE 0x0BCA"
on = true
width = 1920
height = 1200
refresh = 60.0
x = 1440
y = 0
scale = 1.25
transform = "normal"
```

A key a monitor's table does not have, `scael = 1.5`, keeps the last
good profiles with a notice naming it, so a typo never reaches your
monitors.

## A file for your compositor

`outputs.apply` arranges the monitors while the shell runs. To have
your compositor start with the same arrangement, write its monitor
lines from `outputs.all`, here for Hyprland (`source =
~/.config/hypr/monitors.conf` in `hyprland.conf`):

```strand file=hyprland.strand
// hyprland.strand: the monitors as they are now, for Hyprland's next start
fn turned(t: OutputTransform) -> int {
  let n = match t {
    normal => 0
    rotate_90 => 1
    rotate_180 => 2
    rotate_270 => 3
    flipped => 4
    flipped_90 => 5
    flipped_180 => 6
    flipped_270 => 7
  }
  n
}
fn line(o: Output) -> text {
  let m = o.mode
  o.enabled && m != null
    ? join(", ", "monitor = " + o.name,
      join("@", join("x", m?.width, m?.height), m?.refresh),
      join("x", o.x, o.y), o.scale, "transform", turned(o.transform))
    : "monitor = " + o.name + ", disable"
}
file "~/.config/hypr/monitors.conf" {
  text: "# Written by Strand from outputs.all\n" + outputs.all.map(o => line(o)).join("\n") + "\n"
}

// The same monitors as JSON, by id, for a script of your own.
file "~/.cache/monitors.json" {
  text: json(outputs.all.filter(o => o.enabled).map(o => [o.id, o.name]))
}
```

- **It follows the monitors.** The text reads `outputs.all`, so the
  file is written again whenever a monitor is moved, turned or
  plugged in, at most twice a second.
- **It waits for `outputs`.** Until the monitors are in, `outputs.all`
  is empty, so the file is not written at all and the last session's
  lines stay.
- **Names, not ids, in code.** The connector names are what the file
  is for now, and it is rewritten when they change. The `id`s, which
  hold text from the monitor itself, go into JSON, where `json` quotes
  them.

For sway, the line is `output <name> mode <w>x<h>@<r>Hz pos <x> <y>
scale <s> transform <degrees>`, with the same parts.

## Workspaces on their monitors

Once the monitors are arranged, put workspaces where you want them:
`ws.move_to(screen)` moves a workspace, with its windows, to a screen,
on Hyprland, sway and niri.

```strand file=workspaces.strand
// workspaces.strand: workspaces 1 to 5 on the big monitor while it is on
let big = screens.all.find(s => s.id.starts_with("Dell Inc. | DELL U2720Q"))
on change big?.name after 1500ms {
  for s in screens.all.filter(s => s.name == big?.name) {
    for ws in workspaces.all.filter(w => (number(w.name) ?? 0) <= 5) { ws.move_to(s) }
  }
}
```

- **`big`** finds the monitor by its `id` (a screen has the same `id`
  as its output), so it is found on whichever port it is plugged into.
  `on change big?.name` places the workspaces when it is connected or
  moves to another port, not on every switch: a workspace you move by
  hand stays where you put it.
- **`ws.move_to(s)`** leaves the focus on the workspace that had it,
  and a workspace on that screen already is left alone, so placing
  them again does nothing. Each call is its own request, so a `for`
  moves every one.
- **A workspace not made yet** is not moved: your compositor's own
  rules place it when it is made (`workspace 3 output DP-3` in sway,
  a workspace rule in Hyprland).

## The laptop's lid

The laptop's panel can go off while its lid is shut on a dock. UPower
follows the lid, so declare a service for it and replace `lit` in
`layout.strand`:

```strand fragment
service upower from dbus system "org.freedesktop.UPower" {
  lid_closed: bool = LidIsClosed
}

// The panel is off while its lid is shut and another monitor is there.
fn lit(o: Output) -> bool {
  !inside(o) || !upower.lid_closed || outputs.all.len == 1
}
```

The best guess then leaves the panel out of the row and turns it off,
and turns it back on when the lid opens. `chosen` reads `lit` through
the guess, so a profile you saved is left as it is: save one with the
panel off (`on = false`) for the dock instead.

## Variations

- **The panel off when docked.** Save the docked profile with the
  laptop's panel off (`on = false`): the saved profile turns it off
  whenever that set is connected.
- **Two monitors of one model** have different `id`s when their
  descriptions differ (most carry the serial number). Two that are
  described alike get ` #2` on the second, the monitors that are on
  numbered first.
- **Only a profile, never a guess.** Apply only when `mons != null`,
  and a set nobody saved stays as the compositor arranged it.

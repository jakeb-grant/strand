# Monitor layouts

Arrange your monitors from your shell: a best guess for a set of
monitors you have not set up yet, and a layout you save for each set
you use, kept in a settings file you can edit by hand. Both use
`outputs`: `outputs.all` lists every connected monitor, on or off, and
`outputs.apply(changes)` sets them together, as one configuration.

## A best guess

Monitors left to right in the order the compositor lists them, the
laptop's own panel last, each at its preferred mode:

```strand file=layout.strand
// layout.strand: a best guess, left to right, the laptop's panel last

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

let order = outputs.all.sort_by(o => o.name.starts_with("eDP"))
export let guess = order.map(o => OutputChange(
  name: o.name, enabled: true, mode: o.preferred,
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

Nothing here is a special layout feature: the list operations are the
ones every list has (the [reference](../../docs/reference/functions.md)
lists them).

## A layout for each set of monitors

Save the layout you arranged (by hand, or with your compositor's own
tool) for the monitors connected now, and get it back whenever the same
monitors are connected again. A set nobody saved a layout for gets the
best guess:

```strand file=monitors.strand
// monitors.strand: a saved layout for each set of monitors, else the best guess
type Mon {
  id: text; on: bool; width: int; height: int; refresh: float
  x: int; y: int; scale: float; transform: OutputTransform
}
type Layout { mons: [Mon] }
state saved from "monitors.toml" { layouts: [text: Layout] = [] }

// A monitor by what it is, not by the port it is plugged into.
fn ident(o: Output) -> text { o.make + " " + o.model }
let setup = outputs.all.map(o => ident(o)).sort_by(i => i).join(" + ")

fn kept(o: Output) -> Mon {
  Mon(id: ident(o), on: o.enabled, width: o.mode?.width ?? 0,
    height: o.mode?.height ?? 0, refresh: o.mode?.refresh ?? 0,
    x: o.x, y: o.y, scale: o.scale, transform: o.transform)
}
fn placed(m: Mon) -> OutputChange {
  OutputChange(name: outputs.all.find(o => ident(o) == m.id)?.name ?? "",
    enabled: m.on,
    mode: m.on ? OutputMode(width: m.width, height: m.height, refresh: m.refresh) : null,
    x: m.x, y: m.y, scale: m.scale, transform: m.transform)
}
let mons = saved.layouts[setup]?.mons
let chosen = mons == null ? layout.guess : (mons ?? []).map(m => placed(m))

// A monitor plugged in or out: wait for a dock's monitors to settle.
on change setup after 1500ms { outputs.apply(chosen) }

bar Displays {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  row { pad: 0, $space.3; gap: $space.3; align: center
    text mons == null ? "Best guess" : "Saved layout"
    button "Arrange" { on click { outputs.apply(chosen) } }
    button "Save this layout" {
      on click { saved.layouts[setup] = Layout(mons: outputs.all.map(o => kept(o))) }
    }
  }
}
```

- **`setup`** names the set of monitors connected now, by make and
  model, in a fixed order, so the same monitors are the same set
  whatever port they are on. It is the key of the
  [dictionary](../concepts/language.md#dictionaries) of saved layouts.
- **Save this layout** stores the monitors as they are now under that
  key. Arrange them some other way first (your compositor's settings,
  `wlr-randr`, a config line) and save once for each place you work.
- **`on change setup`** applies the saved layout, or the best guess,
  when the set changes. `on change` never runs for the first value, so
  a layout is not applied when the shell starts: **Arrange** applies
  it at once.
- **`after 1500ms`** waits until the set has stayed the same for a
  second and a half, so a dock that brings up its monitors one by one
  is arranged once.

## The file

The [settings file](settings-file.md) holds one section per set of
monitors, each monitor a `[[…]]` table under it. It is yours to edit
and keep with your dotfiles: an edit applies live, and Strand writes a
saved layout back into the sections the file has, keeping the comments
on every other one.

```toml
# Monitor layouts, one per set of monitors.

# At the desk: the Dell on its side, left of the laptop.
[layouts."BOE 0x0BCA + Dell Inc. DELL U2720Q"]
[[layouts."BOE 0x0BCA + Dell Inc. DELL U2720Q".mons]]
id = "Dell Inc. DELL U2720Q"
on = true
width = 3840
height = 2160
refresh = 60.0
x = 0
y = 0
scale = 1.5
transform = "rotate_90"

[[layouts."BOE 0x0BCA + Dell Inc. DELL U2720Q".mons]]
id = "BOE 0x0BCA"
on = true
width = 1920
height = 1200
refresh = 60.0
x = 1440
y = 0
scale = 1.25
transform = "normal"
```

A key a monitor's line does not have, `scael = 1.5`, keeps the last
good layouts with a notice naming it, so a typo never reaches your
monitors.

## Variations

- **The panel off when docked.** Save the docked layout with the
  laptop's panel off (`on = false`): the saved layout turns it off
  whenever that set is connected.
- **Two monitors of one model** share an `id` here, so both lines of
  a saved layout go to the same one. Add the port to `ident` (`o.make +
  " " + o.model + " " + o.name`) to tell them apart, at the cost of a
  layout for each way they are plugged in.

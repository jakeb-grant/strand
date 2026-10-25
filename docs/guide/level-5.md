# Level 5: your own theme, services and effects

You learn: `tokens`, spring overrides `~`, `service from dbus`,
`pages`, drag and drop, shaders, `file`; also `set { }`, `keyframes` and
`play`, `canvas`, `type` and `key`, `slot`, `fn`, settings files,
SVG layer selectors, and `service … from file | listen | poll` with
`permit exec`, and `action`.
You build: a theme file, a CSS file of your colours that other programs
can read, services Strand does not ship, a control centre and a dock.

This is the last level. After it you have met every construct in the
language; [`docs/spec.md`](../spec.md) has the full rules for each.

## 1. A theme of your own

Until now your shell has used the built-in theme. Make `theme.strand`:

```strand level=5 file=theme.strand
// theme.strand
enum Look { auto, light, dark, wallpaper, mocha }
export state look = auto persist
state prefs from "~/.config/strand/prefs.toml" {
  accent: color = #7aa2f7; wallpaper: path = "~/Pictures/wall.jpg"; compact: bool = false
}
let dark = match look { auto => system.dark, light => false, _ => true }
let scheme: Palette = match look {
  wallpaper => material(image: prefs.wallpaper, variant: tonal_spot, dark: dark)
               ?? material(seed: prefs.accent, dark: dark),   // while quantising, or if missing
  mocha     => import("catppuccin:mocha"),
  _         => material(seed: system.accent ?? prefs.accent, dark: dark, contrast: system.contrast),
}
tokens base {
  space  { 1: 4px; 2: 8px; 3: 12px; 4: 16px }
  radius { sm: 6px; md: 10px; lg: 14px; xl: 20px; full: 999px }
  font   { ui: "Inter" 13px 500; title: "Inter" 18px 600; caption: "Inter" 11px; mono: "JetBrains Mono" 12px }
  motion { spatial: spring(3000, 0.75); effects: spring(1600, 1); bouncy: spring(380, 0.75) }
  elevation {
    md: 0 2px 8px $shadow.alpha(0.25)
    lg: 0 8px 24px $shadow.alpha(0.3), 0 1px 2px $shadow.alpha(0.2)
    xl: 0 16px 48px $shadow.alpha(0.4)
  }
  surface.hi:       $surface.mix($fg, 8%)
  fg.muted:         $fg.alpha(0.65)
  fg.faint:         $fg.alpha(0.35)
  accent.hover:     $accent.mix($on_accent, 8%)
  accent.container: $accent.alpha(0.22)
  border:           oklch(from $surface, l: l + 0.12)
}
tokens compact extends base { override space { 1: 2px; 2: 4px; 3: 8px; 4: 12px } }
use tokens prefs.compact ? compact : base, palette scheme
file "~/.cache/strand/accent.txt" { text: $accent }   // kept in step with every switch
```

Little changes at first: these are the built-in theme's values, written
out. (If Inter or JetBrains Mono is not installed, put fonts you have in
the `font` line.) Now try:

```
strand set theme.look mocha
strand set theme.look light
strand set theme.look auto
```

Each time, every colour in every surface glides to the new palette,
and nothing reloads: the open launcher keeps its query, the calendar
its month. The value is persisted, so the look you leave it on is the
one you log in to.

**Tokens come in three tiers.**

| Tier | What it holds | Here |
| --- | --- | --- |
| Palette | The colour roles: Material 3's roles (`$surface`, `$fg`, `$accent`, `$on_accent`, `$error`, `$outline`, …) | `use … palette scheme` |
| Base tokens | Scales, and roles derived from the palette | `tokens base { … }` |
| Component tokens | Knobs a component of yours exposes | section 4 |

**A palette is a value.** `material(seed: c, dark: d)` builds the
whole set of roles from one colour, the way Android's Material You
does. `material(image: path)` builds it from a picture: your wallpaper.
`import("catppuccin:mocha")` reads a published scheme (Catppuccin,
base16 and base24, matugen output and W3C design-token files). Every
source fills every role; a role a source lacks is derived.

`material(image: …)` takes a moment, so it is an `Async` (level 4), and
`?? material(seed: …)` is the palette to use until the wallpaper is
read, or if the file is missing. The wallpaper file is watched: replace
it, or point the setting at another, and the theme follows. Until the
new palette is ready the old one stays, so the screen never flashes
default colours.

**Hue roles.** Beside the Material roles, every palette has thirteen
colours for things that must stay recognisably coloured, such as a
green "charged" or a red "recording": `$red`, `$orange`,
`$yellow`, `$green`, `$cyan`, `$blue`, `$magenta`, and `$bright_red`
through `$bright_magenta` (no bright orange). A scheme that has these
hues (base16, Catppuccin) gives its own; a seed or a wallpaper gets
each one nudged toward your accent so they sit well together.

**`tokens base { … }` declares base tokens.** A group
(`space { 1: 4px; … }`) gives `$space.1` to `$space.4`. A token can be
an expression over the palette: `fg.muted: $fg.alpha(0.65)` stays a
softer `$fg` whatever `$fg` becomes, frame by frame while a palette
glides.

**`tokens compact extends base { … }`** starts from `base` and changes
some of it. Changing a token you inherited needs the word `override`,
so a typo is an error ("unknown token") instead of a new token nobody
reads.

**`use tokens …, palette …` picks the set and the palette in force.**
Both are expressions, so the choice is live: `prefs.compact ? compact :
base` switches the spacing when the setting changes.

**`let dark = match look { … }`** turns your `Look` into light or dark:
`auto` follows the desktop (`system.dark`), `light` is light, and `_`,
every other look, is dark. `let scheme: Palette = match look { … }`
picks the palette the same way.

**`state prefs from "~/.config/strand/prefs.toml" { … }` is a settings
file.** Each field is `name: type = default`, read as `prefs.accent`.
It is a plain TOML file you, or anyone, can edit by hand:

```toml
# ~/.config/strand/prefs.toml
accent = "#e06c75"
compact = true
```

Save it and the shell follows, field by field. A field with a bad value
keeps its last good value and says why; the others still apply. A
broken file keeps every last good value. A deleted line goes back to
its default.

It works the other way too. `strand set prefs.compact true` writes the
file, and so would a `<->` binding or a handler assigning
`prefs.compact = true`. Writes keep your comments, spacing and order.

**A theme switcher anywhere** is one line, which the control centre in
section 4 uses: `segmented { options: Look; value: <-> theme.look }`.

**The last line keeps a file in step with the theme.** Open
`~/.cache/strand/accent.txt`: it holds your accent colour, `#rrggbb`,
and every switch of the look rewrites it. Section 2 explains `file`.

**Trying a colour without saving it.** `strand set '$accent' '#ff5555'`
sets a token for the running shell only, over whatever the theme says,
and `strand clear '$accent'` takes it off. (Quote both: a shell reads
`$accent` as a variable and `#` as a comment.) While it is set, it wins
over your files, and when a save changes what it hides, the reload
overlay says so and offers to clear it.

## 2. Your colours for other programs

Strand ships no themes for other programs. It writes your palette in
forms any program, or any of the community's base16/base24 templates,
can use.

Make the template, `colors.css.tmpl`, in your config directory. It is
not a `.strand` file: it is the text to write, with holes.

```css level=5 file=colors.css.tmpl
:root {
  --accent: {{ $accent }};
  --on-accent: {{ $on_accent }};
  --surface: {{ $surface }};
  --fg: {{ $fg }};
  --fg-muted: {{ hex($fg.muted, alpha: true) }};
  --glow: {{ hex($accent.alpha(0.4), alpha: true) }};
}
```

Then make `colors.strand`:

```strand level=5 file=colors.strand
// colors.strand: keeps ~/.cache/strand/colors.css in step with the theme
file "~/.cache/strand/colors.css" { template: "colors.css.tmpl" }
```

Open `~/.cache/strand/colors.css`: each hole is replaced by its value,
a colour written `#rrggbb`. Run `strand set theme.look mocha` and the file is
rewritten with Catppuccin's colours, once, after the switch (never with
the colours halfway through it). A stylesheet, a web dashboard or a
script can read these custom properties.

**A hole is `{{ expression }}`**, any expression over your tokens. A
colour on its own is written `#rrggbb`. `hex(c, alpha: true)` writes
`#rrggbbaa`, which `$fg.muted` needs since it is translucent; a
translucent colour without `alpha: true` is an error, never quietly
made opaque. `argb(c)` writes `0xaarrggbb`. A literal `{{` is
`{{ "{{" }}`. An error names the template's line and column, and
nothing is written until it is fixed.

Templates can read a little more than props can:

- `$surface.light()` and `$surface.dark()` are a token in the other
  mode, so one file can carry both: with `look` on `auto` and a dark
  desktop, `{{ $surface.light() }}` is the surface you would have in
  light mode. When the theme gives the same mode either way (`mocha` is
  always dark), there is no other mode, and the hole needs a fallback:
  `{{ $surface.light() ?? $surface }}`.
- `.red()`, `.green()` and `.blue()` (0 to 255) and `.opacity()` (0 to
  1) take a colour apart, for formats that want numbers.
- `$palette.name` is the scheme's name, and `$palette.mauve`,
  `$palette.base0d` and so on are the source's own colours by their own
  names. They work in props too: a config ported from a Catppuccin
  shell can say `bg: $palette.mantle ?? $surface`, the fallback for a
  palette that has no `mantle`.

**`file "path" { … }`** keeps one file in step. It may stand at the top
level of any `.strand` file, as here and in `theme.strand`, since it
draws nothing. Two `file`s may not write the same path. It writes
only when the text would change, atomically, and catches up when the
shell starts. It never overwrites a file it did not write: if
`colors.css` already exists, Strand asks first. Removing the `file`
stops the updates and leaves the file where it is. Editing
`colors.css.tmpl` rewrites it at once.

For one line, `text:` replaces the template file, as in the last line
of `theme.strand`: `file "~/.cache/strand/accent.txt" { text: $accent }`.

To tell a program its colours changed, add `run:`, a command run 200 ms
after the file is written. Running a program needs your permission,
`permit exec` (section 3):

```
permit exec "notify-send"
file "~/.cache/strand/colors.css" { template: "colors.css.tmpl"; run: ["notify-send", "Colours updated"] }
```

**`strand export`** writes the same things once, to standard output or,
atomically, to a file with `-o`:

```
strand export json                                  # the palette as data
strand export base24 -o ~/.cache/strand/base24.yaml # for base16/base24 templates
strand export --template ~/.config/strand/colors.css.tmpl  # any template of yours
```

It asks the running shell; with none running it works the theme out
from your config, and says "run strand once, or fix the theme" if it
cannot.

## 3. Services of your own

Strand's services cover the usual things: audio, battery, network,
notifications and so on. For the rest, you declare one. Make
`services.strand`:

```strand level=5 file=services.strand
// services.strand: services Strand does not ship
service ppd from dbus system "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }
service host from file "/proc/sys/kernel/hostname" { name: text }
service temps from poll ["sensors", "-j", "coretemp-*"] every 5s { cpu: float = temp1_input }
permit exec "sensors"
```

Nothing shows yet; the control centre in the next section reads all
three. A service starts when something first reads it and stops a few
seconds after the last reader goes, as the built-in ones do.

**`from dbus`** reads a D-Bus object. `ppd` is the power profiles
daemon on the system bus, and its `profile` field is the daemon's
`ActiveProfile` property. `rw` marks it writable: assigning
`ppd.profile = "power-saver"` in a handler (or binding it with `<->`)
sets the property, which the control centre below does when you click a
profile. Strand checks every field against the object's
introspection, so a misspelt property is an error with a
did-you-mean. If the daemon is not running, that is a
warning: your config is not broken because a daemon is down.

**`from file`** reads a file and watches it. The kernel keeps the name
your computer runs under in `/proc/sys/kernel/hostname`: one word, on
every Linux system. (`/etc/hostname` is not as dependable: some systems
leave it out, and it may hold `#` comment lines.) A file that is neither
JSON nor `key=value` lines is read whole, whatever the key: `host.name`
is that word. Files under `/proc` change without saying so, so after you
rename the machine `host.name` shows the new name once the service
starts again: when Strand restarts, or after nothing has read it for a
few seconds. A JSON file is read by key. So is a file of `key=value` (or
`key: value`) lines such as `/etc/os-release`, with blank lines and `#`
comment lines skipped and the double or single quotes around a value
removed, but only when every other line is a pair: one line that is
neither makes the whole file text again, and every field reads all of
it. A field without `= key` reads the key with its own name.

**`from poll … every T`** reads again every `T`, and only while
something showing reads it, or while a `graph` keeps history of it:
then, hidden, it reads at the coarser of `T` and a twentieth of the
graph's `history` (at least 2 s), and the graph draws each hidden read
as a line from the one before. Given a file path (`"/sys/class/…"`), it
reads the file. Given a command, it runs it: `sensors -j` prints JSON,
and `= temp1_input` finds the first `temp1_input` in it, however deep.
Values are converted to the declared type.

`sensors` alone lists every chip it knows (the ACPI thermal zone, the
graphics card, the disks), each with its own `temp1_input`, and the
first one found is not always the CPU's. `"coretemp-*"` asks for
Intel's CPU sensor only, whose `temp1_input` is the whole package. On
an AMD processor write `"k10temp-*"` instead (its `temp1_input` is
`Tctl`). Run `sensors` in a terminal to see the chip names your
machine has. With no chip of that name, `sensors` fails every 5 s and
`temps.cpu` stays `0`. Nothing marks the service failed and `strand
watch` shows nothing; the only trace is a line in the terminal running
`strand run` each time, ``strand: WARN strand_services::custom: `sensors`
failed (exit status: 1)``.

**`permit exec` lists the programs your config may run.** A service
that runs a command (`from poll` of a command, or `from listen`) is an
error without it, so a shared config can never run a program you did
not allow. `permit exec` with no list allows any.

**`from listen`** runs a command and reads each line it prints as an
update, for programs that report changes as they happen:
`service player from listen ["playerctl", "--follow", "status"] { status: text }`,
with `permit exec "playerctl"`, has `player.status` follow your media
player as it plays and pauses.

**`action` runs a command from a handler.** For what no service does
(opening an app of your own, a compositor's dispatch, a tool with only a
command line), declare the command once at the top level, its
parameters in it, and call it like a function:
`action dispatch(ws: int) ["hyprctl", "dispatch", "workspace", ws]`,
with `permit exec "hyprctl"`, then `on click { dispatch(3) }`. Each
parameter is exactly one argument of the program, as it is: a title
with spaces or quotes is never split, and no shell stands between
strand and the program. (The program reads the argument its own way:
`["sh", "-c", cmd]` runs `cmd` as a shell script, and `strand check`
warns about that; a value starting with `-` may look like an option.) The
program must be written out and allowed by `permit exec`, so every
command your config can run is listed where it is declared: `strand
check` lists them all with their parameters. The command runs on its
own, detached; if it cannot start or exits with an error, the terminal
running `strand run` and `strand watch` say so. A `from dbus` service
can declare actions too, calling a method of its object:
`action hold(profile: text, reason: text, app: text) = HoldProfile`
in `ppd`'s block is `ppd.hold("performance", "gaming", "strand")` in a
handler, checked against the object's introspection as its fields are.

For a service none of these can describe, write it in Rust (section 7).

## 4. A control centre

Make `control.strand`:

```strand level=5 file=control.strand
// control.strand. Bind a key to: strand toggle control.open
export state open = false
enum View { main, devices }

fn degrees(c: float) -> text { join("", c.round(), "°") }

component Tile(label: text, active: bool = false) tokens { radius: $radius.lg; pad: $space.3 } {
  col { grow: 1; gap: $space.2; pad: $Tile.pad; radius: $Tile.radius; bg: $surface.hi
    when active { bg: $accent.container }
    text label { font: $font.caption; color: $fg.muted }
    slot                                           // the caller's children go here
  }
}

panel Control {
  screens: focused; layer: overlay; anchor: top_right; margin: $space.2; keyboard: on_demand
  open: <-> open
  enter { opacity: 0; x: 24 }
  state view = main
  on show { view = main }
  col { width: 360; pad: $space.3; gap: $space.3; radius: $radius.xl
        bg: $surface.alpha(0.86); blur: 32; shadow: $elevation.xl
    set { $surface: $surface.mix($accent, 4%) }    // a tinted surface for this panel
    row { align: center; gap: $space.2
      text host.name { grow: 1; weight: 600; ellipsis: end }
      text degrees(temps.cpu) { font: $font.mono; color: $fg.muted }
    }
    pages current: view {
      page main {
        col { gap: $space.2
          Tile "Appearance" { segmented { options: Look; value: <-> theme.look } }
          row { gap: $space.2
            set { $Tile.pad: $space.2 }
            Tile "Volume" { slider { value: <-> audio.sink.volume } }
            if brightness.available { Tile "Brightness" { slider { value: <-> brightness.level } } }
          }
          Tile "Power profile" {
            row { gap: $space.1
              for p in ["power-saver", "balanced", "performance"] key p {
                text p { grow: 1; font: $font.caption; color: $fg.muted
                  when ppd.profile == p { color: $accent; weight: 600 }
                  on click { ppd.profile = p }
                }
              }
            }
          }
          Tile "Bluetooth" { active: bluetooth.powered
            row { gap: $space.1
              button bluetooth.powered ? "On" : "Off" { grow: 1; on click { bluetooth.powered = !bluetooth.powered } }
              button "Devices ›" { grow: 1; on click { view = devices } }
            }
          }
        }
      }
      page devices {
        col { gap: $space.1
          row { align: center; gap: $space.2
            button "‹" { on click { view = main } }
            text "Bluetooth devices" { grow: 1; weight: 600 }
          }
          for d in bluetooth.devices {
            row { pad: $space.2; gap: $space.2; radius: $radius.md; align: center
              when hover { bg: $surface.hi }
              on click { if d.connected { d.disconnect() } else { d.connect() } }
              icon d.icon { size: 20 }
              text d.name { grow: 1; ellipsis: end }
              if d.connected { text "Connected" { font: $font.caption; color: $accent } }
            }
          }
        }
      }
    }
  }
}
```

Bind a key to `strand toggle control.open` and press it. A panel slides
in at the top right: your system's name and CPU temperature, then tiles
for the look, volume, brightness (on a laptop), the power profile in
use (click one to switch) and Bluetooth. Pick "mocha" and the panel itself changes colour as
you watch. "Devices ›" slides to a second page that lists your Bluetooth
devices; "‹" slides back.

**`pages current: view { page main { … }; page devices { … } }`** shows
one page at a time: the `page` whose name is `view`'s value. Changing
`view` slides from one to the other. A page that is not shown is taken
down, so what it reads stops: the device list is not followed while you
are on the main page. Page names are values of `current`'s type, here
the `View` enum's variants.

**`slot` is where a component puts the children you give it.**
`Tile "Volume" { slider { … } }` draws the tile's box and label, and the
slider where `Tile` wrote `slot`. Every tile looks the same; what is in
it is up to the caller. A component without `slot` takes no children.

**`fn degrees(c: float) -> text { … }` is a function of your own.** Its
body's last line is its value. A `fn` is pure: it computes a value
from its arguments and cannot assign, call an action or `await`, so
calling it anywhere is safe. The builtin functions it can use are listed
in [the function reference](../reference/functions.md): besides `pct`
and `dur`, `num(x, digits: 1)` writes a number with fixed decimals,
`bytes(n)` a size (`1.5 GiB`), `number(t)` reads a number from text
(`null` when it is not one), `sin`, `cos`, `atan2`, `sqrt` and `pi` do
geometry (angles in degrees, clockwise from the right), and a list
folds with `xs.reduce(0, (sum, x) => sum + x)` and grows with
`xs + [x]`.

**`component Tile(…) tokens { radius: …; pad: … }` gives the
component tokens of its own,** read inside as `$Tile.radius` and
`$Tile.pad`. They are the knobs someone using `Tile` can turn without
editing it.

**`set { … }` overrides tokens for a node and everything inside it.**

- `set { $surface: $surface.mix($accent, 4%) }` on the panel's column
  gives the whole panel a surface tinted with the accent. On the right,
  `$surface` is the value from outside, so this is not a loop. Tokens
  derived from `$surface` derive again inside: every tile's
  `$surface.hi` is a step up from the tinted surface, not the normal
  one.
- `set { $Tile.pad: $space.2 }` on the row of sliders makes the two
  tiles in it tighter. The other tiles keep `$space.3`.

**`for p in [...] key p`** loops over a list you wrote. Service items
(workspaces, devices) carry their own keys; your own data needs one,
and `key p` says each name is its own key. Strand uses keys to tell
items apart when the list changes, so it insists.

`segmented { options: Look; value: <-> theme.look }` reads `Look`, the
enum from `theme.strand`, and `theme.look`, its exported state. An
`enum` is shared by every file; a `state` is shared only when it is
exported.

## 5. A dock

Make `dock.strand`:

```strand level=5 file=dock.strand
// dock.strand: a dock on every monitor. Drag icons to reorder; right-click to unpin
type Pin { app: AppId; label: text }
state pins: [Pin] key app = [
  Pin(app: "firefox", label: "Firefox"),
  Pin(app: "org.gnome.Nautilus", label: "Files"),
] persist

keyframes hop { 0%, 100% { y: 0 }; 40% { y: -10 }; duration: 450ms }

bar Dock {
  edge: bottom; height: 64; margin: 0, $space.2, $space.2
  bg: $surface.alpha(0.72); blur: 24; radius: $radius.xl; shadow: $elevation.md
  row { grow: 1; justify: center; align: center; gap: $space.2
    on drop(p: Pin, at: int) { pins.move(p.app, at) }
    for p in pins key p.app {
      let app = apps.all.find(a => a.id == p.app)   // the installed app, or null
      image app?.icon ?? "application-x-executable" { size: 44; drag: p; tooltip: p.label
        enter { scale: 0; opacity: 0 }
        when app == null { opacity: 0.4 }
        when hover { scale: 1.15 ~ $motion.bouncy }
        on click     { app?.launch(); play hop }
        on secondary { pins.remove_key(p.app) }
      }
    }
  }
}
```

A dock appears at the bottom of every monitor. Point at an icon and it
grows with a bounce; click it and it hops while the app starts. Drag an
icon along the dock and the others glide out of its way. Right-click
one to unpin it. Your order is kept across restarts.

An app id is its desktop file's name without `.desktop`
(`org.gnome.Nautilus`). A pin whose app is not installed shows a faded
generic icon, so you can still drag it or right-click it away.

**`type Pin { app: AppId; label: text }` declares a record**, a value
with named fields. Build one with its name and named arguments:
`Pin(app: "firefox", label: "Firefox")`.

**`state pins: [Pin] key app = […]` is a keyed list.** `[Pin]` is a
list of pins, and `key app` says each pin is known by its `app`. Change
it with `pins.push(p)`, `pins.insert(i, p)`, `pins.remove_key(k)`,
`pins.move(k, i)` and `pins.update(k, …)`. Because each item is known by
its key, an icon keeps its place, its animation and anything it holds
when the order changes, and glides to its new spot. `persist` keeps the
list across restarts.

**Drag and drop.** `drag: p` makes the icon draggable, carrying its
pin. `on drop(p: Pin, at: int)` on the row receives it with the index
where it was dropped, and `pins.move` puts it there. Things dragged
from other programs (files, apps, text) arrive as a `Drop` value:
`on drop(d: Drop, at: int)`.

**`~` picks a node's animation for one prop.** Every prop springs with
the theme's motion; `scale: 1.15 ~ $motion.bouncy` uses the bouncy
spring instead, while this `when` applies. `~ 200ms` animates over a
fixed time, `~ ease(out_back, 300ms)` with an easing curve, and
`~ instant` not at all.

**`keyframes hop { … }` is a timed animation**: at 40% of its 450 ms
the icon is 10 px up, at 0% and 100% where it belongs. `play hop` in a
handler plays it once on the node the handler is on.

**`let app = apps.all.find(a => a.id == p.app)`** finds the installed
app with the pin's id, or `null` when it is not installed. You need the
app itself, not its id, to call `launch()`. `app?.icon` is `null` when
`app` is, and `?? "application-x-executable"` puts the generic icon in
its place; `app?.launch()` does nothing when `app` is `null`.

Every pin draws exactly one icon, the missing ones included, and that
matters for drag and drop: the `at` a drop reports counts the icons
drawn in the row, leaving out the one you are dragging, and `pins.move`
counts the pins, taking the moved one out first. Were a missing pin to
draw nothing, the two would disagree and a dragged icon would land one
place off.

With more than one monitor there is a dock on each, and you can drag
an icon from one to another. The other dock draws the same pin too, and
its `at` leaves that icon out as well, so the pin lands where you let
it go on either dock.

## 6. Escape hatches

Some things are easier to draw than to describe. Each of these needs a
file of your own beside the config, so this section shows them without
steps.

- **Shaders.** `shader "aurora.wgsl" { u_speed: 0.4; u_tint: $accent }`
  runs your own WGSL fragment shader; props named `u_…` are its
  uniforms, and may be any expression, tokens included. It is checked
  when the config loads and reloads when you save the `.wgsl` file. It
  runs on the GPU, which starts only while it is visible; without a
  usable GPU it draws nothing, and `strand report` says why (a `GPU:`
  warning in the log says it too).
- **Canvas.** `canvas { draw: (c) => … }` draws with code: `c.line`,
  `c.rect`, `c.circle` and `c.text`, painted with `c.fill` and
  `c.stroke`, sized by `c.width` and `c.height`. Use it for charts and
  visualisers the built-in `graph`, `arc` and `spectrum` do not cover.
  Shapes collect into one path, and `c.fill` or `c.stroke` paints the
  shapes since the last paint, then starts a new path: to fill and
  stroke a shape, draw it again before the stroke. A shape with no paint
  after it draws nothing, `c.fill` before any shape paints the whole
  canvas, and later paints cover earlier ones. For shapes of your own,
  draw a path point by point: `c.move(x, y)` starts it, `c.to(x, y)`
  draws a line on, `c.arc(x, y, r, from: a, to: b)` an arc of a circle
  (angles clockwise from the right, as `rotate` turns), and `c.close()`
  joins it back to its start. `c.move(20, 20); c.arc(20, 20, 16, from:
  -90deg, to: -90deg + share * 360deg); c.close(); c.fill($accent)` is a
  pie slice. `c.fill(paint, evenodd: true)` lets a shape inside another
  cut a hole: two circles make a ring. `draw` runs again when what it
  reads changes, not every frame, so it cannot read `t` or `wave(…)`;
  to animate a canvas, animate its own props (`rotate: t * 90deg`), or
  use a shader.
- **SVG layers.** `svg "gauge.svg" { #needle { rotate: level * 270deg } }`
  draws an SVG and binds props to the element with `id="needle"` inside
  it: `rotate`, `scale`, `x`, `y`, `opacity` and `fill`. Draw a gauge
  in an editor, name the needle, and it turns with your data. Its path,
  like an `image`'s, is relative to your config directory:
  `svg "icons/gauge.svg"` is in the `icons` folder beside your files.
- **Your own icons.** Put SVG or PNG files in an `icons/` folder beside
  your config and name them as an icon theme would: `icon "wifi"` draws
  `icons/wifi.svg` (or `wifi-symbolic.svg`, or a `.png`), and on a
  machine without the file it falls back to the icon theme. A file
  whose name ends in `-symbolic` is drawn in `color`, like a theme's
  symbolic icon; any other is drawn as you made it. Every icon name is
  looked up there first, so `icons/firefox.svg` also replaces Firefox's
  icon wherever an app's or tray item's icon is shown. Adding, editing
  or removing a file there redraws the icons at once.

## 7. Plugins

When no `from dbus`, `from file`, `from poll` or `from listen` service
can describe what you need, a service can be written in Rust and built
into your own `strand` binary. [The plugin page](plugins.md) shows how.

## What you have

| File | Holds |
| --- | --- |
| `theme.strand` | `Look`, the settings, your token sets and `accent.txt` |
| `colors.strand` and `colors.css.tmpl` | your colours as CSS custom properties |
| `services.strand` | `ppd`, `host` and `temps` |
| `control.strand` | the `Control` panel and `Tile` |
| `dock.strand` | `Pin`, the pins and the `Dock` bar |

Together with levels 1 to 4: a bar on every monitor, an OSD,
notifications, a launcher, a lock screen, a control centre and a dock,
all in one language, themed from one file, and live on every save.
